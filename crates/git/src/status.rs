//! Which files changed, and how. Item 1 of #64.

use std::path::{Path, PathBuf};

use crate::Repo;

/// What happened to one file, from the working tree's point of view.
///
/// **One status per file, not two.** Git's real model is two independent axes — what the
/// index has staged, and what the working tree has on top of it — and a file can be
/// modified in both at once. A faithful model would be a pair. This is deliberately the
/// flattened version, because the panel this feeds is read-only: with no stage and no
/// unstage button there is nothing a user can *do* with the distinction, and the pair would
/// be two columns of jargon in a sidebar. [`FileStatus::staged`] keeps the one bit that
/// still means something without a write path — "some of this is already staged" — and item
/// 3 is where the pair has to come back, because that is when the two halves become
/// separately actionable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// In the working tree or the index, not in HEAD.
    Added,
    /// Content differs.
    Modified,
    /// In HEAD, gone from the working tree or the index.
    Deleted,
    /// Moved, with content similar enough that git paired the two paths.
    Renamed,
    /// Not tracked and not ignored — the state most files in a new project are in.
    Untracked,
    /// Both sides changed and git could not merge them. Listed first in the panel: a
    /// conflict is the only status here that is blocking work rather than describing it.
    Conflicted,
}

impl Status {
    /// A one- or two-character marker, the way `git status --short` writes it.
    ///
    /// **This is the accessibility requirement, not decoration.** #64 and #71 both say the
    /// markers must not be colour-only, and this is the second channel: a red `D` and a
    /// green `A` differ by their glyph before they differ by their colour, so the panel is
    /// readable with no colour perception at all. The renderer shows this next to every
    /// row, always — not on hover, not only in a high-contrast mode.
    ///
    /// The letters are git's own, so anyone who has run `git status -s` already knows them,
    /// and `?` for untracked and `!` for conflicted match the porcelain format.
    pub fn marker(self) -> &'static str {
        match self {
            Status::Added => "A",
            Status::Modified => "M",
            Status::Deleted => "D",
            Status::Renamed => "R",
            Status::Untracked => "?",
            Status::Conflicted => "!",
        }
    }

    /// The word for it, for a tooltip and for anything reading the panel aloud.
    pub fn label(self) -> &'static str {
        match self {
            Status::Added => "Added",
            Status::Modified => "Modified",
            Status::Deleted => "Deleted",
            Status::Renamed => "Renamed",
            Status::Untracked => "Untracked",
            Status::Conflicted => "Conflicted",
        }
    }

    /// Sort rank: conflicts first, then the working set, then untracked noise.
    ///
    /// Untracked files last because on a project with a stale `.gitignore` there can be
    /// hundreds of them, and burying the three files you actually edited under a build
    /// directory is the failure mode that makes a status panel useless.
    fn rank(self) -> u8 {
        match self {
            Status::Conflicted => 0,
            Status::Added | Status::Modified | Status::Deleted | Status::Renamed => 1,
            Status::Untracked => 2,
        }
    }
}

/// One changed file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileStatus {
    /// Absolute path, for opening the file.
    pub path: PathBuf,
    /// Path relative to the repository root — what the panel shows. The absolute path is
    /// noise in a sidebar, the same reasoning as `IndexedFile::relative` in `elle-workspace`.
    pub relative: String,
    pub status: Status,
    /// Whether any part of this change is in the index.
    ///
    /// Not a second `Status`, for the reason on [`Status`]. A read-only panel uses this to
    /// mark a row as already staged and nothing more.
    pub staged: bool,
}

impl FileStatus {
    /// The file name alone, for a panel that shows the name and the directory separately.
    pub fn name(&self) -> &str {
        match self.relative.rfind('/') {
            Some(slash) => &self.relative[slash + 1..],
            None => &self.relative,
        }
    }
}

