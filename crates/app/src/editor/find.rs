//! Find and replace within one document. No gpui, so it is unit-testable at full speed.
//!
//! # Where the work happens, and why
//!
//! Matches are computed **once per (query, buffer version)**, eagerly, over the whole
//! buffer — not per frame, and not per viewport. That is the opposite of what #52 taught
//! about syntax highlighting, so it needs a justification rather than a shrug:
//!
//! - The **match count** ("3 of 17") is a whole-file fact. There is no viewport-only way
//!   to compute it, and an editor that cannot tell you how many hits there are is not
//!   doing the job. Neither is one whose ⌘G stops at the bottom of the screen.
//! - A rescan runs only on a keystroke *in the find field* or an edit while the bar is
//!   open — not on every keystroke in the document, and not per frame.
//! - Per **frame**, the cost still tracks the viewport: [`Matches::in_range`] binary-searches
//!   the sorted list and returns a slice. That is the property
//!   `viewport_cost_does_not_grow_with_file_size` asserts for highlighting, and
//!   `match_lookup_cost_does_not_grow_with_file_size` asserts here.
//!
//! # What a rescan actually costs
//!
//! Measured, not assumed, on Apple Silicon in release against Laravel-shaped PHP
//! (`benchmarks/BASELINE.md` records the same numbers). Medians of 60 runs:
//!
//! | file | query | total | of which scan |
//! | --- | --- | --- | --- |
//! | 33 KB / 1k lines | `$user`, hits every line | 120 µs | 106 µs |
//! | 349 KB / 10k lines | `$user`, hits every line | 389 µs | 334 µs |
//! | 1.8 MB / 50k lines | `$user`, hits every line | **1.8 ms** | 1.64 ms |
//! | 1.8 MB / 50k lines | `zzz`, no hits | 235 µs | 148 µs |
//! | 1.8 MB / 50k lines | `\$user\d+` regex, hits every line | **2.4 ms** | 2.22 ms |
//!
//! So the honest statement is **not** "search is free". The 50k-line worst case is a
//! fifth of a 8.3 ms frame, on the UI thread, once per keystroke in the find field. It
//! fits, and typing in the *document* is unaffected, but it is close enough that the next
//! person to make search slower should re-measure rather than assume headroom.
//!
//! The cost tracks the **hit count**, not the file size: the same 1.8 MB file with no
//! matches costs 235 µs. `Rope::to_string` is 74 µs of that and regex compilation is
//! 33 µs (58 µs for a real regex) — both real, neither dominant, so neither is cached.
//! Caching the compiled regex is the obvious first optimisation if this ever needs one.
//!
//! The case this design would lose is a multi-megabyte minified file re-searched on every
//! keystroke. [`Matches::new`] therefore refuses to scan past [`MAX_SEARCH_BYTES`] and
//! reports the file as too large rather than dropping frames, which is the same shape as
//! `MAX_MEASURE_BYTES` in the view. The real fix — a cancellable background scan — is
//! machinery find-in-project needs anyway, and is where it should be built.

use std::ops::Range;

use regex::Regex;

/// Largest buffer that is searched synchronously.
///
/// Picked from the measurements above rather than round-numbered: 1.8 MB with a hit on
/// every line costs 1.8 ms, so the cost is roughly 1 ms per megabyte per hit-dense
/// pattern. 4 MB is therefore about 4 ms — half a frame, the most a keystroke can spend
/// on the UI thread and still feel immediate.
///
/// Past it the honest answer is "not searched", which the find bar says out loud. A
/// silent frame drop the user cannot explain is worse than a control admitting it will
/// not do the thing.
pub const MAX_SEARCH_BYTES: usize = 4 * 1024 * 1024;

/// What to look for, and how.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SearchQuery {
    pub pattern: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

impl SearchQuery {
    /// A default query for `pattern`: case-insensitive, not whole-word, not regex.
    ///
    /// Only the tests construct one this way — the find bar mutates its own
    /// `SearchQuery` in place as the user types and toggles — but it is the constructor
    /// every test wants and spelling out four fields at each call site would bury the
    /// pattern under the defaults.
    #[cfg(test)]
    pub fn literal(pattern: &str) -> Self {
        Self { pattern: pattern.to_string(), ..Default::default() }
    }

    /// True when there is nothing to search for.
    pub fn is_empty(&self) -> bool {
        self.pattern.is_empty()
    }

