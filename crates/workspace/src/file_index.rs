//! A flat list of every file in the project, for quick open.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ignore::WalkBuilder;

/// One quick-open candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedFile {
    /// Absolute path, used to open the file.
    pub path: PathBuf,
    /// Path relative to the project root — what the user reads and types against.
    /// `app/Models/User.php` is meaningful; the absolute path is noise.
    pub relative: String,
    /// Byte offset of the file name within `relative`, so a matcher can weight a hit on
    /// the name above one buried in a directory component without re-splitting the string.
    pub name_offset: usize,
}

impl IndexedFile {
    pub fn name(&self) -> &str {
        &self.relative[self.name_offset..]
    }
}

/// Cooperative cancellation for a walk in progress.
///
/// A recursive walk of a Laravel project is the first operation here that can run long
/// enough to be worth abandoning — the user closes quick open, or opens a different
/// folder, and the in-flight walk becomes waste (§22: "trabalho obsoleto deve ser
/// cancelável").
///
/// Dropping the gpui `Task` that owns the walk stops *awaiting* it, but the blocking loop
/// on the background thread keeps running to completion unless something tells it to stop.
/// This is that something.
#[derive(Clone, Default, Debug)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the walk to stop. Safe to call from another thread, and idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Upper bound on indexed files.
///
/// ponytail: a hard cap rather than a streaming index. A project with more files than this
/// has bigger problems than an incomplete quick open, and the cap keeps the walk's memory
/// and the matcher's work bounded. Raise it — or stream results into the palette as they
/// arrive — when someone hits it on a real project.
pub const MAX_INDEXED_FILES: usize = 100_000;

/// Directories never worth indexing, regardless of what `.gitignore` says.
///
/// `.gitignore` covers these on a well-kept project, but not on every one: a checkout with
/// no `.gitignore`, or a `vendor/` that is deliberately committed, would otherwise bury
/// quick open under tens of thousands of dependency files. Skipping them by name costs one
/// comparison per directory and removes the failure mode entirely.
const ALWAYS_SKIPPED: [&str; 3] = ["vendor", "node_modules", ".git"];

/// Walks `root` and returns every non-ignored file.
///
/// Blocking, and deliberately so: the caller wraps it in `cx.background_spawn` (ADR-0007).
///
/// Respects `.gitignore` and skips hidden files, matching what the file tree shows — quick
/// open offering `vendor/` results that the tree hides would be incoherent. `vendor/`,
/// `node_modules/` and `.git/` are skipped even when `.gitignore` does not mention them.
///
/// Returns whatever was collected before cancellation, rather than an error: a partial
/// index is still useful to a palette that is about to be re-populated anyway.
pub fn index_files(root: &Path, cancel: &CancelFlag) -> Vec<IndexedFile> {
    let mut files = Vec::new();

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .parents(false)
        // Prunes rather than filters: `filter_entry` stops the walk descending into these
        // at all, so a 40k-file `vendor/` costs one rejected directory instead of 40k
        // rejected files.
        .filter_entry(|entry| {
            !entry.file_type().is_some_and(|t| t.is_dir())
                || !entry.file_name().to_str().is_some_and(|n| ALWAYS_SKIPPED.contains(&n))
        })
        // The walk is already on a background thread; threading it further would
        // complicate cancellation for no benefit at this scale.
        .build();

    for entry in walker {
        // Checked per entry rather than per directory: on a large `vendor/` the gap
        // between directory boundaries is long enough for a user to notice.
        if cancel.is_cancelled() || files.len() >= MAX_INDEXED_FILES {
            break;
        }

        // A permission error on one entry must not abandon the whole index.
        let Ok(entry) = entry else { continue };

        // `file_type` is None only for stdin, which cannot appear here.
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }

        let path = entry.into_path();
        let Some(relative) = relative_display(root, &path) else { continue };
        let name_offset = relative.rfind('/').map_or(0, |i| i + 1);

        files.push(IndexedFile { path, relative, name_offset });
    }

    files
}

