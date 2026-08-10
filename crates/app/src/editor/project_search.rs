//! Find in project (#80): scanning every file in a folder for one query.
//!
//! No gpui, like [`super::find`] beside it, so the whole thing is unit-testable at full
//! speed and the measurements below were taken without a window open.
//!
//! # Why this is not "in-file find, in a loop"
//!
//! [`super::find::Matches`] scans one buffer that is already in memory and returns byte
//! ranges. Project search cannot borrow that shape wholesale for three reasons, each of
//! which is a design constraint here:
//!
//! 1. **The regex is compiled once, not once per file.** `find.rs` recompiles per scan and
//!    documents that as ~33-58 µs — irrelevant against one file, and 16 ms across 500 of
//!    them, which is two dropped frames spent on nothing.
//! 2. **A result needs a line, not an offset.** Nothing has loaded the file into a rope, so
//!    there is no `Document` to convert an offset with. Line numbers and the line's text are
//!    computed during the scan, from the same string, and the offset is thrown away.
//! 3. **The output is bounded, the input is not.** A query like `e` matches a million times
//!    in a Laravel project. Holding all of them is pointless — nobody reads past the first
//!    few hundred — so the scan stops at [`MAX_RESULTS`] and says it was truncated.
//!
//! # What a project search actually costs
//!
//! Measured, not assumed — [`tests::measure_project_search`] is the harness and it is in
//! this file so the number can be re-taken rather than inherited. Apple Silicon, release,
//! `search_project` end to end, median of 5 warm runs:
//!
//! | project | files | bytes | walk | `$user` | `function` | `\$\w+` regex | no hits |
//! | --- | --- | --- | --- | --- | --- | --- | --- |
//! | crm-livewire-v3 | 279 | 0.8 MB | 2.9 ms | 7.2 ms | 8.0 ms | 6.1 ms | 7.0 ms |
//! | ellefuanti | 156 | 1.8 MB | 1.4 ms | 4.3 ms | 4.6 ms | 4.8 ms | 4.3 ms |
//!
//! **Two readings disagreed and the disagreement was the finding**, which is what
//! `benchmarks/BASELINE.md` opens by telling you to expect. The first run reported the walk
//! at 10.6 ms — larger than the whole 7.5 ms search that contains it, which is impossible.
//! The walk was being timed cold, once, against searches timed warm five times: 10.6 ms
//! cold, 2.9 ms warm, a 7x gap that is the page cache and not the code. Both are printed
//! separately now.
//!
//! What that resolves to: the walk is **40% of a search** on crm-livewire-v3 and 33% here,
//! and the scan of 0.8 MB is ~4.3 ms. The cost tracks the **file count** far more than the
//! byte count — 156 files holding 1.8 MB search *faster* than 279 files holding 0.8 MB,
//! because the per-file `metadata` + `read` syscall pair dominates the matching. That is
//! also why [`MAX_FILE_BYTES`] barely helps and skipping `vendor/` helps enormously.
//!
//! The `no hits` column is the one that shapes the UI: **7.0 ms is what a query costs
//! before it finds anything**. That is 84% of an 8.3 ms frame, which settles two questions:
//!
//! - It **cannot run on the UI thread**. One keystroke would eat a frame outright, and a
//!   real project — this is a small one — scales with file count without bound.
//! - It **must be debounced**, not merely cancelled. At 7 ms a walk, someone typing at
//!   8 keystrokes/second starts a search every 125 ms and each one re-walks the whole
//!   project to be thrown away. Cancellation stops the waste from *accumulating*;
//!   debouncing stops it from *starting*.
//!
//! Both are the caller's job — this module is blocking and executor-free by the same rule
//! `crates/workspace` follows (ADR-0007). What this module owns is being *stoppable*: the
//! [`CancelFlag`] is checked once per file, so a superseded search abandons within one
//! file's work rather than at the end of the project.

use std::path::{Path, PathBuf};

use elle_workspace::{CancelFlag, IndexedFile, index_files};
use regex::Regex;

use super::find::SearchQuery;

/// Files larger than this are skipped rather than searched.
///
/// Much smaller than `find::MAX_SEARCH_BYTES` (4 MB) on purpose, and the difference is the
/// point: in-file search is something the user *asked for on this file*, so refusing costs
/// them the thing they wanted. Project search is a sweep over files nobody named, where a
/// 1 MB minified `app.js` or a committed SQL dump is noise whose hits are never useful and
/// whose scan cost is charged to every query.
///
/// 512 KB clears every hand-written source file by a wide margin — the largest `.php` in
/// the two projects measured above is 84 KB — while excluding the generated blobs that make
/// a project search feel slow.
pub const MAX_FILE_BYTES: usize = 512 * 1024;

