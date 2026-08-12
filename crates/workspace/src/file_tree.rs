//! Lazy file tree over an open folder.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    Directory,
    File,
}

/// One row of the tree, already flattened for rendering.
///
/// Flat instead of nested children: the renderer virtualises a flat list (gpui's
/// `uniform_list`), so a flat `Vec` is what it needs and indentation is just `depth`.
/// A nested tree would have to be flattened on every frame.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    /// Display name (final path component).
    pub name: String,
    pub kind: EntryKind,
    /// Nesting level; the root's children are depth 0.
    pub depth: usize,
    /// Directories only: whether children are currently shown.
    pub expanded: bool,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}

/// The visible file tree for one open folder.
///
/// Reads a directory only when it is expanded, so opening a Laravel project does not
/// stat 30k files in `vendor/` before the window appears (§13 lazy loading).
pub struct FileTree {
    root: PathBuf,
    entries: Vec<Entry>,
    ignore: Gitignore,
    /// Whether to show entries matched by .gitignore, and dotfiles.
    show_hidden: bool,
}

impl FileTree {
    /// Opens `root` and reads its immediate children only.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root.canonicalize().with_context(|| format!("opening {}", root.display()))?;

        // .gitignore of the opened folder. Errors are non-fatal: a malformed ignore file
        // must not stop the folder from opening (§24 fault isolation).
        let mut builder = GitignoreBuilder::new(&root);
        if let Some(err) = builder.add(root.join(".gitignore")) {
            tracing::debug!("ignoring .gitignore in {}: {err}", root.display());
        }
        let ignore = builder.build().unwrap_or_else(|err| {
            tracing::warn!("could not build ignore matcher: {err}");
            Gitignore::empty()
        });

        let mut tree = Self { root, entries: Vec::new(), ignore, show_hidden: false };
        tree.entries = tree.read_dir(&tree.root.clone(), 0)?;
        Ok(tree)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Display name of the open folder.
    pub fn root_name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// Toggles hidden/ignored visibility and reloads, collapsing back to the root.
    pub fn set_show_hidden(&mut self, show: bool) -> Result<()> {
        if self.show_hidden == show {
            return Ok(());
        }
        self.show_hidden = show;
        self.entries = self.read_dir(&self.root.clone(), 0)?;
        Ok(())
    }

    /// Expands or collapses the directory at `index`.
    ///
    /// Expanding splices freshly read children in after the row; collapsing removes the
    /// contiguous run of deeper rows. Both are local edits, not a tree rebuild.
    pub fn toggle(&mut self, index: usize) -> Result<()> {
        let Some(entry) = self.entries.get(index) else { return Ok(()) };
        if !entry.is_dir() {
            return Ok(());
        }

        let (path, depth, expanded) = (entry.path.clone(), entry.depth, entry.expanded);

        if expanded {
            // Every following row deeper than this one is a descendant.
            let end = self.entries[index + 1..]
                .iter()
                .position(|e| e.depth <= depth)
                .map_or(self.entries.len(), |offset| index + 1 + offset);
            self.entries.drain(index + 1..end);
            self.entries[index].expanded = false;
        } else {
            // Read before mutating: a permission error leaves the tree untouched.
            let children = self.read_dir(&path, depth + 1)?;
            self.entries[index].expanded = true;
            self.entries.splice(index + 1..index + 1, children);
        }
        Ok(())
    }