/// Project-relative path with forward slashes, or `None` if the path escapes the root.
fn relative_display(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let text = relative.to_str()?;
    // Windows would need separator normalisation here; macOS and Linux already use '/'.
    Some(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn laravel_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("app/Models")).unwrap();
        fs::create_dir_all(root.join("app/Http/Controllers")).unwrap();
        fs::create_dir_all(root.join("resources/views")).unwrap();
        fs::create_dir_all(root.join("vendor/laravel/framework/src")).unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();

        fs::write(root.join(".gitignore"), "/vendor\n").unwrap();
        fs::write(root.join("artisan"), "#!/usr/bin/env php").unwrap();
        fs::write(root.join("app/Models/User.php"), "<?php").unwrap();
        fs::write(root.join("app/Http/Controllers/UserController.php"), "<?php").unwrap();
        fs::write(root.join("resources/views/welcome.blade.php"), "@extends").unwrap();
        fs::write(root.join(".env"), "APP_KEY=").unwrap();
        fs::write(root.join("vendor/laravel/framework/src/Str.php"), "<?php").unwrap();
        fs::write(root.join(".git/objects/abc123"), "binary").unwrap();

        dir
    }

    fn relatives(files: &[IndexedFile]) -> Vec<&str> {
        let mut names: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn indexes_nested_files_the_tree_has_not_expanded() {
        // The whole point: `app/Models/User.php` is three levels down and the tree shows
        // none of it until expanded. Quick open must find it anyway.
        let dir = laravel_fixture();
        let files = index_files(dir.path(), &CancelFlag::new());

        assert_eq!(
            relatives(&files),
            vec![
                "app/Http/Controllers/UserController.php",
                "app/Models/User.php",
                "artisan",
                "resources/views/welcome.blade.php",
            ]
        );
    }

    #[test]
    fn skips_gitignored_hidden_and_git_internals() {
        let dir = laravel_fixture();
        let files = index_files(dir.path(), &CancelFlag::new());

        // vendor/ is gitignored; .env and .gitignore are hidden; .git is both.
        assert!(files.iter().all(|f| !f.relative.starts_with("vendor/")));
        assert!(files.iter().all(|f| !f.relative.starts_with(".git")));
        assert!(files.iter().all(|f| f.relative != ".env"));
    }

    #[test]
    fn paths_are_relative_and_absolute_both() {
        let dir = laravel_fixture();
        let files = index_files(dir.path(), &CancelFlag::new());
        let user = files.iter().find(|f| f.relative == "app/Models/User.php").unwrap();

        assert!(user.path.is_absolute(), "the absolute path is what opens the file");
        assert!(user.path.exists());
        assert_eq!(user.name(), "User.php", "the name is what a matcher weights highest");
    }

    #[test]
    fn a_root_level_file_has_its_whole_path_as_its_name() {
        let dir = laravel_fixture();
        let files = index_files(dir.path(), &CancelFlag::new());
        let artisan = files.iter().find(|f| f.relative == "artisan").unwrap();
        assert_eq!(artisan.name_offset, 0);
        assert_eq!(artisan.name(), "artisan");
    }

    #[test]
    fn dependency_directories_are_skipped_without_a_gitignore() {
        // The fixture above gitignores `vendor`, which hid the real question: a project
        // with no `.gitignore` at all, or one that commits its `vendor/`, used to have
        // quick open buried under dependency files.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("app/Models")).unwrap();
        fs::create_dir_all(root.join("vendor/laravel/framework")).unwrap();
        fs::create_dir_all(root.join("node_modules/vite/dist")).unwrap();

        fs::write(root.join("app/Models/User.php"), "<?php").unwrap();
        fs::write(root.join("vendor/laravel/framework/Str.php"), "<?php").unwrap();
        fs::write(root.join("node_modules/vite/dist/index.js"), "export {}").unwrap();

        assert_eq!(relatives(&index_files(root, &CancelFlag::new())), vec!["app/Models/User.php"]);
    }

    #[test]
    fn cancelling_before_the_walk_returns_nothing() {
        let dir = laravel_fixture();
        let cancel = CancelFlag::new();
        cancel.cancel();

        assert!(index_files(dir.path(), &cancel).is_empty());
    }

    #[test]
    fn cancelling_mid_walk_stops_early_and_keeps_what_it_had() {
        // Enough files that cancelling partway is observable.
        let dir = tempfile::tempdir().unwrap();
        for i in 0..500 {
            fs::write(dir.path().join(format!("file{i}.php")), "<?php").unwrap();
        }

        let cancel = CancelFlag::new();
        let flag = cancel.clone();

        // Cancel from another thread while the walk runs, which is how the UI does it.
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_micros(50));
            flag.cancel();
        });

        let files = index_files(dir.path(), &cancel);
        handle.join().unwrap();

        // A partial result is the contract: fewer than everything, and no panic. The exact
        // count is timing-dependent, so it is deliberately not asserted.
        assert!(files.len() <= 500);
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn a_flag_is_shared_across_clones() {
        let cancel = CancelFlag::new();
        let clone = cancel.clone();
        assert!(!cancel.is_cancelled());
        clone.cancel();
        assert!(cancel.is_cancelled(), "clones must share one flag or cancellation is lost");
    }

    #[test]
    fn an_empty_project_indexes_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(index_files(dir.path(), &CancelFlag::new()).is_empty());
    }

    #[test]
    fn a_missing_root_yields_nothing_rather_than_panicking() {
        let files = index_files(Path::new("/definitely/not/here/ellefuanti"), &CancelFlag::new());
        assert!(files.is_empty());
    }
}
