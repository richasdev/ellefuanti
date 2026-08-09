//! Reading and writing files. Blocking; call from a background thread.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result, bail};

/// A file loaded from disk.
#[derive(Debug)]
pub struct ReadFile {
    pub text: String,
    /// True when the file had a final newline. Preserved on save so the editor does not
    /// silently add or strip one and produce a spurious git diff.
    pub trailing_newline: bool,
}

/// Files above this size are refused rather than loaded.
///
/// ponytail: a hard limit, not a streaming/virtualised large-file mode. Refusing with a
/// clear message beats freezing the UI on a 2 GB log. Raise it — or implement
/// memory-mapped viewing — when someone actually needs to open such a file.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Reads a UTF-8 text file.
///
/// Rejects binary files (NUL byte in the first 8 KiB, the same heuristic git uses) so
/// the editor never renders garbage or corrupts a binary by saving it back as text.
pub fn read_file(path: &Path) -> Result<ReadFile> {
    let meta =
        fs::metadata(path).with_context(|| format!("reading metadata of {}", path.display()))?;
    if meta.is_dir() {
        bail!("{} is a directory", path.display());
    }
    if meta.len() > MAX_FILE_BYTES {
        bail!(
            "{} is {:.1} MB; the limit is {} MB",
            path.display(),
            meta.len() as f64 / 1_048_576.0,
            MAX_FILE_BYTES / 1_048_576
        );
    }

    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        bail!("{} looks like a binary file", path.display());
    }

    let mut text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;

    // Strip a UTF-8 BOM so it does not show up as a stray glyph at 0,0. It is not
    // written back — a BOM in a PHP file breaks output, and Laravel files never carry one.
    if text.starts_with('\u{feff}') {
        text.remove(0);
    }

    let trailing_newline = text.ends_with('\n');
    Ok(ReadFile { text, trailing_newline })
}

/// Writes a file by writing a sibling temp file and renaming over the target.
///
/// Not simplified to a plain `fs::write`: that truncates first, so a crash or full disk
/// mid-write leaves the user's source truncated. Rename is atomic within a filesystem,
/// so the file on disk is always either the old version or the complete new one.
pub fn write_file(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} has no file name", path.display()))?;

    // Same directory as the target: a temp dir could be another filesystem, where
    // rename is not atomic (and may fail outright).
    let temp_path = dir.join(format!(".{file_name}.elle-tmp"));

    let write_result = (|| -> Result<()> {
        let mut file = fs::File::create(&temp_path)
            .with_context(|| format!("creating {}", temp_path.display()))?;
        file.write_all(text.as_bytes())?;
        // Durability before the rename: otherwise a crash can leave the renamed file
        // present but empty.
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    // Preserve the original file's permissions; File::create would otherwise reset an
    // executable script to 0644.
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&temp_path, meta.permissions());
    }

    fs::rename(&temp_path, path).map_err(|err| {
        let _ = fs::remove_file(&temp_path);
        anyhow::Error::new(err).context(format!("saving {}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn reads_utf8_and_notes_trailing_newline() {
        let dir = tmp();
        let path = dir.path().join("a.php");
        stdfs::write(&path, "<?php // ação\n").unwrap();
        let f = read_file(&path).unwrap();
        assert_eq!(f.text, "<?php // ação\n");
        assert!(f.trailing_newline);

        stdfs::write(&path, "no newline").unwrap();
        assert!(!read_file(&path).unwrap().trailing_newline);
    }

    #[test]
    fn strips_bom() {
        let dir = tmp();
        let path = dir.path().join("bom.php");
        stdfs::write(&path, "\u{feff}<?php".as_bytes()).unwrap();
        assert_eq!(read_file(&path).unwrap().text, "<?php");
    }

    #[test]
    fn rejects_binary_and_directories() {
        let dir = tmp();
        let bin = dir.path().join("x.bin");
        stdfs::write(&bin, [0x00, 0x01, 0x02]).unwrap();
        assert!(read_file(&bin).unwrap_err().to_string().contains("binary"));
        assert!(read_file(dir.path()).unwrap_err().to_string().contains("directory"));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let dir = tmp();
        let path = dir.path().join("bad.txt");
        stdfs::write(&path, [0xff, 0xfe, 0x41]).unwrap();
        assert!(read_file(&path).unwrap_err().to_string().contains("UTF-8"));
    }

    #[test]
    fn write_then_read_round_trips_and_leaves_no_temp_file() {
        let dir = tmp();
        let path = dir.path().join("out.php");
        write_file(&path, "<?php\n$x = 1;\n").unwrap();
        assert_eq!(read_file(&path).unwrap().text, "<?php\n$x = 1;\n");

        let leftovers: Vec<_> = stdfs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("elle-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn overwrite_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tmp();
        let path = dir.path().join("script.sh");
        stdfs::write(&path, "#!/bin/sh\n").unwrap();
        stdfs::set_permissions(&path, stdfs::Permissions::from_mode(0o755)).unwrap();

        write_file(&path, "#!/bin/sh\necho hi\n").unwrap();
        let mode = stdfs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "save must not strip the executable bit");
    }

    #[test]
    fn write_failure_does_not_destroy_the_original() {
        let dir = tmp();
        let path = dir.path().join("sub").join("nested.php");
        // Parent does not exist, so the temp create fails.
        assert!(write_file(&path, "x").is_err());
        assert!(!path.exists());
    }
}