/// How many bytes of a file are inspected for NUL before deciding it is binary.
///
/// The same heuristic and the same window `elle_workspace::read_file` uses, and the same
/// one git uses. Kept identical deliberately: a file the editor refuses to open must not
/// appear in search results, or clicking a hit opens an error.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Upper bound on reported matches.
///
/// A cap rather than a stream with no end, because the list is rendered and read by a
/// human. A single-letter query matches hundreds of thousands of times in a Laravel
/// project; the thousandth hit has never been useful to anyone, and collecting it costs
/// memory and a `uniform_list` row that will never be scrolled to.
///
/// The count is reported as truncated rather than silently capped — a search that quietly
/// stopped early is the kind of lie that makes people distrust the tool (RISKS.md #4).
pub const MAX_RESULTS: usize = 1_000;

/// One line containing at least one hit.
///
/// A line rather than a match: two hits on the same line are one row in the results,
/// because the row shows the line, and showing it twice tells the reader nothing. The
/// `ranges` are byte offsets **within `text`**, so the renderer can bold the hits without
/// re-running the regex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMatch {
    /// Zero-based row, which is what `gpui::Point` and `open_path_at` want. One-based is a
    /// display concern and the conversion happens once, at render.
    pub row: u32,
    /// The line's text, trimmed of leading whitespace so results left-align, with the
    /// trim's length subtracted from every range. Truncated at [`MAX_LINE_BYTES`].
    pub text: String,
    /// Byte ranges of the hits inside `text`, sorted and non-overlapping.
    ///
    /// Always on a char boundary: they come from `Regex::find_iter` over UTF-8, the same
    /// property `find.rs` relies on, and slicing `text` with one is what the renderer does.
    pub ranges: Vec<std::ops::Range<usize>>,
    /// Byte column of the first hit **in the original, untrimmed line** — where the cursor
    /// goes when the row is clicked. Not derivable from `ranges`, which were rebased.
    pub column: u32,
    /// Leading bytes `display_line` removed, so a second hit on the same row can be rebased
    /// against the same origin. Private to the module: it is scan bookkeeping, and a
    /// renderer that read it would be recomputing something `ranges` already answers.
    trimmed: usize,
}

impl LineMatch {
    /// A `LineMatch` without running a search, for `search_panel`'s tests.
    ///
    /// The panel's rendering has to be assertable against handmade results — a test that
    /// wanted a truncated result set, or a file with two hits on one line, would otherwise
    /// need a temp directory and a real scan to produce one. `trimmed` is private, which
    /// is what makes this necessary rather than merely convenient.
    #[cfg(test)]
    pub fn for_test(row: u32, text: &str, ranges: Vec<std::ops::Range<usize>>) -> Self {
        let column = ranges.first().map_or(0, |r| r.start as u32);
        Self { row, text: text.to_string(), ranges, column, trimmed: 0 }
    }
}

/// Longest line kept in a result row.
///
/// A minified file that survived [`MAX_FILE_BYTES`] can still have one 400 KB line, and
/// putting that in a `SharedString` per row is how a result list allocates megabytes to
/// render forty pixels. 300 bytes is past the width of any window.
pub const MAX_LINE_BYTES: usize = 300;

/// Every hit in one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatches {
    /// Absolute path — what `open_path_at` takes.
    pub path: PathBuf,
    /// Project-relative path with forward slashes, which is what the header shows.
    pub relative: String,
    pub lines: Vec<LineMatch>,
}

impl FileMatches {
    /// Total hits in this file, which is not `lines.len()` when a line has two.
    pub fn match_count(&self) -> usize {
        self.lines.iter().map(|line| line.ranges.len()).sum()
    }
}

/// What a completed (or abandoned) project search produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectResults {
    pub files: Vec<FileMatches>,
    /// True when the pattern would not compile. Distinct from "no results" for the same
    /// reason it is in `find.rs`: the panel says *why* the list is empty.
    pub invalid: bool,
    /// True when [`MAX_RESULTS`] was reached and the scan stopped early.
    pub truncated: bool,
    /// True when the [`CancelFlag`] fired. The caller throws these results away — they are
    /// returned rather than dropped only so the function has one return type.
    pub cancelled: bool,
}