/// Everything the panel needs for one repository.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RepoStatus {
    /// Checked-out branch, or a short id when detached. `None` before the first commit.
    pub branch: Option<String>,
    /// Changed files, conflicts first, then path order.
    pub files: Vec<FileStatus>,
    /// Whether the walk stopped early because the caller cancelled it.
    ///
    /// Carried rather than swallowed so the caller can tell a genuinely clean repository
    /// from a walk that was abandoned. Showing "no changes" for the second would be a lie
    /// about the repository, and it is the kind that erodes trust in the whole panel.
    pub cancelled: bool,
}

impl RepoStatus {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Upper bound on listed files.
///
/// ponytail: a hard cap, matching `MAX_INDEXED_FILES`'s reasoning. A repository with more
/// than this many changes is mid-catastrophe — a `node_modules` committed, a line-ending
/// change across a monorepo — and the honest thing is a bounded list rather than a sidebar
/// that takes a second to lay out. 10k rather than the index's 100k because these rows are
/// rendered as a list, not fed to a matcher.
pub const MAX_STATUS_FILES: usize = 10_000;

/// Status for the repository containing `root`.
///
/// Blocking, per ADR-0007. Returns `None` when `root` is not inside a non-bare repository —
/// the common case, and not an error (see the crate docs).
///
/// `should_cancel` is polled per entry. It is a closure rather than an
/// `elle_workspace::CancelFlag` so this crate does not depend on `elle-workspace` for a
/// single type; the caller passes `|| flag.is_cancelled()` and gets the same cooperative
/// cancellation quick open uses. A cancelled walk returns what it had with
/// [`RepoStatus::cancelled`] set, rather than an error, for the same reason `index_files`
/// does: partial results are useful and the caller usually threw them away anyway.
pub fn status(root: &Path, should_cancel: &dyn Fn() -> bool) -> Option<RepoStatus> {
    let repo = Repo::discover(root)?;
    let workdir = repo.workdir()?.to_path_buf();

    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        // Show the directory, not its contents. An untracked `storage/framework/cache/`
        // with two thousand files inside is one row, which is what `git status` shows and
        // the only version that fits in a sidebar.
        .recurse_untracked_dirs(false)
        // Ignored files are not changes. Listing them would drown the panel in `vendor/`
        // on precisely the projects this editor is for.
        .include_ignored(false)
        .include_unmodified(false)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true)
        // `.gitignore` is read per directory as the walk descends, which is what makes a
        // nested ignore file work at all.
        .update_index(false);

    // A repository mid-rebase, mid-merge or with a corrupt index reaches here and fails.
    // `ok()?` is the leaf rule again: the panel shows nothing, the editor keeps working.
    let statuses = repo.repo().statuses(Some(&mut options)).ok()?;

    let mut files = Vec::new();
    let mut cancelled = false;

    for entry in statuses.iter() {
        // Polled per entry rather than per batch: on the repository where this matters,
        // the entries *are* the work.
        if should_cancel() {
            cancelled = true;
            break;
        }
        if files.len() >= MAX_STATUS_FILES {
            break;
        }

        let flags = entry.status();
        let Some(status) = classify(flags) else { continue };

        // A rename reports the new path in `index_to_workdir`/`head_to_index`; `path()`
        // gives the old one for a rename detected in the index. Prefer whichever side
        // describes where the file is *now*, so clicking the row opens something that
        // exists.
        let relative = entry
            .index_to_workdir()
            .and_then(|delta| delta.new_file().path().map(Path::to_path_buf))
            .or_else(|| {
                entry.head_to_index().and_then(|d| d.new_file().path().map(Path::to_path_buf))
            })
            .or_else(|| entry.path().map(PathBuf::from));
        let Some(relative) = relative else { continue };

        // Paths that are not UTF-8 are skipped rather than lossily converted. A lossy name
        // is a path that will not open, and a row that does nothing when clicked is worse
        // than a row that is not there. Rare enough on macOS to be the right trade.
        let Some(relative) = relative.to_str().map(str::to_string) else { continue };

        files.push(FileStatus {
            path: workdir.join(&relative),
            relative,
            status,
            staged: flags.intersects(STAGED),
        });
    }

