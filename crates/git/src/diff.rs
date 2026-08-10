//! What changed inside one file. Item 2 of #64.

use std::path::Path;

use crate::Repo;

/// What a diff line is.
///
/// Three variants and no "no newline at end of file" marker: libgit2 reports that as a
/// separate origin, and it is folded into the line it belongs to (see [`diff_file`]) rather
/// than becoming a fourth kind that every renderer has to remember not to count.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    /// Unchanged, shown for context.
    Context,
    Added,
    Removed,
}

impl LineKind {
    /// The leading character, exactly as unified diff writes it.
    ///
    /// **Not decoration — this is the non-colour channel #64 requires.** Green and red are
    /// the same colour to a deuteranope, so `+` and `-` are what actually distinguish an
    /// addition from a deletion. They are rendered in the gutter of every line, always.
    /// The colour is the redundant cue, not the primary one.
    pub fn marker(self) -> &'static str {
        match self {
            LineKind::Context => " ",
            LineKind::Added => "+",
            LineKind::Removed => "-",
        }
    }
}

/// One line of a diff.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Line {
    pub kind: LineKind,
    /// The line's text, without its trailing newline.
    ///
    /// Stripped here so a renderer never has to: a `\n` inside a `StyledText` run would lay
    /// out as a visible glyph or an unexpected wrap depending on the shaper, and every
    /// caller would strip it anyway.
    pub text: String,
    /// 1-based line number in the old file. `None` for an added line.
    pub old_line: Option<u32>,
    /// 1-based line number in the new file. `None` for a removed line.
    pub new_line: Option<u32>,
}

/// A contiguous run of changes with its surrounding context — one `@@` block.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@` header, as git writes it.
    pub header: String,
    pub lines: Vec<Line>,
}

/// The diff for one file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiffFile {
    /// Path relative to the repository root.
    pub relative: String,
    pub hunks: Vec<Hunk>,
    /// True when git found the content to be binary and produced no line diff.
    ///
    /// A distinct flag rather than an empty `hunks`, because "binary" and "no changes" are
    /// different things and a panel that renders both as a blank pane is lying about one of
    /// them.
    pub binary: bool,
}

impl DiffFile {
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// `(added, removed)` line counts, for a summary line above the diff.
    pub fn counts(&self) -> (usize, usize) {
        let mut added = 0;
        let mut removed = 0;
        for line in self.hunks.iter().flat_map(|hunk| &hunk.lines) {
            match line.kind {
                LineKind::Added => added += 1,
                LineKind::Removed => removed += 1,
                LineKind::Context => {}
            }
        }
        (added, removed)
    }
}

/// How much unchanged context to keep around each change.
///
/// Three, which is git's default and therefore what the diff people have in their heads
/// already. Not configurable: a setting nobody asked for is #64's "no speculative
/// abstraction", and the number is one line to change if someone does.
const CONTEXT_LINES: u32 = 3;

