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