    // Conflicts first, then alphabetically within each rank, so the list does not reshuffle
    // between refreshes — a sidebar whose rows move under the pointer is unusable.
    files.sort_by(|a, b| {
        a.status.rank().cmp(&b.status.rank()).then_with(|| a.relative.cmp(&b.relative))
    });

    Some(RepoStatus { branch: repo.head_branch(), files, cancelled })
}

/// The index-side bits: anything here means part of the change is staged.
const STAGED: git2::Status = git2::Status::INDEX_NEW
    .union(git2::Status::INDEX_MODIFIED)
    .union(git2::Status::INDEX_DELETED)
    .union(git2::Status::INDEX_RENAMED)
    .union(git2::Status::INDEX_TYPECHANGE);

/// Flattens git's two-axis status into one [`Status`].
///
/// Order is the whole content of this function, and each step is a decision:
///
/// 1. **Conflicted wins outright.** It is the only status that blocks work.
/// 2. **Untracked next**, before the modified checks, because git also sets `WT_NEW` for a
///    file that is untracked, and reporting it as "added" would claim it is going into the
///    next commit when it is not.
/// 3. **Working tree before index** for everything else: what is on disk is what the user
///    is looking at in the editor, so when the two disagree the panel describes the disk.
///
/// Returns `None` for a status with no bits we show — `IGNORED` (excluded by the options
/// above, but a caller could pass their own) and `CURRENT`.
fn classify(flags: git2::Status) -> Option<Status> {
    use git2::Status as S;

    if flags.contains(S::CONFLICTED) {
        return Some(Status::Conflicted);
    }
    if flags.contains(S::WT_NEW) {
        // `INDEX_NEW | WT_NEW` is a staged new file that was then edited again: tracked, so
        // Added rather than Untracked.
        return Some(if flags.contains(S::INDEX_NEW) { Status::Added } else { Status::Untracked });
    }
    if flags.intersects(S::WT_DELETED | S::INDEX_DELETED) {
        return Some(Status::Deleted);
    }
    if flags.intersects(S::WT_RENAMED | S::INDEX_RENAMED) {
        return Some(Status::Renamed);
    }
    if flags.contains(S::INDEX_NEW) {
        return Some(Status::Added);
    }
    // TYPECHANGE — a file becoming a symlink — is reported as a modification. It is rare,
    // and "modified" is true enough for a read-only panel; a distinct status would be a
    // seventh marker to explain for a case most users will never produce.
    if flags.intersects(S::WT_MODIFIED | S::INDEX_MODIFIED | S::WT_TYPECHANGE | S::INDEX_TYPECHANGE)
    {
        return Some(Status::Modified);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    /// A real repository on disk, made with the real `git` binary.
    ///
    /// Shelling out to `git` in the *test* while the code under test uses libgit2 is the
    /// point, not an inconsistency: it means the fixtures are whatever real git produces,
    /// so a divergence between the two implementations shows up as a failing test rather
    /// than as a bug report. Building the fixture with git2 would test git2 against itself.
    fn repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(&dir, &["init", "--initial-branch=main"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        dir
    }

    fn git(dir: &TempDir, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git should be on PATH for these tests");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write(dir: &TempDir, name: &str, text: &str) {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, text).expect("write");
    }

    fn never() -> impl Fn() -> bool {
        || false
    }

    /// The case the whole crate is shaped around: most folders are not repositories.
    #[test]
    fn a_folder_that_is_not_a_repo_is_none_not_an_error() {
        let dir = TempDir::new().expect("tempdir");
        write(&dir, "notes.txt", "just a folder");

        assert!(status(dir.path(), &never()).is_none());
        assert!(Repo::discover(dir.path()).is_none());
        assert!(crate::discover_workdir(dir.path()).is_none());
    }

    /// A path that does not exist at all. Same answer, and it has to be, because the
    /// folder can be deleted between the refresh being scheduled and it running.
    #[test]
    fn a_missing_path_is_none() {
        let dir = TempDir::new().expect("tempdir");
        let gone = dir.path().join("no-such-folder");
        assert!(status(&gone, &never()).is_none());
    }

    #[test]
    fn a_fresh_repo_with_no_commits_has_no_branch_and_no_error() {
        let dir = repo();
        let status = status(dir.path(), &never()).expect("a repo");

        // HEAD points at `refs/heads/main`, which does not exist yet.
        assert_eq!(status.branch, None);
        assert!(status.is_empty());
    }

    #[test]
    fn an_untracked_file_is_untracked() {
        let dir = repo();
        write(&dir, "new.php", "<?php\n");

        let status = status(dir.path(), &never()).expect("a repo");
        assert_eq!(status.files.len(), 1);
        assert_eq!(status.files[0].status, Status::Untracked);
        assert_eq!(status.files[0].relative, "new.php");
        assert!(!status.files[0].staged);
        // The absolute path has to actually point at the file, because clicking the row
        // opens it.
        assert!(status.files[0].path.is_file());
    }

    #[test]
    fn a_staged_new_file_is_added_and_staged() {
        let dir = repo();
        write(&dir, "new.php", "<?php\n");
        git(&dir, &["add", "new.php"]);

        let status = status(dir.path(), &never()).expect("a repo");
        assert_eq!(status.files[0].status, Status::Added);
        assert!(status.files[0].staged);
    }

    #[test]
    fn an_edited_tracked_file_is_modified() {
        let dir = repo();
        write(&dir, "a.php", "<?php\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "a.php", "<?php\n// edited\n");

        let status = status(dir.path(), &never()).expect("a repo");
        assert_eq!(status.files.len(), 1);
        assert_eq!(status.files[0].status, Status::Modified);
        assert!(!status.files[0].staged, "edited on disk only, nothing in the index");
        assert_eq!(status.branch.as_deref(), Some("main"));
    }

    #[test]
    fn a_deleted_tracked_file_is_deleted() {
        let dir = repo();
        write(&dir, "a.php", "<?php\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        fs::remove_file(dir.path().join("a.php")).expect("rm");

        let status = status(dir.path(), &never()).expect("a repo");
        assert_eq!(status.files[0].status, Status::Deleted);
    }

    /// A staged rename. `renames_head_to_index` is what makes this one row rather than an
    /// add and a delete, and the assertion is that the *new* path is what shows — clicking
    /// the old one would open nothing.
    #[test]
    fn a_rename_reports_the_new_path() {
        let dir = repo();
        write(&dir, "old.php", "<?php\n// enough content to match on\nclass Foo {}\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        git(&dir, &["mv", "old.php", "new.php"]);

        let status = status(dir.path(), &never()).expect("a repo");
        assert_eq!(status.files.len(), 1);
        assert_eq!(status.files[0].status, Status::Renamed);
        assert_eq!(status.files[0].relative, "new.php");
    }

    /// Opening a subdirectory has to find the repository above it — the common case when
    /// someone opens `app/` rather than the project root.
    #[test]
    fn discovery_walks_up_from_a_subdirectory() {
        let dir = repo();
        write(&dir, "app/Models/User.php", "<?php\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "app/Models/User.php", "<?php\n// edited\n");

        let status = status(&dir.path().join("app/Models"), &never()).expect("found the repo");
        assert_eq!(status.files[0].relative, "app/Models/User.php", "relative to the repo root");
    }

    #[test]
    fn ignored_files_are_not_listed() {
        let dir = repo();
        write(&dir, ".gitignore", "vendor/\n");
        write(&dir, "vendor/autoload.php", "<?php\n");
        git(&dir, &["add", ".gitignore"]);
        git(&dir, &["commit", "-m", "first"]);

        let status = status(dir.path(), &never()).expect("a repo");
        assert!(
            status.is_empty(),
            "vendor/ is ignored, and .gitignore is committed: {:?}",
            status.files
        );
    }

    /// An untracked directory is one row, not one per file inside it.
    #[test]
    fn an_untracked_directory_collapses_to_one_row() {
        let dir = repo();
        for n in 0..5 {
            write(&dir, &format!("build/out{n}.txt"), "x");
        }

        let status = status(dir.path(), &never()).expect("a repo");
        assert_eq!(status.files.len(), 1);
        assert_eq!(status.files[0].relative, "build/");
    }

    /// Conflicts sort above everything, and untracked sinks below the real work.
    #[test]
    fn conflicts_first_untracked_last() {
        let dir = repo();
        write(&dir, "b-modified.php", "<?php\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "b-modified.php", "<?php\n// edited\n");
        write(&dir, "a-untracked.php", "<?php\n");

        let status = status(dir.path(), &never()).expect("a repo");
        let order: Vec<_> = status.files.iter().map(|f| f.relative.as_str()).collect();
        // Alphabetically `a-untracked` sorts first; by rank it must not.
        assert_eq!(order, ["b-modified.php", "a-untracked.php"]);
    }

    /// A real merge conflict, produced by real git, because the `CONFLICTED` bit is the one
    /// that is easiest to get wrong by reasoning about it instead of observing it.
    #[test]
    fn a_merge_conflict_is_conflicted() {
        let dir = repo();
        write(&dir, "a.php", "<?php\n// base\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "base"]);

        git(&dir, &["checkout", "-b", "other"]);
        write(&dir, "a.php", "<?php\n// theirs\n");
        git(&dir, &["commit", "-am", "theirs"]);

        git(&dir, &["checkout", "main"]);
        write(&dir, "a.php", "<?php\n// ours\n");
        git(&dir, &["commit", "-am", "ours"]);

        // Expected to fail — that is the point.
        let out = Command::new("git")
            .args(["merge", "other"])
            .current_dir(dir.path())
            .output()
            .expect("git merge");
        assert!(!out.status.success(), "the merge was supposed to conflict");

        let status = status(dir.path(), &never()).expect("a repo mid-merge is still readable");
        assert_eq!(status.files.len(), 1);
        assert_eq!(status.files[0].status, Status::Conflicted);
        assert_eq!(status.files[0].status.marker(), "!");
    }

    /// Cancellation stops the walk and says so, rather than reporting a clean repository.
    #[test]
    fn cancellation_is_reported_not_swallowed() {
        let dir = repo();
        write(&dir, "a.php", "<?php\n");
        write(&dir, "b.php", "<?php\n");

        let status = status(dir.path(), &|| true).expect("a repo");
        assert!(status.cancelled);
        assert!(status.files.is_empty(), "cancelled before the first entry");
    }

    /// Every marker is distinct, which is what makes the glyph a real second channel
    /// alongside colour rather than a decoration that repeats it.
    #[test]
    fn every_status_has_a_distinct_marker() {
        let all = [
            Status::Added,
            Status::Modified,
            Status::Deleted,
            Status::Renamed,
            Status::Untracked,
            Status::Conflicted,
        ];
        let mut markers: Vec<_> = all.iter().map(|s| s.marker()).collect();
        markers.sort_unstable();
        markers.dedup();
        assert_eq!(markers.len(), all.len(), "two statuses share a marker");

        let mut labels: Vec<_> = all.iter().map(|s| s.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn name_is_the_last_path_component() {
        let file = FileStatus {
            path: PathBuf::from("/repo/app/Models/User.php"),
            relative: "app/Models/User.php".to_string(),
            status: Status::Modified,
            staged: false,
        };
        assert_eq!(file.name(), "User.php");

        let root = FileStatus { relative: "README.md".to_string(), ..file };
        assert_eq!(root.name(), "README.md");
    }
}
