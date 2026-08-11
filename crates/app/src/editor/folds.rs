//! Code folding's core: which lines are hidden, and the row↔line map (#82).
//!
//! The issue's own warning is the design constraint: `uniform_list` maps rows to buffer
//! lines 1:1 today, folding breaks that, and *"a wrong row mapping is the kind of bug
//! that corrupts edits"*. So the mapping lives here as pure, exhaustively-tested logic,
//! and the view converts exactly once, at the `uniform_list` boundary — each rendered
//! row is built with (and hands its mouse events) the buffer line it actually shows,
//! so `offset_at` and every consumer below it never see a folded row at all.
//!
//! What folds is decided by indentation (`foldable_block_at`), not the syntax tree —
//! deliberately: it is language-agnostic across all nine grammars, costs nothing to
//! compute, and is the same fallback VS Code ships. A tree-sitter fold query per
//! language is the upgrade path if indent folding misleads in practice.
//!
//! Survival across edits is the simplest rule that cannot corrupt: an edit that keeps
//! the buffer's line count is inside one line and leaves every range valid; an edit
//! that changes the line count clears all folds (`invalidate`). Adjusting ranges by
//! line deltas is the upgrade when clearing annoys someone — recorded, not built.

/// The hidden lines, as sorted disjoint ranges, and the map they imply.
#[derive(Default)]
pub struct Folds {
    /// Ranges of *hidden* buffer lines — never including the fold's header line, which
    /// stays visible as the thing you click to unfold.
    hidden: Vec<std::ops::Range<usize>>,
    /// The buffer's line count when the folds were made, for `invalidate`.
    lines_at_fold: usize,
}

impl Folds {
    /// Test-only observer; production reads go through the row map.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.hidden.is_empty()
    }

    /// Hides `lines`, merging with existing folds. The caller passes the *body* of a
    /// block — the header stays visible by construction.
    pub fn fold(&mut self, lines: std::ops::Range<usize>, buffer_lines: usize) {
        if lines.is_empty() {
            return;
        }
        self.hidden.push(lines);
        self.hidden.sort_by_key(|range| range.start);
        // Merge overlaps and adjacencies so the map below can binary-search.
        let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(self.hidden.len());
        for range in self.hidden.drain(..) {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        self.hidden = merged;
        self.lines_at_fold = buffer_lines;
    }

    /// Reveals the fold whose header is `line` — the fold that starts right below it.
    pub fn unfold_at_header(&mut self, header: usize) -> bool {
        let before = self.hidden.len();
        self.hidden.retain(|range| range.start != header + 1);
        before != self.hidden.len()
    }

    /// Reveals the fold containing `line`, if any. Returns whether anything changed.
    pub fn unfold_containing(&mut self, line: usize) -> bool {
        let before = self.hidden.len();
        self.hidden.retain(|range| !range.contains(&line));
        before != self.hidden.len()
    }

    pub fn clear(&mut self) {
        self.hidden.clear();
    }

    /// Drops every fold if the buffer's line count changed — the survival rule from the
    /// module doc. Call after any edit.
    pub fn invalidate(&mut self, buffer_lines: usize) {
        if !self.hidden.is_empty() && buffer_lines != self.lines_at_fold {
            self.hidden.clear();
        }
    }

    pub fn is_hidden(&self, line: usize) -> bool {
        self.hidden.iter().any(|range| range.contains(&line))
    }

    /// How many rows the list shows for a buffer of `buffer_lines` lines.
    pub fn visible_count(&self, buffer_lines: usize) -> usize {
        let hidden: usize = self.hidden.iter().map(|range| range.len()).sum();
        buffer_lines.saturating_sub(hidden)
    }

    /// The buffer line shown at `row`. Rows past the end clamp to the last line, which
    /// is what every caller wants at a boundary (`uniform_list` never asks past its
    /// count; clamping is belt for the callers that compute rows themselves).
    pub fn line_of_row(&self, row: usize, buffer_lines: usize) -> usize {
        let mut line = row;
        // Each hidden range whose start we have passed pushes the answer down by its
        // length. One pass over a short sorted list beats a cached Vec<usize> the size
        // of the file that would need rebuilding on every fold change.
        for range in &self.hidden {
            if range.start <= line {
                line += range.len();
            } else {
                break;
            }
        }
        line.min(buffer_lines.saturating_sub(1))
    }

    /// The row showing `line`, or `None` when the line is hidden.
    pub fn row_of_line(&self, line: usize) -> Option<usize> {
        let mut hidden_before = 0;
        for range in &self.hidden {
            if range.contains(&line) {
                return None;
            }
            if range.end <= line {
                hidden_before += range.len();
            } else {
                break;
            }
        }
        Some(line - hidden_before)
    }
}