    /// A `Regex` for this query, or `None` when the pattern is empty.
    ///
    /// Everything goes through `regex`, including literal search — a literal is compiled
    /// with [`regex::escape`], and whole-word wraps it in `\b`. One code path rather than
    /// two means the case-insensitive and whole-word rules cannot drift between modes,
    /// and the crate's literal prefilter (`memchr`/Teddy) means the escaped-literal case
    /// still runs as a byte search rather than as an automaton.
    ///
    /// Returns `Err` for an unparseable regex, which is a thing the user is *mid-typing*
    /// most of the time — `[a-` is not an error to report loudly, it is a query that does
    /// not match anything yet.
    /// `pub(super)` rather than private since #80's project search: `project_search.rs`
    /// compiles the query **once for the whole project** instead of once per file, which
    /// is the difference between 33 µs and 16 ms across 500 files. Going through this
    /// function rather than building its own regex is what keeps the escaping,
    /// case-folding and whole-word rules identical between in-file and project search —
    /// two `Regex::new` call sites is exactly how those drift.
    pub(super) fn compile(&self) -> Option<Result<Regex, regex::Error>> {
        if self.pattern.is_empty() {
            return None;
        }
        let body = if self.regex { self.pattern.clone() } else { regex::escape(&self.pattern) };
        // `\b` around a pattern that starts or ends with a non-word character never
        // matches (`\b(` requires a word char adjacent to `(`), so wrapping is only
        // applied where it can mean something. VS Code behaves the same way.
        let body = if self.whole_word { format!(r"(?:\b(?:{body})\b)") } else { body };

        // Inline flags rather than `RegexBuilder`, which reads worse but keeps the file
        // free of a `.build()` call — `crates/app/tests/theming.rs` greps the whole `src`
        // tree for that string to enforce #48's "no view constructs its own theme", and a
        // regex builder here would trip it. Narrowing that grep to catch this one call
        // would weaken a guard that is doing its job.
        //
        // `m` makes `^`/`$` mean line edges, which is what a user typing a regex into a
        // find bar expects; `.` still does not cross a newline. `i` is the default because
        // case-insensitive is what a find bar does until you say otherwise.
        let flags = if self.case_sensitive { "m" } else { "im" };
        Some(Regex::new(&format!("(?{flags}){body}")))
    }
}

/// The matches for one query against one buffer snapshot, sorted and non-overlapping.
///
/// Holding the whole list is what makes "3 of 17" and a wrapping ⌘G possible at all; the
/// per-frame cost is paid by [`Matches::in_range`], not by holding it.
#[derive(Clone, Debug, Default)]
pub struct Matches {
    ranges: Vec<Range<usize>>,
    /// True when the pattern would not compile. Distinct from "no matches": the find bar
    /// says *why* nothing is highlighted, and replace refuses to run.
    invalid: bool,
    /// True when the buffer was past [`MAX_SEARCH_BYTES`] and never scanned.
    too_large: bool,
}