/// Diff for one file: HEAD (or the index, when the file is staged) against the working tree.
///
/// `path` may be absolute or relative to the repository root. Blocking, per ADR-0007.
///
/// Returns `None` when `path` is not inside a repository, or is outside its working
/// directory, or the diff cannot be produced. An untracked file diffs as entirely added,
/// which is what `git diff --no-index /dev/null` shows and what someone clicking a `?` row
/// expects to see.
///
/// **Diffing against the working tree, not against the index.** With no stage button there
/// is no way to produce an interesting index state from inside this editor, and "what have
/// I changed since the last commit" is the question a read-only panel is being asked. Item
/// 3 splits this into the two diffs it really is.
pub fn diff_file(root: &Path, path: &Path) -> Option<DiffFile> {
    let repo = Repo::discover(root)?;
    let workdir = repo.workdir()?.to_path_buf();

    // Normalise to a repo-relative path. `strip_prefix` handles the absolute case; a path
    // already relative passes through. A path outside the working directory yields `None`
    // rather than being diffed against a same-named file inside it — a genuine mismatch,
    // and silently diffing the wrong file would be worse than showing nothing.
    let relative = if path.is_absolute() {
        // Canonicalise both sides so a symlinked workdir (`/var` → `/private/var` on macOS,
        // which is where every one of these tests runs) does not make a path that really is
        // inside the repository look as though it is not.
        let canonical_path = path.canonicalize().ok();
        let canonical_root = workdir.canonicalize().ok();
        match (&canonical_path, &canonical_root) {
            (Some(p), Some(r)) => p.strip_prefix(r).ok()?.to_path_buf(),
            // A deleted file cannot be canonicalised — and a deleted file is exactly the
            // case where someone most wants to see the diff. Fall back to the uncanonical
            // comparison rather than refusing.
            _ => path.strip_prefix(&workdir).ok()?.to_path_buf(),
        }
    } else {
        path.to_path_buf()
    };
    let relative = relative.to_str()?.to_string();

    let mut options = git2::DiffOptions::new();
    options
        .pathspec(&relative)
        .context_lines(CONTEXT_LINES)
        .include_untracked(true)
        // Without this an untracked file is one "added file" delta with no line content,
        // and the pane renders empty for the status row people click most on a new project.
        .show_untracked_content(true)
        .include_typechange(true);

    // `tree_to_workdir_with_index` is the three-way one: HEAD → index → working tree, which
    // means a file whose change is partly staged still shows the whole change against the
    // last commit. That is the question a read-only panel answers.
    //
    // `None` for the tree on a repository with no commits yet: everything diffs as added,
    // which is correct there.
    let head_tree = repo.repo().head().ok().and_then(|head| head.peel_to_tree().ok());

    let diff =
        repo.repo().diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut options)).ok()?;

    let mut file = DiffFile { relative, hunks: Vec::new(), binary: false };

    // `print` is the callback form: one call per line, with the hunk it belongs to. It is
    // used rather than `foreach` because it is the API that hands over the `@@` header text
    // already formatted, and reproducing that format by hand is pointless work.
    let printed = diff.print(git2::DiffFormat::Patch, |delta, hunk, line| {
        if delta.flags().is_binary() {
            file.binary = true;
            return true;
        }

        match line.origin() {
            // The `diff --git a/… b/…` preamble and the `@@` header come through as lines
            // too; the header is captured from `hunk` instead, and the preamble is dropped.
            'F' | 'H' => {}
            '+' | '-' | ' ' => {
                let Some(hunk) = hunk else { return true };
                let header = String::from_utf8_lossy(hunk.header()).trim_end().to_string();
                if file.hunks.last().map(|h| h.header.as_str()) != Some(header.as_str()) {
                    file.hunks.push(Hunk { header, lines: Vec::new() });
                }

                let kind = match line.origin() {
                    '+' => LineKind::Added,
                    '-' => LineKind::Removed,
                    _ => LineKind::Context,
                };
                // Lossy rather than skipped: a diff hunk with a hole in it would misalign
                // every line number after it, and a file with one invalid byte is still
                // mostly readable. The opposite trade to `status`, and for the opposite
                // reason — nothing here has to be openable, only legible.
                let text =
                    String::from_utf8_lossy(line.content()).trim_end_matches('\n').to_string();

                file.hunks.last_mut().expect("just pushed").lines.push(Line {
                    kind,
                    text,
                    old_line: line.old_lineno(),
                    new_line: line.new_lineno(),
                });
            }
            // `\ No newline at end of file`. Folded into the preceding line rather than
            // becoming a fourth `LineKind`: it annotates that line, and a renderer counting
            // additions must not count it.
            '\\' => {}
            // Origins only `DiffFormat::PatchHeader` and the binary formats produce.
            _ => {}
        }
        true
    });

    // A failed print is a broken repository, and the leaf rule applies: nothing to show,
    // and nothing worth interrupting the user over.
    printed.ok()?;

    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    /// Same fixture discipline as `status`: real `git`, so libgit2 is being checked against
    /// the thing it is meant to agree with.
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
            .expect("git should be on PATH");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn write(dir: &TempDir, name: &str, text: &str) {
        fs::write(dir.path().join(name), text).expect("write");
    }

    fn texts(file: &DiffFile, kind: LineKind) -> Vec<String> {
        file.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind == kind)
            .map(|line| line.text.clone())
            .collect()
    }

    #[test]
    fn not_a_repo_is_none() {
        let dir = TempDir::new().expect("tempdir");
        write(&dir, "a.php", "<?php\n");
        assert!(diff_file(dir.path(), Path::new("a.php")).is_none());
    }

    #[test]
    fn an_edited_file_shows_the_added_and_removed_lines() {
        let dir = repo();
        write(&dir, "a.php", "<?php\nline one\nline two\nline three\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "a.php", "<?php\nline one\nline TWO\nline three\n");

        let file = diff_file(dir.path(), Path::new("a.php")).expect("a diff");
        assert!(!file.binary);
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(texts(&file, LineKind::Removed), ["line two"]);
        assert_eq!(texts(&file, LineKind::Added), ["line TWO"]);
        assert_eq!(file.counts(), (1, 1));
        // Context is what makes it readable, and three lines of a four-line file is all
        // of it.
        assert_eq!(texts(&file, LineKind::Context), ["<?php", "line one", "line three"]);
    }

    /// Line numbers are what a gutter renders, and getting them off by one is the classic
    /// diff bug. Removed lines have no new number and added lines have no old one.
    #[test]
    fn line_numbers_track_both_sides() {
        let dir = repo();
        write(&dir, "a.php", "one\ntwo\nthree\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "a.php", "one\nTWO\nthree\n");

        let file = diff_file(dir.path(), Path::new("a.php")).expect("a diff");
        let lines = &file.hunks[0].lines;

        let removed = lines.iter().find(|l| l.kind == LineKind::Removed).expect("a removal");
        assert_eq!(removed.old_line, Some(2));
        assert_eq!(removed.new_line, None);

        let added = lines.iter().find(|l| l.kind == LineKind::Added).expect("an addition");
        assert_eq!(added.old_line, None);
        assert_eq!(added.new_line, Some(2));

        let first = &lines[0];
        assert_eq!((first.old_line, first.new_line), (Some(1), Some(1)));
    }

    #[test]
    fn the_hunk_header_is_gits_own() {
        let dir = repo();
        write(&dir, "a.php", "one\ntwo\nthree\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "a.php", "one\nTWO\nthree\n");

        let file = diff_file(dir.path(), Path::new("a.php")).expect("a diff");
        assert_eq!(file.hunks[0].header, "@@ -1,3 +1,3 @@");
    }

    /// The row people click most on a new project. An untracked file has to show its
    /// content, not an empty pane.
    #[test]
    fn an_untracked_file_diffs_as_entirely_added() {
        let dir = repo();
        write(&dir, "a.php", "<?php\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "new.php", "<?php\nclass New {}\n");

        let file = diff_file(dir.path(), Path::new("new.php")).expect("a diff");
        assert_eq!(texts(&file, LineKind::Added), ["<?php", "class New {}"]);
        assert_eq!(file.counts(), (2, 0));
    }

    #[test]
    fn a_deleted_file_diffs_as_entirely_removed() {
        let dir = repo();
        write(&dir, "a.php", "<?php\nclass A {}\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        fs::remove_file(dir.path().join("a.php")).expect("rm");

        let file = diff_file(dir.path(), Path::new("a.php")).expect("a diff");
        assert_eq!(texts(&file, LineKind::Removed), ["<?php", "class A {}"]);
        assert_eq!(file.counts(), (0, 2));
    }

    /// A change staged and then edited again still diffs against the last commit, which is
    /// what `diff_tree_to_workdir_with_index` buys and the reason it was chosen.
    #[test]
    fn a_partly_staged_change_diffs_against_head() {
        let dir = repo();
        write(&dir, "a.php", "one\ntwo\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);

        write(&dir, "a.php", "one\nSTAGED\n");
        git(&dir, &["add", "a.php"]);
        write(&dir, "a.php", "one\nSTAGED THEN EDITED\n");

        let file = diff_file(dir.path(), Path::new("a.php")).expect("a diff");
        assert_eq!(texts(&file, LineKind::Removed), ["two"], "against HEAD, not the index");
        assert_eq!(texts(&file, LineKind::Added), ["STAGED THEN EDITED"]);
    }

    #[test]
    fn an_absolute_path_works_the_same_as_a_relative_one() {
        let dir = repo();
        write(&dir, "a.php", "one\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "a.php", "two\n");

        let relative = diff_file(dir.path(), Path::new("a.php")).expect("relative");
        let absolute = diff_file(dir.path(), &dir.path().join("a.php")).expect("absolute");
        assert_eq!(relative, absolute);
    }

    /// A path outside the repository shows nothing rather than diffing a same-named file
    /// that happens to be inside it.
    #[test]
    fn a_path_outside_the_repo_is_none() {
        let dir = repo();
        write(&dir, "a.php", "one\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);

        let elsewhere = TempDir::new().expect("tempdir");
        fs::write(elsewhere.path().join("a.php"), "different\n").expect("write");

        assert!(diff_file(dir.path(), &elsewhere.path().join("a.php")).is_none());
    }

    #[test]
    fn an_unchanged_file_has_no_hunks() {
        let dir = repo();
        write(&dir, "a.php", "one\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);

        let file = diff_file(dir.path(), Path::new("a.php")).expect("a diff");
        assert!(file.is_empty());
        assert!(!file.binary, "unchanged is not binary — the panel must tell them apart");
        assert_eq!(file.counts(), (0, 0));
    }

    /// Binary is a flag, not an empty diff.
    #[test]
    fn a_binary_file_is_flagged() {
        let dir = repo();
        fs::write(dir.path().join("logo.png"), [0u8, 1, 2, 0, 255, 0, 3]).expect("write");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        fs::write(dir.path().join("logo.png"), [0u8, 9, 9, 0, 254, 0, 7]).expect("write");

        let file = diff_file(dir.path(), Path::new("logo.png")).expect("a diff");
        assert!(file.binary);
        assert!(file.is_empty());
    }

    /// A file with no trailing newline. The `\ No newline` marker must not become a line.
    #[test]
    fn a_missing_trailing_newline_is_not_a_diff_line() {
        let dir = repo();
        write(&dir, "a.php", "one\ntwo\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "a.php", "one\nno trailing newline");

        let file = diff_file(dir.path(), Path::new("a.php")).expect("a diff");
        assert_eq!(texts(&file, LineKind::Added), ["no trailing newline"]);
        for line in file.hunks.iter().flat_map(|h| &h.lines) {
            assert!(!line.text.starts_with('\\'), "the no-newline marker leaked in: {line:?}");
        }
    }

    /// A change big enough to split into two hunks, so the multi-hunk path is exercised
    /// rather than assumed.
    #[test]
    fn distant_changes_become_separate_hunks() {
        let dir = repo();
        let original: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        write(&dir, "a.php", &original);
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);

        let edited: String = (1..=40)
            .map(|n| match n {
                2 => "line TWO\n".to_string(),
                39 => "line THIRTY-NINE\n".to_string(),
                _ => format!("line {n}\n"),
            })
            .collect();
        write(&dir, "a.php", &edited);

        let file = diff_file(dir.path(), Path::new("a.php")).expect("a diff");
        assert_eq!(
            file.hunks.len(),
            2,
            "headers: {:?}",
            file.hunks.iter().map(|h| &h.header).collect::<Vec<_>>()
        );
        assert_eq!(file.counts(), (2, 2));
    }

    /// No trailing newline survives into the text, in any variant.
    #[test]
    fn no_line_carries_a_newline() {
        let dir = repo();
        write(&dir, "a.php", "one\ntwo\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);
        write(&dir, "a.php", "one\nTWO\n");

        let file = diff_file(dir.path(), Path::new("a.php")).expect("a diff");
        for line in file.hunks.iter().flat_map(|h| &h.lines) {
            assert!(!line.text.contains('\n'), "{line:?}");
        }
    }

    #[test]
    fn markers_are_the_unified_diff_characters() {
        assert_eq!(LineKind::Added.marker(), "+");
        assert_eq!(LineKind::Removed.marker(), "-");
        assert_eq!(LineKind::Context.marker(), " ");
    }
}