/// The indent-foldable block whose header is at (or contains) `line`:
/// `(header, hidden_body)`.
///
/// A header is a line whose next non-blank line is more indented; the body runs to the
/// last consecutive line that is blank or more indented than the header. Blanks inside
/// a block fold with it; trailing blanks after the block do not (they belong to the
/// gap between blocks, and hiding them makes two neighbours look glued together).
pub fn foldable_block_at(
    text: &str,
    line: usize,
) -> Option<(usize, std::ops::Range<usize>)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let header = line;
    let header_indent = indent_of(lines.get(header)?)?;

    // The body: consecutive following lines that are blank or deeper than the header.
    let mut end = header + 1;
    let mut last_deep = header;
    while end < lines.len() {
        match indent_of(lines[end]) {
            None => end += 1, // blank: provisionally inside
            Some(indent) if indent > header_indent => {
                last_deep = end;
                end += 1;
            }
            Some(_) => break,
        }
    }
    (last_deep > header).then(|| (header, header + 1..last_deep + 1))
}

/// The block containing `line`: at `line`'s own header if it is one, else the nearest
/// header above whose body spans `line`. What ⌥⌘[ folds when the cursor sits mid-block.
pub fn enclosing_block(text: &str, line: usize) -> Option<(usize, std::ops::Range<usize>)> {
    if let Some(found) = foldable_block_at(text, line) {
        return Some(found);
    }
    (0..line).rev().find_map(|candidate| {
        foldable_block_at(text, candidate).filter(|(_, body)| body.contains(&line))
    })
}

/// Every top-level foldable block — what fold-all folds. Top-level means the least
/// indentation that has blocks at all, so a file whose whole body is inside one class
/// still folds its methods rather than nothing.
pub fn top_level_blocks(text: &str) -> Vec<std::ops::Range<usize>> {
    let lines: Vec<&str> = text.split('\n').collect();
    let base = lines.iter().filter_map(|line| indent_of(line)).min().unwrap_or(0);

    let mut blocks = Vec::new();
    let mut line = 0;
    while line < lines.len() {
        if indent_of(lines[line]) == Some(base)
            && let Some((_, body)) = foldable_block_at(text, line)
        {
            line = body.end;
            blocks.push(body);
        } else {
            line += 1;
        }
    }
    blocks
}