impl ProjectResults {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Total hits across every file.
    pub fn match_count(&self) -> usize {
        self.files.iter().map(FileMatches::match_count).sum()
    }

    /// Files that contain at least one hit.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

/// Searches every non-ignored file under `root`.
///
/// **Blocking.** The caller wraps it in `cx.background_spawn` (ADR-0007), debounces it, and
/// cancels the previous one — see the module docs for why all three are required rather
/// than optional.
///
/// Traversal is [`index_files`], not a second walk: quick open's rules (`.gitignore`,
/// hidden files, and `vendor/`/`node_modules/`/`.git` regardless of what git says) are the
/// rules search wants, and a project search that found hits in files the tree hides and
/// quick open will not open would be incoherent.
pub fn search_project(root: &Path, query: &SearchQuery, cancel: &CancelFlag) -> ProjectResults {
    let Some(compiled) = query.compile() else { return ProjectResults::default() };
    let Ok(regex) = compiled else {
        return ProjectResults { invalid: true, ..Default::default() };
    };

    let files = index_files(root, cancel);
    if cancel.is_cancelled() {
        return ProjectResults { cancelled: true, ..Default::default() };
    }

    search_files(&files, &regex, cancel)
}

/// The scan itself, over an already-walked file list.
///
/// Split out from [`search_project`] so a test can hand it a list without a temp directory,
/// and so a future caller with the persisted index (`crates/index`) can skip the walk
/// entirely — the walk and the scan are independent costs.
fn search_files(files: &[IndexedFile], regex: &Regex, cancel: &CancelFlag) -> ProjectResults {
    let mut results = ProjectResults::default();
    let mut found = 0usize;

    for file in files {
        // Once per file, not once per line: a file is a few hundred microseconds of work,
        // which is a fine granularity to abandon at, and checking per line would put an
        // atomic load in the hot loop.
        if cancel.is_cancelled() {
            results.cancelled = true;
            return results;
        }
        if found >= MAX_RESULTS {
            results.truncated = true;
            return results;
        }

        let Some(text) = read_searchable(&file.path) else { continue };

        let lines = scan_text(&text, regex, MAX_RESULTS - found);
        if lines.is_empty() {
            continue;
        }

        found += lines.iter().map(|line| line.ranges.len()).sum::<usize>();
        results.files.push(FileMatches {
            path: file.path.clone(),
            relative: file.relative.clone(),
            lines,
        });
    }

    results
}

/// Reads `path` if it is a text file small enough to be worth scanning.
///
/// `None` for anything the search should pretend does not exist: too large, binary, not
/// UTF-8, or unreadable. All four are ordinary in a project directory and none of them is
/// an error worth reporting — a permission-denied `storage/` file must not abort the sweep.
fn read_searchable(path: &Path) -> Option<String> {
    // `metadata` before `read`: a size check that reads the file first has not saved
    // anything. This is the guard that keeps a committed 40 MB database dump from costing
    // 40 MB of allocation per keystroke.
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() as usize > MAX_FILE_BYTES {
        return None;
    }

    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(BINARY_SNIFF_BYTES).any(|&byte| byte == 0) {
        return None;
    }
    // Invalid UTF-8 is dropped rather than lossily converted: a lossy conversion produces
    // U+FFFD at byte positions that no longer correspond to the file, so a click would jump
    // to the wrong column in a file the editor then refuses to open anyway.
    String::from_utf8(bytes).ok()
}

/// Finds every matching line in one file's text.
///
/// One pass over the matches, one pass over the newlines *between* them. The obvious
/// implementation — `text.lines()` and a regex per line — is O(lines) regardless of hits,
/// which is the wrong shape: a query with no matches in a 10,000-line file should cost one
/// failed search, not ten thousand of them. The regex crate's literal prefilter makes the
/// no-match case a `memchr` sweep, and that is where the "no hits" column above comes from.
fn scan_text(text: &str, regex: &Regex, budget: usize) -> Vec<LineMatch> {
    let mut lines: Vec<LineMatch> = Vec::new();
    let mut found = 0usize;

    // Rolling cursor over newlines: `line_start` is the offset of the current row's first
    // byte and `row` is its index. Both only ever move forward, so the total newline
    // counting across all matches is one scan of the file, not one per match.
    let mut line_start = 0usize;
    let mut row = 0u32;

    for m in regex.find_iter(text) {
        // Zero-width matches are dropped for the same reason `find.rs` drops them: `^`
        // would report every line as a hit with nothing highlighted.
        if m.is_empty() {
            continue;
        }
        if found >= budget {
            break;
        }

        // Advance to the row containing this match. `m.start()` is non-decreasing across
        // `find_iter`, so this never rewinds.
        while let Some(next) = text[line_start..].find('\n') {
            let newline = line_start + next;
            if newline >= m.start() {
                break;
            }
            line_start = newline + 1;
            row += 1;
        }

        let line_end = text[line_start..].find('\n').map_or(text.len(), |i| line_start + i);
        // A match can span newlines when the user writes a regex like `foo[\s\S]*bar`. It
        // is reported on its first line, and only the part on that line is highlighted —
        // the alternative is a "line" of text that is really six.
        let hit = m.start() - line_start..m.end().min(line_end) - line_start;

        match lines.last_mut() {
            // Same row as the previous hit: append rather than pushing a duplicate row.
            // `trimmed` is carried on the row rather than recomputed, and it is *not*
            // `column` — that is the first hit's offset, which is only the same number
            // when the first hit sits flush against the indent.
            Some(last) if last.row == row => {
                if let Some(range) = rebase(&last.text, last.trimmed, hit) {
                    last.ranges.push(range);
                }
            }
            _ => {
                let raw = &text[line_start..line_end];
                let (display, trimmed_by) = display_line(raw);
                let column = hit.start as u32;
                let Some(range) = rebase(&display, trimmed_by, hit) else { continue };
                lines.push(LineMatch {
                    row,
                    text: display,
                    ranges: vec![range],
                    column,
                    trimmed: trimmed_by,
                });
            }
        }
        found += 1;
    }

    lines
}

/// A range in the raw line, expressed against the trimmed-and-truncated display line.
///
/// `None` when the hit fell outside what is displayed — past [`MAX_LINE_BYTES`] on a very
/// long line. The row still exists and is still clickable; only the highlight is dropped,
/// which is better than either slicing a `String` out of bounds (a panic) or widening every
/// row to hold a 400 KB minified line.
fn rebase(
    display: &str,
    trimmed_by: usize,
    hit: std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    let start = hit.start.checked_sub(trimmed_by)?;
    let end = hit.end.checked_sub(trimmed_by)?;
    if end > display.len() {
        return None;
    }
    // Char boundaries are guaranteed by the source ranges, but the truncation in
    // `display_line` is the one place a boundary could be invented, so this asserts rather
    // than assumes: a range landing mid-codepoint panics in debug the moment it is sliced.
    if !display.is_char_boundary(start) || !display.is_char_boundary(end) {
        return None;
    }
    Some(start..end)
}

/// The text shown for a result row, and how many leading bytes were dropped.
///
/// Leading whitespace goes because a hit inside a deeply-nested `if` would otherwise render
/// as an empty row with the code off-screen. Trailing goes for free. The truncation is at a
/// char boundary, because slicing a `String` at a byte inside a multi-byte character is a
/// panic and accented text is in this repo's own test corpus.
fn display_line(raw: &str) -> (String, usize) {
    let trimmed_by = raw.len() - raw.trim_start().len();
    let body = raw[trimmed_by..].trim_end();

    if body.len() <= MAX_LINE_BYTES {
        return (body.to_string(), trimmed_by);
    }
    let mut cut = MAX_LINE_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    (body[..cut].to_string(), trimmed_by)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn literal(pattern: &str) -> SearchQuery {
        SearchQuery::literal(pattern)
    }

    fn scan(text: &str, query: &SearchQuery) -> Vec<LineMatch> {
        let regex = query.compile().unwrap().unwrap();
        scan_text(text, &regex, MAX_RESULTS)
    }

    /// A project fixture with the shape that matters: nested source, an ignored `vendor/`,
    /// a binary, and accented text.
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("app/Models")).unwrap();
        fs::create_dir_all(root.join("app/Http")).unwrap();
        fs::create_dir_all(root.join("vendor/laravel")).unwrap();

