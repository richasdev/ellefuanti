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
/// Stages one file (adds it to the index), or unstages it (restores the index entry to
/// HEAD's version).
///
/// # git2, and why it is safe here where commit is not
///
/// Staging touches only the *index* — the worktree file and every commit are untouched,
/// so both directions are freely reversible and neither needs a confirmation. This is
/// the half of the write path #64 rates safe to build first. `git2` is fine for it:
/// hooks do not run on `add`, so the known libgit2 gap does not apply.
///
/// Unstaging a file that is new in the index (no HEAD version) removes the entry, which
/// is what `git restore --staged` does for the same case.
pub fn stage(root: &Path, path: &Path, stage: bool) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let repo = git2::Repository::discover(root).context("not a git repository")?;
    let workdir = repo.workdir().context("bare repository")?;
    // Canonical forms before the strip — the /var vs /private/var trap, met for the
    // fourth time in this codebase and now expected on sight: libgit2 canonicalises the
    // workdir, the caller's path is whatever spelling it arrived with, and a failed
    // strip here surfaced as git2's "repo path should be relative".
    let workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let relative = canonical.strip_prefix(&workdir).unwrap_or(path);

    if stage {
        let mut index = repo.index()?;
        index.add_path(relative)?;
        index.write()?;
        return Ok(());
    }

    // Unstage: reset the index entry to HEAD. `reset_default` is exactly
    // `git restore --staged <path>` — worktree untouched.
    let head = repo.head().ok().and_then(|h| h.peel(git2::ObjectType::Commit).ok());
    repo.reset_default(head.as_ref(), [relative])?;
    Ok(())
}

/// Commits the staged changes with `message`, through the **git CLI**, not libgit2.
///
/// The choice #64 asks to be made per operation and recorded: libgit2 does not run
/// hooks, and a commit that skips the user's pre-commit hook writes commits their own
/// tooling would have rejected — the crate docs have carried that warning since the
/// read-only panel shipped. The CLI runs the hooks, respects `commit.gpgsign`, the
/// user's editor-less `-m` path, and `include`d config, all the places libgit2 quietly
/// diverges. The cost is a subprocess per commit, which is commit-rate.
///
/// Blocking; run it on the background executor. Returns the CLI's stderr on failure,
/// because a hook's rejection message is *for the user* and swallowing it would turn
/// "your linter said no" into "the commit silently did not happen".
pub fn commit(root: &Path, message: &str) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["commit", "-m", message])
        .output()
        .context("could not run git — is it installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        anyhow::bail!("{}", if stderr.trim().is_empty() { stdout } else { stderr });
    }
    Ok(stdout)
}

/// Fetches from the configured remotes, through the CLI like every write (#64).
///
/// Safe by construction: fetch updates remote-tracking refs and touches no working-tree
/// file, so the danger note on this issue (uncommitted work is unrecoverable) does not
/// reach it. Credentials come from the user's own git — ssh agent, credential helper —
/// which is the entire reason this is the CLI and not libgit2's auth callbacks.
pub fn fetch(root: &Path) -> anyhow::Result<String> {
    run_git(root, &["fetch", "--prune"])
}

/// Pushes the current branch, plain. **There is no force flag on purpose** — a force
/// push is one of the destructive operations #25 names, and the way to guarantee this
/// panel never runs one is for the argument not to exist in the signature.
pub fn push(root: &Path) -> anyhow::Result<String> {
    run_git(root, &["push"])
}

/// Local branches, current first: `(name, is_current)`.
pub fn branches(root: &Path) -> anyhow::Result<Vec<(String, bool)>> {
    let out = run_git(root, &["branch", "--list", "--format=%(HEAD) %(refname:short)"])?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let current = line.starts_with('*');
            let name = line.trim_start_matches(['*', ' ']).trim();
            (!name.is_empty()).then(|| (name.to_string(), current))
        })
        .collect())
}

/// Switches to `name`, refusing outright if the working tree has any change.
///
/// Stricter than `git switch` on purpose: git carries compatible changes across, which
/// is exactly the surprise the #64 danger note exists to prevent — a file that silently
/// travelled to another branch is uncommitted work in a place the user did not put it.
/// The refusal message says what to do instead of doing it for them.
pub fn switch_branch(root: &Path, name: &str) -> anyhow::Result<String> {
    let status = run_git(root, &["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        anyhow::bail!("the working tree has changes; commit or stash before switching");
    }
    run_git(root, &["switch", name])
}

/// One CLI call, stderr surfaced on failure — a remote's rejection message is for the
/// user, same rule as commit's hook output.
fn run_git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("could not run git — is it installed?")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        anyhow::bail!("{}", if stderr.trim().is_empty() { stdout } else { stderr });
    }
    Ok(stdout)
}