/// Indentation width in characters, or `None` for a blank line. Tabs count as one —
/// the comparison is only ever deeper-than, within one file's own convention.
fn indent_of(line: &str) -> Option<usize> {
    if line.trim().is_empty() {
        return None;
    }
    Some(line.len() - line.trim_start().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_is_the_identity_until_something_folds() {
        let folds = Folds::default();
        assert_eq!(folds.visible_count(10), 10);
        assert_eq!(folds.line_of_row(7, 10), 7);
        assert_eq!(folds.row_of_line(7), Some(7));
    }

    #[test]
    fn a_fold_hides_its_body_and_the_map_skips_it() {
        // Lines 0..10, fold hides 3..6 (header at 2 stays).
        let mut folds = Folds::default();
        folds.fold(3..6, 10);

        assert_eq!(folds.visible_count(10), 7);
        // Rows 0,1,2 are lines 0,1,2; row 3 is line 6.
        assert_eq!(folds.line_of_row(2, 10), 2);
        assert_eq!(folds.line_of_row(3, 10), 6);
        assert_eq!(folds.line_of_row(6, 10), 9);
        // And the inverse agrees.
        assert_eq!(folds.row_of_line(6), Some(3));
        assert_eq!(folds.row_of_line(9), Some(6));
        assert_eq!(folds.row_of_line(4), None, "a hidden line has no row");
        assert!(folds.is_hidden(4));
        assert!(!folds.is_hidden(2), "the header stays visible");
    }

    #[test]
    fn two_folds_compose_and_the_maps_stay_inverse() {
        let mut folds = Folds::default();
        folds.fold(2..4, 12);
        folds.fold(7..10, 12);

        assert_eq!(folds.visible_count(12), 7);
        // Exhaustive inverse check — the property that corrupts edits if it breaks.
        for row in 0..folds.visible_count(12) {
            let line = folds.line_of_row(row, 12);
            assert_eq!(folds.row_of_line(line), Some(row), "row {row} line {line}");
        }
        for line in 0..12 {
            if let Some(row) = folds.row_of_line(line) {
                assert_eq!(folds.line_of_row(row, 12), line, "line {line} row {row}");
            } else {
                assert!(folds.is_hidden(line));
            }
        }
    }

    #[test]
    fn overlapping_folds_merge_rather_than_double_count() {
        let mut folds = Folds::default();
        folds.fold(2..5, 10);
        folds.fold(4..7, 10);
        assert_eq!(folds.visible_count(10), 5, "2..7 hidden once, not 2..5 plus 4..7");
    }

    #[test]
    fn unfolding_reveals_exactly_the_containing_fold() {
        let mut folds = Folds::default();
        folds.fold(2..4, 12);
        folds.fold(7..10, 12);
        assert!(folds.unfold_containing(8));
        assert_eq!(folds.visible_count(12), 10, "the other fold survives");
        assert!(!folds.unfold_containing(8), "already revealed");
    }

    #[test]
    fn a_line_count_change_clears_all_folds() {
        let mut folds = Folds::default();
        folds.fold(2..4, 10);
        folds.invalidate(10);
        assert!(!folds.is_empty(), "same count, folds survive");
        folds.invalidate(11);
        assert!(folds.is_empty(), "a newline anywhere invalidates the line numbers");
    }

    #[test]
    fn an_indented_block_folds_from_its_header() {
        let text = "class A {\n    fn b() {\n        body;\n    }\n}\n";
        // Header line 0: everything deeper folds (lines 1..=3).
        assert_eq!(foldable_block_at(text, 0), Some((0, 1..4)));
        // Header line 1: its own body only.
        assert_eq!(foldable_block_at(text, 1), Some((1, 2..3)));
        // Line 4 (`}`) opens nothing.
        assert_eq!(foldable_block_at(text, 4), None);
    }

    #[test]
    fn blanks_inside_fold_and_trailing_blanks_do_not() {
        let text = "fn a() {\n    one;\n\n    two;\n}\n\nfn b() {}\n";
        let (_, body) = foldable_block_at(text, 0).expect("a block");
        assert_eq!(body, 1..4, "the inner blank folds; the gap after the brace does not");
    }

    #[test]
    fn a_flat_file_has_nothing_to_fold() {
        assert_eq!(foldable_block_at("a;\nb;\nc;\n", 1), None);
    }

    #[test]
    fn the_enclosing_block_is_found_from_mid_body() {
        let text = "class A {\n    fn b() {\n        body;\n    }\n}\n";
        // From the body line, the nearest header whose block contains it is fn b.
        assert_eq!(enclosing_block(text, 2), Some((1, 2..3)));
        // From a header line, the header's own block.
        assert_eq!(enclosing_block(text, 0), Some((0, 1..4)));
        // The closing brace of the class sits outside every body.
        assert_eq!(enclosing_block(text, 4), None);
    }

    #[test]
    fn fold_all_takes_every_top_level_block_once() {
        let text = "fn a() {\n    one;\n}\nfn b() {\n    two;\n    three;\n}\n";
        assert_eq!(top_level_blocks(text), vec![1..2, 4..6]);
    }

    #[test]
    fn fold_all_in_an_indented_file_still_folds_something() {
        // A snippet pasted with uniform leading indent: base is the minimum, not zero.
        let text = "    fn a() {\n        one;\n    }\n";
        assert_eq!(top_level_blocks(text), vec![1..2]);
    }
}