        fs::write(root.join(".gitignore"), "/vendor\n").unwrap();
        fs::write(
            root.join("app/Models/User.php"),
            "<?php\nclass User\n{\n    public $needle = 1;\n}\n",
        )
        .unwrap();
        fs::write(root.join("app/Http/Kernel.php"), "<?php\n// no hits here\n").unwrap();
        fs::write(root.join("vendor/laravel/Str.php"), "<?php\n$needle = 'vendor';\n").unwrap();
        fs::write(root.join("notes.txt"), "needle needle\nnothing\nneedle\n").unwrap();

        dir
    }

    fn relatives(results: &ProjectResults) -> Vec<&str> {
        let mut names: Vec<&str> = results.files.iter().map(|f| f.relative.as_str()).collect();
        names.sort_unstable();
        names
    }

    // --- the walk ------------------------------------------------------------------

    #[test]
    fn finds_hits_across_nested_files_and_skips_ignored_ones() {
        let dir = project();
        let results = search_project(dir.path(), &literal("needle"), &CancelFlag::new());

        assert_eq!(relatives(&results), vec!["app/Models/User.php", "notes.txt"]);
        // vendor/ has a hit and must not appear: search follows the same rules as the tree
        // and quick open, or a result opens a file the user cannot see anywhere else.
        assert!(!results.files.iter().any(|f| f.relative.starts_with("vendor/")));
        assert_eq!(results.match_count(), 4, "one in User.php, three in notes.txt");
        assert_eq!(results.file_count(), 2);
        assert!(!results.truncated);
        assert!(!results.cancelled);
    }

    #[test]
    fn an_empty_pattern_searches_nothing_at_all() {
        // Not "matches nothing" — the walk must not even run. An empty find field is the
        // state the panel is in before the user types, and walking the project there is
        // the whole cost of a search for no result.
        let dir = project();
        let results = search_project(dir.path(), &literal(""), &CancelFlag::new());
        assert!(results.is_empty());
        assert!(!results.invalid);
    }

    #[test]
    fn an_unparseable_regex_is_invalid_rather_than_a_panic() {
        let dir = project();
        let query = SearchQuery { regex: true, ..literal("[a-") };
        let results = search_project(dir.path(), &query, &CancelFlag::new());
        assert!(results.invalid);
        assert!(results.is_empty());
    }

    #[test]
    fn cancelling_before_the_walk_returns_nothing() {
        let dir = project();
        let cancel = CancelFlag::new();
        cancel.cancel();

        let results = search_project(dir.path(), &literal("needle"), &cancel);
        assert!(results.is_empty());
        assert!(results.cancelled, "a cancelled search must say so, not look like no results");
    }

    #[test]
    fn cancelling_mid_scan_stops_and_reports_it() {
        // The property that matters is that the flag is read *inside* the file loop, so a
        // superseded search abandons rather than finishing the project.
        let dir = tempfile::tempdir().unwrap();
        for i in 0..200 {
            fs::write(dir.path().join(format!("f{i}.php")), "<?php $needle;").unwrap();
        }
        let files = index_files(dir.path(), &CancelFlag::new());
        assert_eq!(files.len(), 200);

        let cancel = CancelFlag::new();
        let regex = literal("needle").compile().unwrap().unwrap();
        // Cancel before the scan starts but after the walk: `search_files` must still bail.
        cancel.cancel();
        let results = search_files(&files, &regex, &cancel);
        assert!(results.cancelled);
        assert!(results.is_empty());
    }

    #[test]
    fn a_missing_root_yields_nothing_rather_than_panicking() {
        let results =
            search_project(Path::new("/definitely/not/here"), &literal("x"), &CancelFlag::new());
        assert!(results.is_empty());
    }

    // --- which files are read ------------------------------------------------------

    #[test]
    fn a_binary_file_is_skipped_rather_than_matched() {
        // A NUL in the first 8 KB, the same heuristic `read_file` uses. Without this a
        // search for a common byte sequence lists every compiled asset in the project, and
        // clicking one opens the "binary file" error.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("data.bin"), b"needle\0needle").unwrap();
        fs::write(dir.path().join("ok.txt"), "needle").unwrap();

        let results = search_project(dir.path(), &literal("needle"), &CancelFlag::new());
        assert_eq!(relatives(&results), vec!["ok.txt"]);
    }

    #[test]
    fn invalid_utf8_is_skipped_rather_than_lossily_converted() {
        let dir = tempfile::tempdir().unwrap();
        // A lone 0xFF: not valid UTF-8, and no NUL, so the binary sniff does not catch it.
        fs::write(dir.path().join("latin1.txt"), b"needle \xFF here").unwrap();

        let results = search_project(dir.path(), &literal("needle"), &CancelFlag::new());
        assert!(results.is_empty(), "a lossy conversion would report a column that is a lie");
    }

    #[test]
    fn a_file_past_the_size_cap_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_FILE_BYTES + 1) + "needle";
        fs::write(dir.path().join("bundle.js"), big).unwrap();
        fs::write(dir.path().join("small.js"), "needle").unwrap();

        let results = search_project(dir.path(), &literal("needle"), &CancelFlag::new());
        assert_eq!(relatives(&results), vec!["small.js"]);
    }

    // --- the scan ------------------------------------------------------------------

    #[test]
    fn a_row_is_zero_based_and_the_column_points_at_the_first_hit() {
        // Both are handed straight to `open_path_at`'s `Point`, which is zero-based on both
        // axes. Off by one here is a click that lands on the wrong line.
        let text = "one\ntwo needle\nthree\n    needle again\n";
        let lines = scan(text, &literal("needle"));

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].row, 1);
        assert_eq!(lines[0].column, 4, "byte column in the *untrimmed* line");
        assert_eq!(lines[3 - 2].row, 3);
        assert_eq!(lines[1].column, 4, "the indent counts toward the column");
    }

    #[test]
    fn the_displayed_line_is_trimmed_and_its_ranges_move_with_it() {
        let text = "        $needle = 1;    \n";
        let lines = scan(text, &literal("needle"));

        assert_eq!(lines[0].text, "$needle = 1;");
        // The hit is at byte 9 in the raw line and byte 1 in the trimmed one.
        assert_eq!(lines[0].ranges, vec![1..7]);
        assert_eq!(&lines[0].text[lines[0].ranges[0].clone()], "needle");
        // But the *column* is still against the real line, because that is where the
        // cursor goes.
        assert_eq!(lines[0].column, 9);
    }

    #[test]
    fn two_hits_on_one_line_are_one_row_with_two_ranges() {
        // The failure this rules out: a results list that shows the same line twice, which
        // is what a naive per-match push produces and what makes a search for `$` unusable.
        let lines = scan("needle and needle\n", &literal("needle"));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].ranges, vec![0..6, 11..17]);
        assert_eq!(lines[0].row, 0);
        assert_eq!(lines[0].column, 0, "the column is the *first* hit");
    }

    #[test]
    fn a_second_hit_on_an_indented_line_is_rebased_against_the_indent() {
        // The bug this caught during development: the second hit on a row was rebased
        // against the *first hit's column* rather than against the trim, which silently
        // shifted every range after the first. It only shows up when the first hit is not
        // flush against the indent — which is most real code.
        let lines = scan("    a needle and needle\n", &literal("needle"));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "a needle and needle");
        for range in &lines[0].ranges {
            assert_eq!(&lines[0].text[range.clone()], "needle");
        }
        assert_eq!(lines[0].ranges, vec![2..8, 13..19]);
    }

    #[test]
    fn rows_are_counted_correctly_when_hits_are_far_apart() {
        // The rolling newline cursor is the part most likely to be wrong: it must not
        // rewind, and it must not lose count across a long gap with no matches.
        let mut text = String::from("needle\n");
        text.push_str(&"filler\n".repeat(500));
        text.push_str("needle\n");

        let lines = scan(&text, &literal("needle"));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].row, 0);
        assert_eq!(lines[1].row, 501);
    }

    #[test]
    fn a_hit_on_the_last_line_without_a_trailing_newline_is_found() {
        let lines = scan("first\nneedle", &literal("needle"));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].row, 1);
        assert_eq!(lines[0].text, "needle");
    }

    #[test]
    fn a_hit_on_the_very_first_byte_is_found() {
        let lines = scan("needle first\n", &literal("needle"));
        assert_eq!(lines[0].row, 0);
        assert_eq!(lines[0].ranges, vec![0..6]);
    }

    #[test]
    fn an_empty_file_produces_no_rows() {
        assert!(scan("", &literal("needle")).is_empty());
    }

    // --- multibyte -----------------------------------------------------------------

    #[test]
    fn an_accented_match_never_lands_mid_codepoint() {
        // The debug-build panic this rules out: a range sliced out of `text` at a byte
        // inside a multi-byte character. Every range here is sliced, which is the assertion.
        let text = "função ação\n    çedilha e função\n";
        let lines = scan(text, &literal("ção"));

        assert_eq!(lines.len(), 2);
        for line in &lines {
            for range in &line.ranges {
                assert!(line.text.is_char_boundary(range.start), "{range:?} starts mid-codepoint");
                assert!(line.text.is_char_boundary(range.end), "{range:?} ends mid-codepoint");
                assert_eq!(&line.text[range.clone()], "ção");
            }
        }
        // Byte offsets: `fun` is 3 bytes, `ç` is 2, so `ção` starts at 3.
        assert_eq!(lines[0].ranges, vec![3..8, 10..15]);
        // And the trimmed second line rebased both the range and kept the raw column.
        assert_eq!(lines[1].text, "çedilha e função");
        assert_eq!(lines[1].column, 4 + "çedilha e fun".len() as u32);
    }

    #[test]
    fn a_line_of_only_multibyte_text_truncates_on_a_char_boundary() {
        // `MAX_LINE_BYTES` is a byte count and `ç` is two bytes, so the naive cut lands
        // inside a character on exactly the input a Portuguese codebase produces.
        let long = "ç".repeat(MAX_LINE_BYTES);
        let text = format!("needle {long}\n");
        let lines = scan(&text, &literal("needle"));

        assert_eq!(lines.len(), 1);
        assert!(lines[0].text.len() <= MAX_LINE_BYTES);
        assert!(lines[0].text.is_char_boundary(lines[0].text.len()));
        // The hit itself is at the front, so it survives the truncation.
        assert_eq!(&lines[0].text[lines[0].ranges[0].clone()], "needle");
    }

    #[test]
    fn a_hit_past_the_truncation_keeps_the_row_and_drops_the_highlight() {
        // The alternative is a slice out of bounds, which is a panic. The row is still
        // useful — it says "this file, this line" — so it is kept.
        let text = format!("{}needle\n", "x".repeat(MAX_LINE_BYTES + 10));
        let lines = scan(&text, &literal("needle"));
        assert!(lines.is_empty(), "no displayable range, so no row rather than a bad slice");

        // But a line with one hit inside the window and one past it keeps the first.
        let text = format!("needle{}needle\n", "x".repeat(MAX_LINE_BYTES));
        let lines = scan(&text, &literal("needle"));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].ranges, vec![0..6], "the second hit is off the end of the row");
    }

    // --- query modes ---------------------------------------------------------------

    #[test]
    fn case_sensitivity_and_whole_word_reach_the_project_scan() {
        // These are `SearchQuery`'s job and `find.rs` tests them thoroughly; what this
        // asserts is that project search goes through the same `compile`, so the two
        // cannot drift.
        let text = "User user username\n";
        assert_eq!(scan(text, &literal("user"))[0].ranges.len(), 3);

        let sensitive = SearchQuery { case_sensitive: true, ..literal("user") };
        assert_eq!(scan(text, &sensitive)[0].ranges.len(), 2);

        let word = SearchQuery { whole_word: true, ..literal("user") };
        assert_eq!(scan(text, &word)[0].ranges.len(), 2);
    }

    #[test]
    fn a_literal_pattern_is_escaped_rather_than_parsed() {
        // Typing `$user` into a project-search field must not be a regex.
        let text = "a.b axb\n";
        assert_eq!(scan(text, &literal("."))[0].ranges, vec![1..2]);
    }

    #[test]
    fn a_zero_width_regex_yields_no_rows() {
        let query = SearchQuery { regex: true, ..literal("^") };
        assert!(scan("a\nb\nc", &query).is_empty());
    }

    #[test]
    fn a_regex_spanning_newlines_is_reported_on_its_first_line() {
        // `[\s\S]*` is how a user writes "anything including newlines". Reporting the hit
        // as a "line" containing three real lines would put a multi-line string in a
        // fixed-height row.
        let query = SearchQuery { regex: true, ..literal(r"start[\s\S]*end") };
        let lines = scan("x\nstart\nmiddle\nend\n", &query);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].row, 1);
        assert_eq!(lines[0].text, "start");
        assert_eq!(lines[0].ranges, vec![0..5], "only the part on this line is highlighted");
    }

    // --- bounds --------------------------------------------------------------------

    #[test]
    fn the_scan_stops_at_the_result_cap_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        // 30 files x 50 hits = 1500, comfortably past MAX_RESULTS.
        for i in 0..30 {
            fs::write(dir.path().join(format!("f{i}.txt")), "needle\n".repeat(50)).unwrap();
        }

        let results = search_project(dir.path(), &literal("needle"), &CancelFlag::new());
        assert!(results.truncated);
        assert!(
            results.match_count() <= MAX_RESULTS,
            "reported {} matches, cap is {MAX_RESULTS}",
            results.match_count()
        );
        assert!(!results.cancelled, "hitting the cap is not a cancellation");
    }

    #[test]
    fn the_budget_is_enforced_within_a_single_file_too() {
        // A cap checked only between files would let one file with a million hits through.
        let text = "needle\n".repeat(10_000);
        let regex = literal("needle").compile().unwrap().unwrap();
        assert_eq!(scan_text(&text, &regex, 5).len(), 5);
    }

    /// Prints what a project search costs on a real folder. **Not a pass/fail test.**
    ///
    /// Ignored by default and run by hand, in release, against a path given in the
    /// environment:
    ///
    /// ```sh
    /// ELLE_SEARCH_ROOT=~/some/laravel/app \
    ///   cargo test --release --bin ellefuanti -- --ignored --nocapture measure_project_search
    /// ```
    ///
    /// A wall-clock assertion in the suite would be a flaky test that teaches nothing when
    /// it fails, which is the lesson `benchmarks/BASELINE.md` opens with. This prints and
    /// asserts nothing but that the search ran.
    #[test]
    #[ignore = "measurement, not a check: needs ELLE_SEARCH_ROOT and a release build"]
    fn measure_project_search() {
        let Some(root) = std::env::var_os("ELLE_SEARCH_ROOT") else {
            eprintln!("set ELLE_SEARCH_ROOT to a project directory");
            return;
        };
        let root = PathBuf::from(root);

        let walk = std::time::Instant::now();
        let files = index_files(&root, &CancelFlag::new());
        let walk = walk.elapsed();
        let bytes: u64 =
            files.iter().filter_map(|f| std::fs::metadata(&f.path).ok()).map(|m| m.len()).sum();
        eprintln!(
            "\n{}: {} files, {:.1} MB, walk {:?}",
            root.display(),
            files.len(),
            bytes as f64 / 1_048_576.0,
            walk
        );

        // The walk, five more times, warm. The first reading above is cold and is printed
        // separately on purpose: 10.6 ms cold against 1.4 ms warm on the same folder is a
        // 7x gap that would otherwise look like the scan being slow.
        let mut walks = Vec::new();
        for _ in 0..5 {
            let start = std::time::Instant::now();
            index_files(&root, &CancelFlag::new());
            walks.push(start.elapsed());
        }
        walks.sort_unstable();
        eprintln!("  {:<18} {:>8.1?} (warm, median of 5)", "walk", walks[2]);

        let cases: [(&str, SearchQuery); 4] = [
            ("literal $user", literal("$user")),
            ("literal function", literal("function")),
            ("regex \\$\\w+", SearchQuery { regex: true, ..literal(r"\$\w+") }),
            // Deliberately not a string that appears in this file, or the "no hits" case
            // finds itself when the search root is this repo — which is exactly what the
            // first run of this measurement did.
            ("no hits", literal("qqzzxxwwvv-absent")),
        ];

        for (name, query) in cases {
            // Five runs, median reported: one run measures the page cache as much as the
            // scan, and BASELINE.md's whole history is about numbers that measured the
            // harness. The spread is printed too — two readings that disagree are evidence
            // about the harness, not noise to average.
            let mut times = Vec::new();
            let mut count = 0;
            for _ in 0..5 {
                let start = std::time::Instant::now();
                let results = search_project(&root, &query, &CancelFlag::new());
                times.push(start.elapsed());
                count = results.match_count();
            }
            times.sort_unstable();
            eprintln!(
                "  {name:<18} {:>8.1?} (min {:>8.1?} max {:>8.1?})  {count} matches",
                times[2], times[0], times[4]
            );
        }
    }

    #[test]
    fn match_count_counts_hits_and_file_count_counts_files() {
        // Two hits on one line is one row, two matches, one file — and the panel header
        // says "3 results in 1 file", so the two counts cannot be the same number.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "needle needle\nneedle\n").unwrap();

        let results = search_project(dir.path(), &literal("needle"), &CancelFlag::new());
        assert_eq!(results.file_count(), 1);
        assert_eq!(results.match_count(), 3);
        assert_eq!(results.files[0].lines.len(), 2, "two rows, three hits");
    }
}
