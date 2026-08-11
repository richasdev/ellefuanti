//! Reading and writing files. Blocking; call from a background thread.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

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

/// Characters a new or renamed name may not contain, and why each is here.
///
/// `/` is a path separator, so accepting it would let "rename" quietly move a file to
/// another directory — or, with a leading one, to an absolute path outside the project.
/// NUL cannot be represented in a path at all and produces an OS error whose message says
/// nothing a user could act on.
const FORBIDDEN_IN_NAME: [char; 2] = ['/', '\0'];

/// Checks a user-typed file name before it is used to build a path.
///
/// # Why this is a validation and not a sanitisation
///
/// The tempting alternative is to strip the bad characters and carry on. That silently
/// creates a file the user did not ask for under a name they did not choose, and the
/// difference only surfaces later when they cannot find it. A name that cannot be honoured
/// is refused with the reason, which is the same rule the rest of this crate follows:
/// never a positive claim the operation cannot support.
///
/// `.` and `..` are refused because they name directories that already exist, so every
/// operation built on them means something other than what it appears to — "create `..`"
/// is not a creation, and "rename to `.`" is not a rename.
pub fn validate_file_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("a name is required");
    }
    if let Some(bad) = trimmed.chars().find(|c| FORBIDDEN_IN_NAME.contains(c)) {
        if bad == '/' {
            bail!("a name cannot contain a slash");
        }
        bail!("a name cannot contain that character");
    }
    if trimmed == "." || trimmed == ".." {
        bail!("`{trimmed}` is not a usable name");
    }
    Ok(())
}

/// Creates an empty file at `path`.
///
/// Fails rather than truncating when something is already there. `create_new` makes that
/// check and the creation one atomic step: testing `path.exists()` first and creating
/// after leaves a window in which another process — or the user's own second click — puts
/// a file there, and the loser of that race silently destroys the winner's content.
pub fn create_file(path: &Path) -> Result<()> {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        validate_file_name(name)?;
    }
    fs::File::create_new(path).with_context(|| format!("creating {}", path.display())).map(|_| ())
}

/// Creates a directory at `path`, and any missing parents.
pub fn create_directory(path: &Path) -> Result<()> {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        validate_file_name(name)?;
    }
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))
}

/// Renames `from` to a sibling called `new_name`.
///
/// Takes a name rather than a destination path: every caller is a rename *in place*, and
/// accepting a full path would make "rename" and "move" the same function with no way for
/// the UI to tell the user which one it just did.
///
/// Refuses to overwrite an existing entry. `fs::rename` on Unix replaces the destination
/// silently, which for a rename typed into a text field means one keystroke can destroy an
/// unrelated file with no warning and no undo.
pub fn rename(from: &Path, new_name: &str) -> Result<PathBuf> {
    validate_file_name(new_name)?;
    let new_name = new_name.trim();

    let parent =
        from.parent().with_context(|| format!("{} has no parent directory", from.display()))?;
    let to = parent.join(new_name);

    if to == from {
        return Ok(to);
    }
    // Not `to.exists()` alone: on a case-insensitive filesystem — which macOS is by
    // default — renaming `User.php` to `user.php` reports the destination as existing
    // because it *is* the source. Refusing that would make a case-only rename impossible
    // on the platform this editor targets.
    if to.exists() && !same_file(from, &to) {
        bail!("{} already exists", to.display());
    }

    fs::rename(from, &to)
        .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
    Ok(to)
}

/// Whether two paths refer to the same file on disk.
///
/// Compares device and inode rather than the paths themselves, which is the only way to
/// answer the case-insensitive-filesystem question above.
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
}

