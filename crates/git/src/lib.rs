//! Git status and diff, read-only.
//!
//! Items 1 and 2 of #64 and nothing else. There is no `stage`, no `commit`, no `push` in
//! this crate, and their absence is the design: #64 says item 5 is where the danger is,
//! and the danger is specifically that losing uncommitted work cannot be undone. A crate
//! that only ever *reads* cannot lose anyone's work, which is what makes it safe to ship
//! before the confirmation machinery those operations need exists.
//!
//! Every function here is synchronous and blocking, like `elle-workspace` and for the same
//! reason: the caller wraps it in `cx.background_spawn` and this crate does not know which
//! executor that is (ADR-0007). No `gpui`, no UI types (ADR-0004).
//!
//! ## git2 rather than the git CLI, and why that is the right call *here*
//!
//! #64 asks for `git2-rs` with a CLI fallback, decided per operation and recorded. For
//! status and diff, git2 is correct on every axis and there is no fallback in this crate:
//!
//! - **Nothing diverges.** The divergences libgit2 is known for — hooks, credential
//!   helpers, `include` in config, some worktree layouts — are all about *writing* or
//!   *networking*. Status and diff read the index, the object database and the working
//!   tree. `core.excludesFile`, `.gitignore` precedence and `.gitattributes` are all
//!   implemented by libgit2 and are what these two operations actually depend on.
//! - **It is already linked.** See the note in `Cargo.toml`: `gpui_util` pulls in `git2`
//!   already, so this costs no binary size worth measuring.
//! - **Cancellation.** A CLI call is a process; stopping it mid-status means killing a
//!   child. libgit2's per-entry callbacks let a walk stop cooperatively, which is what
//!   [`CancelFlag`](elle_workspace::CancelFlag)-shaped cancellation needs. This crate keeps
//!   that abstract — see [`status`]'s `should_cancel` parameter — so it does not have to
//!   depend on `elle-workspace` for one type.
//! - **No parsing.** `git status --porcelain=v2 -z` is parseable but it is still a text
//!   format being reconstructed into types that libgit2 hands over directly.
//!
//! The fallback question comes back for items 3–5, and the answer will be different there:
//! a commit must run `pre-commit`, and libgit2 does not run hooks. That is a real reason to
//! shell out for *commit* specifically, and it is written down here so item 4 does not have
//! to rediscover it.
//!
//! ## Not a repository is not an error
//!
//! §24 and #25 make this a leaf: a folder that is not a git repo, a repo mid-rebase, or a
//! corrupt `.git` must not break editing. Most folders anyone opens are not repositories,
//! so [`Repo::discover`] returns `Option` rather than `Result` — there is no error to
//! report, and no dialog and no log line, because nothing went wrong.

mod diff;
mod status;

pub use diff::{DiffFile, Hunk, Line, LineKind, diff_file};
pub use status::{FileStatus, RepoStatus, Status, status};

use std::path::{Path, PathBuf};

/// An open git repository.
///
/// Wraps `git2::Repository`, which is deliberately **not** re-exported: keeping libgit2's
/// types inside this crate is what lets items 3–5 swap an operation to the CLI without the
/// call sites noticing, and it keeps `crates/app` from acquiring a git2 dependency of its
/// own (ADR-0004's boundary, applied one layer in).
///
/// Not `Send`, because `git2::Repository` is not. This is the reason [`status`] and
/// [`diff_file`] take a `&Path` and open the repository themselves rather than taking a
/// `&Repo`: a background task has to own everything it touches, and a handle that cannot
/// cross to the background executor would push every caller into re-opening anyway. Opening
/// is cheap — it reads `.git/config` and the index header, not the object database — and
/// paying it per call buys an API where the blocking function is a plain
/// `fn(&Path) -> T` that `cx.background_spawn` accepts without ceremony.
pub struct Repo {
    inner: git2::Repository,
}

impl Repo {
    /// Opens the repository containing `path`, or `None` if there is not one.
    ///
    /// Discovery walks upward, so opening a subdirectory of a checkout finds the checkout —
    /// which is what someone opening `app/Models` in this editor expects.
    ///
    /// `None` covers every way this can fail to produce a repository, and they are all
    /// ordinary: not a repo, the folder was deleted, `.git` is unreadable, the repo is
    /// corrupt. None of them is worth an error type, because the only thing any caller can
    /// do is show no git panel. That is the leaf-in-the-graph rule from #25 expressed as a
    /// return type rather than as a promise in a comment.
    pub fn discover(path: &Path) -> Option<Self> {
        // `discover` also stops at a filesystem boundary and honours `GIT_CEILING_DIRECTORIES`
        // the way git does.
        let inner = git2::Repository::discover(path).ok()?;
        Some(Self { inner })
    }

    /// The working directory root — the folder containing `.git`.
    ///
    /// `None` for a bare repository. A bare repo has no working tree, so it has no file
    /// status and nothing to diff against; callers treat it exactly like "not a repo",
    /// which is why this is an `Option` rather than a panic on a case that really happens
    /// (someone opens a `--bare` clone or a server-side hook directory).
    pub fn workdir(&self) -> Option<&Path> {
        self.inner.workdir()
    }

    /// The short name of the checked-out branch, or a short commit id when detached.
    ///
    /// `None` on a repository with no commits yet, where `HEAD` points at a branch that
    /// does not exist. That is a real state — `git init` and nothing else — and it is the
    /// one most likely to be hit by someone trying this editor on a new folder, so it
    /// returns "no branch" rather than an error.
    pub fn head_branch(&self) -> Option<String> {
        let head = self.inner.head().ok()?;
        if head.is_branch() {
            return head.shorthand().map(str::to_string);
        }
        // Detached HEAD: show the abbreviated id, which is what `git status` does.
        let id = head.target()?;
        Some(id.to_string().chars().take(7).collect())
    }

    fn repo(&self) -> &git2::Repository {
        &self.inner
    }
}

/// Where the repository root is for `path`, if `path` is inside a non-bare repository.
///
/// The one-shot form of [`Repo::discover`] plus [`Repo::workdir`], for the common case of
/// "is this folder a repo, and if so where does it start". Blocking; call it off the UI
/// thread with everything else.
pub fn discover_workdir(path: &Path) -> Option<PathBuf> {
    Repo::discover(path)?.workdir().map(Path::to_path_buf)
}