    /// Re-reads the tree from disk, keeping every directory that was expanded expanded.
    ///
    /// # Why this is not `FileTree::new`
    ///
    /// Rebuilding from scratch is one line and loses the thing the user cares about most:
    /// their place. Creating a file three directories deep would collapse the tree back to
    /// the root and leave them to click their way down again, which makes the operation
    /// feel like it went wrong even when it worked.
    ///
    /// So the expanded set is collected first, the root is re-read, and every directory
    /// that was open is re-expanded if it is still there. A directory that was deleted —
    /// or renamed, which is a delete as far as its old path is concerned — simply does not
    /// come back, without that being an error.
    ///
    /// The walk is breadth-first by construction: expanding a directory splices its
    /// children in immediately after it, so re-scanning from the current index onwards
    /// reaches nested directories only after their parents have been opened.
    ///
    /// ponytail: re-reads every expanded directory rather than only the one that changed.
    /// A caller knows which path it touched and could splice one level, but every operation
    /// here follows a click, so this runs at human speed against directories already in the
    /// page cache. Narrow it if a project with thousands of expanded directories ever makes
    /// it visible.
    pub fn refresh(&mut self) -> Result<()> {
        let expanded: std::collections::HashSet<PathBuf> = self
            .entries
            .iter()
            .filter(|entry| entry.is_dir() && entry.expanded)
            .map(|entry| entry.path.clone())
            .collect();

        self.entries = self.read_dir(&self.root.clone(), 0)?;

        let mut index = 0;
        while index < self.entries.len() {
            let entry = &self.entries[index];
            if entry.is_dir() && expanded.contains(&entry.path) {
                // A directory that has become unreadable since it was opened is left
                // collapsed rather than failing the refresh: the rest of the tree is still
                // worth showing, which is the same fault-isolation rule `new` follows for a
                // malformed .gitignore (§24).
                let _ = self.toggle(index);
            }
            index += 1;
        }
        Ok(())
    }

    /// Collapses every directory, leaving only the top level — the explorer's "collapse
    /// all" button.
    ///
    /// Re-reading the root rather than walking and collapsing each open directory: the
    /// result is defined as "what `new` shows", and one read produces exactly that with no
    /// chance of a stray descendant surviving. `show_hidden` is preserved because the read
    /// goes through the same filter every other read does.
    pub fn collapse_all(&mut self) -> Result<()> {
        self.entries = self.read_dir(&self.root.clone(), 0)?;
        Ok(())
    }

    /// Expands every directory in the tree — the explorer's "expand all" button.
    ///
    /// The tree is lazy, so this has to *realise* the whole thing: walk the flat list from
    /// the top and open each directory that is still closed. Opening one splices its
    /// children in immediately after it, so continuing forward from the same index reaches
    /// those children in turn — a breadth-then-depth sweep that terminates because every
    /// splice lands strictly after the cursor and each directory is opened at most once.
    ///
    /// # Cost
    ///
    /// This is the one operation here that reads the entire project rather than one level:
    /// on a repository with a committed `vendor/` and `show_hidden` on, that is every
    /// directory in it. That is acceptable for an *explicit* button press the way it would
    /// not be for something on the typing path — the user asked for the whole tree, and the
    /// `.gitignore`/hidden filter keeps the common case (dependencies ignored) bounded to
    /// the project's own source. A directory that becomes unreadable mid-walk is left
    /// collapsed rather than failing the whole expansion, the same fault-isolation rule
    /// `refresh` follows (§24).
    pub fn expand_all(&mut self) -> Result<()> {
        let mut index = 0;
        while index < self.entries.len() {
            let entry = &self.entries[index];
            if entry.is_dir() && !entry.expanded {
                let _ = self.toggle(index);
            }
            index += 1;
        }
        Ok(())
    }

    /// Reads one directory level, filtered and sorted for display.
    fn read_dir(&self, dir: &Path, depth: usize) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();

        for dirent in
            std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?
        {
            // Skip unreadable individual entries rather than failing the whole listing.
            let Ok(dirent) = dirent else { continue };
            let path = dirent.path();
            let name = dirent.file_name().to_string_lossy().to_string();

            // file_type() avoids a stat syscall per entry on macOS.
            let is_dir = match dirent.file_type() {
                Ok(ft) if ft.is_symlink() => path.is_dir(),
                Ok(ft) => ft.is_dir(),
                Err(_) => continue,
            };

            if !self.show_hidden {
                // `.git` stays hidden even with show_hidden: nothing good comes of
                // browsing loose objects in a file tree.
                if name == ".git" {
                    continue;
                }
                if name.starts_with('.') || self.ignore.matched(&path, is_dir).is_ignore() {
                    continue;
                }
            } else if name == ".git" {
                continue;
            }

            entries.push(Entry {
                path,
                name,
                kind: if is_dir { EntryKind::Directory } else { EntryKind::File },
                depth,
                expanded: false,
            });
        }