/// Deletes a file, or a directory and everything inside it.
///
/// # Why this is the most dangerous function in the crate
///
/// It is the one action in the editor a user cannot undo. There is no trash, no journal
/// and no confirmation *here* — confirming is the caller's job, because only the caller
/// knows whether a human was asked. What this function does provide is the guarantee that
/// it cannot be aimed outside the project: `root` is passed in and the target must be
/// underneath it.
///
/// That check is not paranoia about the UI. The tree holds paths that were correct when it
/// was read, and a directory replaced by a symlink between the read and the click is enough
/// to point a recursive delete at somewhere else entirely. Refusing anything not under the
/// root turns that from data loss into an error message.
///
/// A symlink is removed as a link, never followed — `symlink_metadata` is what makes that
/// distinction, and using `metadata` here would delete the *contents* of whatever a link in
/// the project happened to point at.
pub fn delete(path: &Path, root: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("reading metadata of {}", path.display()))?;

    // Canonicalise the parent, not the target: canonicalising the target would follow a
    // symlink and compare the wrong thing, and the parent is what has to be inside the
    // project for the deletion to be legitimate.
    let parent = path.parent().unwrap_or(root);
    let real_parent =
        parent.canonicalize().with_context(|| format!("resolving {}", parent.display()))?;
    let real_root = root.canonicalize().with_context(|| format!("resolving {}", root.display()))?;

    if !real_parent.starts_with(&real_root) {
        bail!("{} is outside the open folder", path.display());
    }
    if real_parent.join(path.file_name().unwrap_or_default()) == real_root {
        bail!("the open folder cannot be deleted from inside itself");
    }

    if meta.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("deleting {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))
    }
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

    // --- names (#126) -------------------------------------------------------------

    #[test]
    fn a_name_with_a_slash_is_refused_rather_than_stripped() {
        // Stripping would turn "rename to a/b" into a file called `ab` that the user never
        // asked for and cannot find. A leading slash is worse: it escapes the project.
        assert!(validate_file_name("a/b").is_err());
        assert!(validate_file_name("/etc/passwd").is_err());
    }

    #[test]
    fn empty_and_relative_names_are_refused() {
        assert!(validate_file_name("").is_err());
        assert!(validate_file_name("   ").is_err());
        // These name directories that already exist, so every operation on them means
        // something other than what it looks like.
        assert!(validate_file_name(".").is_err());
        assert!(validate_file_name("..").is_err());
    }

    #[test]
    fn ordinary_names_are_allowed_including_dotfiles() {
        // A dotfile is a normal thing to create in a project, and `.env` is the one a
        // Laravel developer reaches for most.
        for name in [".env", "User.php", "a b.txt", "2024-01-01.log", "ação.php"] {
            assert!(validate_file_name(name).is_ok(), "{name} must be allowed");
        }
    }

    // --- create -------------------------------------------------------------------

    #[test]
    fn creating_a_file_that_exists_does_not_truncate_it() {
        // The whole reason `create_new` is used. A plain `File::create` here would empty
        // the user's file on a mis-click, which is data loss with no undo.
        let dir = tmp();
        let path = dir.path().join("User.php");
        stdfs::write(&path, "<?php // important\n").unwrap();

        assert!(create_file(&path).is_err());
        assert_eq!(stdfs::read_to_string(&path).unwrap(), "<?php // important\n");
    }

    #[test]
    fn creating_makes_an_empty_file_and_a_directory() {
        let dir = tmp();
        let file = dir.path().join("New.php");
        create_file(&file).unwrap();
        assert_eq!(stdfs::read_to_string(&file).unwrap(), "");

        let nested = dir.path().join("app/Models");
        create_directory(&nested).unwrap();
        assert!(nested.is_dir());
    }

    // --- rename -------------------------------------------------------------------

    #[test]
    fn renaming_moves_within_the_same_directory() {
        let dir = tmp();
        let from = dir.path().join("Old.php");
        stdfs::write(&from, "<?php\n").unwrap();

        let to = rename(&from, "New.php").unwrap();
        assert_eq!(to, dir.path().join("New.php"));
        assert!(!from.exists());
        assert_eq!(stdfs::read_to_string(&to).unwrap(), "<?php\n");
    }

    #[test]
    fn renaming_onto_an_existing_file_is_refused() {
        // `fs::rename` replaces the destination silently on Unix, so without this check one
        // typo in a text field destroys an unrelated file with no warning and no undo.
        let dir = tmp();
        let from = dir.path().join("Old.php");
        let occupied = dir.path().join("Taken.php");
        stdfs::write(&from, "source").unwrap();
        stdfs::write(&occupied, "must survive").unwrap();

        assert!(rename(&from, "Taken.php").is_err());
        assert_eq!(stdfs::read_to_string(&occupied).unwrap(), "must survive");
        assert!(from.exists(), "a refused rename must leave the source alone");
    }

    #[test]
    fn a_case_only_rename_works_on_a_case_insensitive_filesystem() {
        // macOS is case-insensitive by default, so `to.exists()` is true for the source
        // itself and a naive existence check makes `User.php` -> `user.php` impossible.
        // That is a rename PHP developers do constantly, PSR-4 being what it is.
        let dir = tmp();
        let from = dir.path().join("User.php");
        stdfs::write(&from, "<?php\n").unwrap();

        let to = rename(&from, "user.php").expect("a case-only rename must be allowed");
        assert_eq!(to.file_name().unwrap(), "user.php");
        assert_eq!(stdfs::read_to_string(&to).unwrap(), "<?php\n");
    }

    #[test]
    fn a_rename_to_a_path_is_refused() {
        let dir = tmp();
        let from = dir.path().join("Old.php");
        stdfs::write(&from, "x").unwrap();

        assert!(rename(&from, "../escaped.php").is_err());
        assert!(rename(&from, "sub/New.php").is_err());
        assert!(from.exists());
    }

    // --- delete, the one that cannot be undone -------------------------------------

    #[test]
    fn deleting_removes_a_file_and_a_whole_directory() {
        let dir = tmp();
        let file = dir.path().join("a.php");
        stdfs::write(&file, "x").unwrap();
        delete(&file, dir.path()).unwrap();
        assert!(!file.exists());

        let nested = dir.path().join("app/Models");
        stdfs::create_dir_all(&nested).unwrap();
        stdfs::write(nested.join("User.php"), "x").unwrap();
        delete(&dir.path().join("app"), dir.path()).unwrap();
        assert!(!dir.path().join("app").exists());
    }

    #[test]
    fn deleting_outside_the_open_folder_is_refused() {
        // The tree holds paths that were true when it was read. This is what stops a stale
        // or manipulated path from aiming a recursive delete at someone's home directory.
        let root = tmp();
        let outside = tmp();
        let victim = outside.path().join("precious.php");
        stdfs::write(&victim, "must survive").unwrap();

        assert!(delete(&victim, root.path()).is_err());
        assert_eq!(stdfs::read_to_string(&victim).unwrap(), "must survive");
    }

    #[test]
    fn deleting_the_open_folder_itself_is_refused() {
        let root = tmp();
        stdfs::write(root.path().join("a.php"), "x").unwrap();

        assert!(delete(root.path(), root.path()).is_err());
        assert!(root.path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_symlink_removes_the_link_and_not_the_target() {
        // A link inside the project can point anywhere, including outside it. Deleting one
        // must unlink it and leave what it points at alone.
        //
        // # What this test does and does not pin
        //
        // It is a **regression guard, not a proof of the guard above it**, and that is
        // worth stating because the obvious reading is wrong. Rewriting `delete` to use
        // `metadata` instead of `symlink_metadata` does not fail this test: a link to a
        // directory then takes the `remove_dir_all` branch, and current Rust's
        // `remove_dir_all` refuses to recurse through a symlink — it unlinks and returns
        // `Ok`. Both spellings unlink the link and both leave the target intact, so the
        // two are observably identical here and no assertion can separate them. Checked by
        // running it, not assumed.
        //
        // `symlink_metadata` stays because the correctness of `delete` should not rest on
        // an implementation detail of `remove_dir_all` that std is free to change, and
        // because reading through a link to decide what a path *is* is wrong on its own
        // terms. What this test pins is the observable contract: the link goes, the target
        // stays. If std ever does start following links, this fails — which is exactly when
        // someone needs to know.
        let root = tmp();
        let outside = tmp();
        let target = outside.path().join("real");
        stdfs::create_dir(&target).unwrap();
        stdfs::write(target.join("precious.php"), "must survive").unwrap();

        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        delete(&link, root.path()).unwrap();
        assert!(link.symlink_metadata().is_err(), "the link itself must be gone");
        assert!(
            target.join("precious.php").exists(),
            "a delete must never follow a link out of the project"
        );
    }
}