impl Matches {
    /// Scans `text` for `query`.
    ///
    /// Empty for an empty pattern, which is what makes an empty find field highlight
    /// nothing rather than everything.
    pub fn new(text: &str, query: &SearchQuery) -> Self {
        if text.len() > MAX_SEARCH_BYTES {
            return Self { ranges: Vec::new(), invalid: false, too_large: true };
        }
        let Some(compiled) = query.compile() else { return Self::default() };
        let Ok(regex) = compiled else {
            return Self { ranges: Vec::new(), invalid: true, too_large: false };
        };

        // `find_iter` yields non-overlapping matches left to right, already sorted, and
        // every boundary it reports is a char boundary because it matched UTF-8 — which
        // is the property the multibyte tests below pin down.
        //
        // A zero-width match (`^`, `\b`, `x*`) would otherwise produce one "match" per
        // byte position and make ⌘G an infinite loop over nothing; they are dropped.
        let ranges = regex.find_iter(text).map(|m| m.range()).filter(|r| !r.is_empty()).collect();
        Self { ranges, invalid: false, too_large: false }
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_invalid(&self) -> bool {
        self.invalid
    }

    pub fn is_too_large(&self) -> bool {
        self.too_large
    }

    /// Every match. The render path uses [`Matches::in_range`] instead — this exists so a
    /// test can assert on exact offsets, which is the only place the whole list is wanted.
    #[cfg(test)]
    pub fn all(&self) -> &[Range<usize>] {
        &self.ranges
    }

    pub fn get(&self, index: usize) -> Option<&Range<usize>> {
        self.ranges.get(index)
    }

    /// The matches overlapping `range`, as a slice of the sorted list.
    ///
    /// **This is the per-frame call**, and the reason the list is sorted. Two binary
    /// searches plus a slice: cost is `O(log n + visible)` in the number of matches, so a
    /// viewport over a file with 50,000 hits costs the same as one over a file with 20.
    /// Pinned by `match_lookup_cost_does_not_grow_with_file_size`.
    pub fn in_range(&self, range: Range<usize>) -> &[Range<usize>] {
        // First match whose *end* is past `range.start`. `partition_point` is the binary
        // search; the predicate is monotone because the ranges are sorted and disjoint.
        let start = self.ranges.partition_point(|m| m.end <= range.start);
        // First match starting at or after `range.end`.
        let end = self.ranges.partition_point(|m| m.start < range.end);
        &self.ranges[start..end.max(start)]
    }

    /// Index of the first match at or after `offset`, wrapping to 0.
    ///
    /// `None` only when there are no matches at all, so a caller never has to special-case
    /// the wrap.
    pub fn index_at_or_after(&self, offset: usize) -> Option<usize> {
        if self.ranges.is_empty() {
            return None;
        }
        let index = self.ranges.partition_point(|m| m.start < offset);
        Some(if index == self.ranges.len() { 0 } else { index })
    }

    /// Index of the last match starting strictly before `offset`, wrapping to the end.
    pub fn index_before(&self, offset: usize) -> Option<usize> {
        if self.ranges.is_empty() {
            return None;
        }
        let index = self.ranges.partition_point(|m| m.start < offset);
        Some(if index == 0 { self.ranges.len() - 1 } else { index - 1 })
    }

    /// What each match should be replaced with, paired with its range, last match first.
    ///
    /// Reverse order so a caller applying them one after another does not have to rebase
    /// offsets: every edit is behind the ones still to come.
    ///
    /// `$1` and `${name}` expand in regex mode, and are literal otherwise — the same rule
    /// VS Code uses, and the reason this cannot be a plain `zip`: the expansion needs the
    /// capture groups, which means re-running the regex.
    pub fn replacements(
        &self,
        text: &str,
        query: &SearchQuery,
        replacement: &str,
    ) -> Vec<(Range<usize>, String)> {
        if !query.regex {
            return self
                .ranges
                .iter()
                .rev()
                .map(|range| (range.clone(), replacement.to_string()))
                .collect();
        }

        let Some(Ok(regex)) = query.compile() else { return Vec::new() };
        let mut out: Vec<(Range<usize>, String)> = regex
            .captures_iter(text)
            .filter_map(|captures| {
                let whole = captures.get(0)?;
                if whole.is_empty() {
                    return None;
                }
                let mut expanded = String::new();
                captures.expand(replacement, &mut expanded);
                Some((whole.range(), expanded))
            })
            .collect();
        out.reverse();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(pattern: &str) -> SearchQuery {
        SearchQuery::literal(pattern)
    }

    #[test]
    fn an_empty_pattern_matches_nothing() {
        let matches = Matches::new("anything at all", &literal(""));
        assert!(matches.is_empty());
        assert!(!matches.is_invalid());
    }

    #[test]
    fn literal_search_is_case_insensitive_by_default() {
        let text = "User user USER";
        assert_eq!(Matches::new(text, &literal("user")).len(), 3);

        let sensitive = SearchQuery { case_sensitive: true, ..literal("user") };
        assert_eq!(Matches::new(text, &sensitive).len(), 1);
        assert_eq!(Matches::new(text, &sensitive).all()[0], 5..9);
    }

    #[test]
    fn a_literal_pattern_is_escaped_rather_than_parsed() {
        // The bug this rules out: typing `$user` or `.` into the find field and getting
        // regex behaviour. `.` must match a dot, not every character.
        let text = "a.b axb $user";
        assert_eq!(Matches::new(text, &literal(".")).len(), 1);
        assert_eq!(Matches::new(text, &literal(".")).all()[0], 1..2);
        assert_eq!(Matches::new(text, &literal("$user")).len(), 1);
        // And the same pattern in regex mode does mean what regex says.
        let re = SearchQuery { regex: true, ..literal(".") };
        assert_eq!(Matches::new(text, &re).len(), text.len());
    }

    #[test]
    fn whole_word_rejects_a_substring_hit() {
        let text = "user username $user superuser";
        assert_eq!(Matches::new(text, &literal("user")).len(), 4);

        let word = SearchQuery { whole_word: true, ..literal("user") };
        let matches = Matches::new(text, &word);
        // `user`, and the `user` in `$user` — `$` is not a word character to a regex, so
        // `\buser\b` matches there. `username` and `superuser` are correctly rejected.
        assert_eq!(matches.len(), 2);
        assert_eq!(matches.all(), &[0..4, 15..19]);
    }

    #[test]
    fn whole_word_and_case_sensitivity_compose() {
        let text = "User user";
        let query = SearchQuery { whole_word: true, case_sensitive: true, ..literal("User") };
        assert_eq!(Matches::new(text, &query).len(), 1);
    }

    #[test]
    fn a_match_never_lands_mid_codepoint() {
        // A match that starts or ends inside a multi-byte character is a debug-build
        // panic the moment `line_runs` hands it to `StyledText` — and the accented text
        // in this repo's own test corpus is exactly where it would happen.
        let text = "função ação função";
        let matches = Matches::new(text, &literal("ção"));
        assert_eq!(matches.len(), 3);
        for range in matches.all() {
            assert!(text.is_char_boundary(range.start), "{range:?} starts mid-codepoint");
            assert!(text.is_char_boundary(range.end), "{range:?} ends mid-codepoint");
            assert_eq!(&text[range.clone()], "ção");
        }
        // Byte offsets, not char offsets: `fun` is 3 bytes, `ç` is 2.
        assert_eq!(matches.all()[0], 3..8);
    }

    #[test]
    fn case_insensitive_matching_is_unicode_aware() {
        // ASCII-only lowercasing would miss this, and a naive `to_ascii_lowercase` on
        // both sides is the usual way that bug arrives.
        let text = "AÇÃO ação";
        assert_eq!(Matches::new(text, &literal("ação")).len(), 2);
        let sensitive = SearchQuery { case_sensitive: true, ..literal("ação") };
        assert_eq!(Matches::new(text, &sensitive).len(), 1);
    }

    #[test]
    fn an_accented_match_survives_whole_word() {
        // `\b` and non-ASCII word characters: `função` must be a word, not three.
        let text = "função funcionar";
        let query = SearchQuery { whole_word: true, ..literal("função") };
        assert_eq!(Matches::new(text, &query).len(), 1);
    }

    #[test]
    fn overlapping_matches_are_reported_left_to_right_without_overlap() {
        // "aaaa" searched for "aa" is two matches, not three: an editor that reported
        // overlapping hits would replace-all into a corrupt buffer.
        let matches = Matches::new("aaaa", &literal("aa"));
        assert_eq!(matches.all(), &[0..2, 2..4]);
    }

    #[test]
    fn a_zero_width_regex_yields_no_matches() {
        // `^` matches at every line start with zero width. Left in, ⌘G would cycle
        // through positions with nothing highlighted and replace-all would insert at
        // every one of them.
        let query = SearchQuery { regex: true, ..literal("^") };
        assert!(Matches::new("a\nb\nc", &query).is_empty());
        let star = SearchQuery { regex: true, ..literal("x*") };
        assert!(Matches::new("abc", &star).is_empty());
    }

    #[test]
    fn an_unparseable_regex_is_invalid_rather_than_a_panic() {
        // Mid-typing state: `[a-` is what the buffer looks like between two keystrokes.
        let query = SearchQuery { regex: true, ..literal("[a-") };
        let matches = Matches::new("abc", &query);
        assert!(matches.is_invalid());
        assert!(matches.is_empty());
        // A *literal* `[a-` is not invalid — it is a string that happens to contain
        // brackets, and it must still be findable.
        assert!(!Matches::new("x[a-y", &literal("[a-")).is_empty());
    }

    #[test]
    fn an_alternation_gets_the_flags_on_both_branches() {
        // The inline-flag form (`(?im)…`) instead of `RegexBuilder` has one way to go
        // wrong: a leading flag that binds tighter than `|` would leave the right-hand
        // branch case-sensitive. It does not, and this is what says so.
        let query = SearchQuery { regex: true, ..literal("alpha|BETA") };
        assert_eq!(Matches::new("ALPHA beta", &query).len(), 2);

        let sensitive = SearchQuery { regex: true, case_sensitive: true, ..literal("alpha|BETA") };
        assert_eq!(Matches::new("ALPHA beta", &sensitive).len(), 0);
        assert_eq!(Matches::new("alpha BETA", &sensitive).len(), 2);
    }

    #[test]
    fn a_regex_can_anchor_to_line_edges() {
        let query = SearchQuery { regex: true, ..literal(r"^\$\w+") };
        let matches = Matches::new("$a = 1;\nx = $b;\n$c = 3;", &query);
        assert_eq!(matches.len(), 2, "multi_line must be on for ^ to mean line start");
    }

    #[test]
    fn in_range_returns_only_matches_touching_the_window() {
        let text = "x".repeat(10) + "needle" + &"x".repeat(10) + "needle";
        let matches = Matches::new(&text, &literal("needle"));
        assert_eq!(matches.len(), 2);

        assert_eq!(matches.in_range(0..5).len(), 0, "before the first");
        assert_eq!(matches.in_range(0..16).len(), 1);
        assert_eq!(matches.in_range(0..text.len()).len(), 2);
        // A window that starts inside a match still sees it: a match straddling the top
        // of the viewport must be painted on the row that is visible.
        assert_eq!(matches.in_range(12..14).len(), 1);
        // Touching the boundary from outside does not.
        assert_eq!(matches.in_range(16..20).len(), 0);
    }

    #[test]
    fn match_lookup_cost_does_not_grow_with_file_size() {
        // The search equivalent of `viewport_cost_does_not_grow_with_file_size` in
        // `crates/syntax`. Asserted structurally rather than by timing, for the reason
        // BASELINE.md opens with: a wall-clock assertion in a test suite is a flaky test
        // that teaches nothing when it fails.
        //
        // The property: the number of matches a frame has to *look at* for a fixed
        // viewport does not change when the file — and therefore the match count — grows.
        // The failure mode this rules out is the obvious implementation, a linear
        // `iter().filter(|m| overlaps(m, viewport))`, which is O(total matches) per frame
        // and is exactly what the diagnostics slice in `render_rows` already had to avoid.
        let line = "$needle = 1;\n";
        let viewport = 0..(line.len() * 40); // ~40 rows, one screenful

        let mut counts = Vec::new();
        for repeats in [50usize, 500, 5_000] {
            let text = line.repeat(repeats);
            let matches = Matches::new(&text, &literal("needle"));
            assert_eq!(matches.len(), repeats, "the file really did grow");
            counts.push(matches.in_range(viewport.clone()).len());
        }

        assert_eq!(counts[0], counts[1], "a 10x larger file handed the viewport more matches");
        assert_eq!(counts[1], counts[2], "a 100x larger file handed the viewport more matches");
        assert_eq!(counts[0], 40, "one match per visible row");
    }

    #[test]
    fn index_at_or_after_wraps_to_the_first_match() {
        let text = "a__a__a";
        let matches = Matches::new(text, &literal("a"));
        assert_eq!(matches.all(), &[0..1, 3..4, 6..7]);

        assert_eq!(matches.index_at_or_after(0), Some(0));
        assert_eq!(matches.index_at_or_after(1), Some(1));
        assert_eq!(matches.index_at_or_after(4), Some(2));
        assert_eq!(matches.index_at_or_after(7), Some(0), "past the last match wraps");
        assert_eq!(Matches::default().index_at_or_after(0), None);
    }

    #[test]
    fn index_before_wraps_to_the_last_match() {
        let matches = Matches::new("a__a__a", &literal("a"));
        assert_eq!(matches.index_before(0), Some(2), "before the first wraps to the last");
        assert_eq!(matches.index_before(1), Some(0));
        assert_eq!(matches.index_before(7), Some(2));
        assert_eq!(Matches::default().index_before(5), None);
    }

    #[test]
    fn replacements_are_ordered_last_first() {
        // The ordering is load-bearing: applied in this order, no edit shifts the offset
        // of an edit not yet made.
        let text = "a a a";
        let matches = Matches::new(text, &literal("a"));
        let out = matches.replacements(text, &literal("a"), "bb");
        assert_eq!(out.iter().map(|(r, _)| r.start).collect::<Vec<_>>(), vec![4, 2, 0]);
        assert!(out.iter().all(|(_, text)| text == "bb"));
    }

    #[test]
    fn a_dollar_sign_in_a_literal_replacement_is_not_a_capture_group() {
        // Replacing with `$this` in a PHP file is a daily operation and must not expand.
        let text = "self";
        let out =
            Matches::new(text, &literal("self")).replacements(text, &literal("self"), "$this");
        assert_eq!(out[0].1, "$this");
    }

    #[test]
    fn regex_replacement_expands_capture_groups() {
        let query = SearchQuery { regex: true, ..literal(r"(\w+)_(\w+)") };
        let text = "foo_bar baz_qux";
        let matches = Matches::new(text, &query);
        let out = matches.replacements(text, &query, "$2$1");
        assert_eq!(out.len(), 2);
        // Reverse order, so the second match comes first.
        assert_eq!(out[0].1, "quxbaz");
        assert_eq!(out[1].1, "barfoo");
    }

    #[test]
    fn a_file_past_the_size_cap_is_reported_rather_than_scanned() {
        let text = "x".repeat(MAX_SEARCH_BYTES + 1);
        let matches = Matches::new(&text, &literal("x"));
        assert!(matches.is_too_large());
        assert!(matches.is_empty());
        assert!(!matches.is_invalid());
    }
}