        // Directories first, then case-insensitive by name — what every file tree does,
        // and stable regardless of readdir order.
        entries.sort_by(|a, b| {
            b.is_dir()
                .cmp(&a.is_dir())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("app/Models")).unwrap();
        fs::create_dir_all(p.join("vendor/laravel")).unwrap();
        fs::create_dir_all(p.join(".git")).unwrap();
        fs::write(p.join("app/Models/User.php"), "<?php").unwrap();
        fs::write(p.join("artisan"), "#!/usr/bin/env php").unwrap();
        fs::write(p.join(".env"), "APP_KEY=").unwrap();
        fs::write(p.join(".gitignore"), "/vendor\n").unwrap();
        fs::write(p.join("vendor/laravel/x.php"), "<?php").unwrap();
        dir
    }

    #[test]
    fn root_lists_only_top_level_and_hides_ignored() {
        let dir = fixture();
        let tree = FileTree::new(dir.path()).unwrap();
        let names: Vec<&str> = tree.entries().iter().map(|e| e.name.as_str()).collect();

        assert_eq!(names, vec!["app", "artisan"], "vendor/.git/.env/.gitignore must be hidden");
        assert!(tree.entries().iter().all(|e| e.depth == 0));
        assert!(tree.entries().iter().all(|e| !e.expanded));
    }

    #[test]
    fn directories_sort_before_files() {
        let dir = fixture();
        let tree = FileTree::new(dir.path()).unwrap();
        assert!(tree.entries()[0].is_dir());
        assert!(!tree.entries()[1].is_dir());
    }

    // --- refresh, for the operations #126 adds -------------------------------------

    #[test]
    fn refreshing_shows_a_new_file_without_collapsing_the_tree() {
        // The requirement that rules out "just call FileTree::new". A file created three
        // directories deep must appear *and* leave the user where they were; collapsing to
        // the root makes a successful operation feel like a failure.
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();

        let app = tree.entries().iter().position(|e| e.name == "app").unwrap();
        tree.toggle(app).unwrap();
        let models = tree.entries().iter().position(|e| e.name == "Models").unwrap();
        tree.toggle(models).unwrap();
        assert!(tree.entries().iter().any(|e| e.name == "User.php"));

        fs::write(dir.path().join("app/Models/Post.php"), "<?php").unwrap();
        tree.refresh().unwrap();

        assert!(tree.entries().iter().any(|e| e.name == "Post.php"), "the new file must appear");
        assert!(
            tree.entries().iter().any(|e| e.name == "User.php"),
            "a nested expansion must survive the refresh"
        );
        let app = tree.entries().iter().find(|e| e.name == "app").unwrap();
        assert!(app.expanded, "the tree must not collapse to the root");
    }

    #[test]
    fn refreshing_drops_a_deleted_file_and_forgets_a_deleted_directory() {
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();
        let app = tree.entries().iter().position(|e| e.name == "app").unwrap();
        tree.toggle(app).unwrap();

        fs::remove_dir_all(dir.path().join("app")).unwrap();
        // A directory that was expanded and is now gone must not make the refresh fail —
        // the rest of the tree is still worth showing.
        tree.refresh().unwrap();

        assert!(tree.entries().iter().all(|e| e.name != "app"));
        assert!(tree.entries().iter().any(|e| e.name == "artisan"), "the rest must survive");
    }

    #[test]
    fn refreshing_keeps_hidden_files_hidden() {
        // `refresh` re-reads through the same filter, so a setting the user chose is not
        // quietly reset by an unrelated file operation.
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();
        assert!(tree.entries().iter().all(|e| e.name != ".env"));

        tree.refresh().unwrap();
        assert!(tree.entries().iter().all(|e| e.name != ".env"));

        tree.set_show_hidden(true).unwrap();
        tree.refresh().unwrap();
        assert!(
            tree.entries().iter().any(|e| e.name == ".env"),
            "show_hidden must survive a refresh too"
        );
    }

    #[test]
    fn expand_and_collapse_splices_children() {
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();
        let before = tree.len();

        tree.toggle(0).unwrap(); // app
        assert!(tree.entries()[0].expanded);
        assert_eq!(tree.entries()[1].name, "Models");
        assert_eq!(tree.entries()[1].depth, 1);

        tree.toggle(1).unwrap(); // app/Models
        assert_eq!(tree.entries()[2].name, "User.php");
        assert_eq!(tree.entries()[2].depth, 2);

        // Collapsing `app` removes all descendants, not just the direct children.
        tree.toggle(0).unwrap();
        assert_eq!(tree.len(), before);
        assert!(!tree.entries()[0].expanded);
    }