pub fn discover_workdir(path: &Path) -> Option<PathBuf> {
    Repo::discover(path)?.workdir().map(Path::to_path_buf)
}

#[cfg(test)]
mod write_tests {
    use super::*;
    use std::process::Command;

    /// A real repository, because staging against a mock proves the mock.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.php"), "<?php\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn fetch_and_push_round_trip_through_a_local_remote() {
        let dir = repo();
        // A bare sibling as `origin` — file:// transport, no credentials involved.
        let remote = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap().status.success(),
                "git {args:?}"
            );
        };
        assert!(
            Command::new("git")
                .args(["init", "-q", "--bare", remote.path().to_str().unwrap()])
                .output()
                .unwrap()
                .status
                .success()
        );
        run(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
        run(&["branch", "-M", "main"]);
        run(&["config", "push.default", "current"]);

        push(dir.path()).expect("plain push to the bare remote");
        fetch(dir.path()).expect("fetch after push");

        // The remote really has the commit — read it back from the bare side.
        let heads = Command::new("git")
            .arg("-C")
            .arg(remote.path())
            .args(["log", "--oneline", "main"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&heads.stdout).contains("init"));
    }

    #[test]
    fn branches_list_marks_the_current_one() {
        let dir = repo();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap().status.success()
            );
        };
        run(&["branch", "-M", "main"]);
        run(&["branch", "feature"]);
        let mut all = branches(dir.path()).unwrap();
        all.sort();
        assert_eq!(all, [("feature".to_string(), false), ("main".to_string(), true)]);
    }

    #[test]
    fn switching_with_a_dirty_tree_is_refused_whole() {
        let dir = repo();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap().status.success()
            );
        };
        run(&["branch", "-M", "main"]);
        run(&["branch", "feature"]);

        std::fs::write(dir.path().join("a.php"), "<?php // edited
").unwrap();
        let refused = switch_branch(dir.path(), "feature");
        assert!(refused.is_err(), "dirty tree must refuse — stricter than git, on purpose");
        assert!(porcelain(dir.path()).contains("a.php"), "and the change is untouched");

        // Clean tree: the switch goes through.
        run(&["checkout", "--", "a.php"]);
        switch_branch(dir.path(), "feature").expect("clean switch");
        let current: Vec<_> =
            branches(dir.path()).unwrap().into_iter().filter(|(_, cur)| *cur).collect();
        assert_eq!(current[0].0, "feature");
    }

    fn porcelain(dir: &std::path::Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn stage_and_unstage_round_trip_without_touching_the_worktree() {
        let dir = repo();
        std::fs::write(dir.path().join("a.php"), "<?php // changed\n").unwrap();

        assert!(porcelain(dir.path()).starts_with(" M"), "modified, unstaged");
        stage(dir.path(), &dir.path().join("a.php"), true).unwrap();
        assert!(porcelain(dir.path()).starts_with("M "), "staged");
        stage(dir.path(), &dir.path().join("a.php"), false).unwrap();
        assert!(porcelain(dir.path()).starts_with(" M"), "back to unstaged");
        // The one guarantee that makes this safe to ship without a confirm: the file's
        // content never moved.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.php")).unwrap(),
            "<?php // changed\n"
        );
    }

    #[test]
    fn unstaging_a_new_file_removes_the_index_entry() {
        let dir = repo();
        std::fs::write(dir.path().join("novo.php"), "<?php\n").unwrap();
        stage(dir.path(), &dir.path().join("novo.php"), true).unwrap();
        assert!(porcelain(dir.path()).starts_with("A "), "added");
        stage(dir.path(), &dir.path().join("novo.php"), false).unwrap();
        assert!(porcelain(dir.path()).starts_with("??"), "untracked again, file intact");
        assert!(dir.path().join("novo.php").exists());
    }

    #[test]
    fn commit_goes_through_the_cli_and_hooks_can_refuse() {
        let dir = repo();
        std::fs::write(dir.path().join("a.php"), "<?php // v2\n").unwrap();
        stage(dir.path(), &dir.path().join("a.php"), true).unwrap();
        commit(dir.path(), "second").unwrap();
        assert!(porcelain(dir.path()).is_empty(), "clean after commit");

        // The reason it is the CLI: a pre-commit hook must be able to say no, and its
        // message must come back to the user. libgit2 would have skipped it silently.
        let hooks = dir.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(
            hooks.join("pre-commit"),
            "#!/bin/sh\necho recusado pelo hook >&2\nexit 1\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(hooks.join("pre-commit"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        std::fs::write(dir.path().join("a.php"), "<?php // v3\n").unwrap();
        stage(dir.path(), &dir.path().join("a.php"), true).unwrap();
        let err = commit(dir.path(), "blocked").unwrap_err().to_string();
        assert!(err.contains("recusado pelo hook"), "the hook's message reaches the user: {err}");
    }
}