    // --- expand-all / collapse-all, the explorer header buttons ---------------------

    #[test]
    fn collapse_all_returns_the_tree_to_the_root() {
        // The cheap direction: forget every expansion and re-read the top level. What is
        // left is exactly what `FileTree::new` produced, so the assertion is that the two
        // agree.
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();
        let root_only: Vec<String> =
            tree.entries().iter().map(|e| e.name.clone()).collect();

        let app = tree.entries().iter().position(|e| e.name == "app").unwrap();
        tree.toggle(app).unwrap();
        let models = tree.entries().iter().position(|e| e.name == "Models").unwrap();
        tree.toggle(models).unwrap();
        assert!(tree.len() > root_only.len(), "the tree must actually be expanded first");

        tree.collapse_all().unwrap();

        let after: Vec<String> = tree.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(after, root_only, "collapse-all must leave only the top level");
        assert!(tree.entries().iter().all(|e| !e.expanded), "nothing may stay expanded");
    }

    #[test]
    fn expand_all_realises_every_directory_in_the_tree() {
        // The lazy tree shows none of the nested files until a folder is opened. Expand-all
        // walks and opens every directory, so a file three levels down is visible without a
        // single click.
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();
        assert!(tree.entries().iter().all(|e| e.name != "User.php"), "nested, so hidden at first");

        tree.expand_all().unwrap();

        assert!(tree.entries().iter().any(|e| e.name == "User.php"), "the nested file must appear");
        // Every directory that is shown must itself be open — that is what "expand all"
        // means, and a half-open tree would be worse than the lazy one.
        assert!(
            tree.entries().iter().filter(|e| e.is_dir()).all(|e| e.expanded),
            "every directory must end expanded"
        );
    }

    #[test]
    fn expand_all_then_collapse_all_round_trips() {
        // The two buttons are inverses: expanding everything and then collapsing must land
        // back on exactly the top level, not some leftover partial expansion.
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();
        let root_only: Vec<String> = tree.entries().iter().map(|e| e.name.clone()).collect();

        tree.expand_all().unwrap();
        tree.collapse_all().unwrap();

        let after: Vec<String> = tree.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(after, root_only);
    }

    #[test]
    fn expand_all_respects_show_hidden() {
        // Expand-all must not smuggle in ignored directories the tree is hiding: the walk
        // goes through the same per-level filter as every other read, so `vendor/` stays
        // out while it is ignored and comes in once hidden files are shown.
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();
        tree.expand_all().unwrap();
        assert!(tree.entries().iter().all(|e| e.name != "vendor"), "vendor is ignored");

        tree.set_show_hidden(true).unwrap();
        tree.expand_all().unwrap();
        assert!(tree.entries().iter().any(|e| e.name == "vendor"), "now shown, so expanded too");
    }

    #[test]
    fn toggling_a_file_or_bad_index_is_a_no_op() {
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();
        let before = tree.len();
        tree.toggle(1).unwrap(); // artisan, a file
        tree.toggle(9999).unwrap();
        assert_eq!(tree.len(), before);
    }

    #[test]
    fn show_hidden_reveals_dotfiles_and_ignored_but_never_git() {
        let dir = fixture();
        let mut tree = FileTree::new(dir.path()).unwrap();
        tree.set_show_hidden(true).unwrap();
        let names: Vec<&str> = tree.entries().iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&".env"));
        assert!(names.contains(&"vendor"));
        assert!(!names.contains(&".git"));
    }

    #[test]
    fn opening_a_missing_folder_errors() {
        assert!(FileTree::new("/definitely/not/here/ellefuanti").is_err());
    }

    #[test]
    fn root_name_is_the_folder_name() {
        let dir = fixture();
        let tree = FileTree::new(dir.path()).unwrap();
        assert_eq!(
            tree.root_name(),
            dir.path().canonicalize().unwrap().file_name().unwrap().to_string_lossy()
        );
    }
}
