//! Selecting a region of the terminal buffer, and turning it back into text.
//!
//! This is plain data over a [`GridSnapshot`] and deliberately has no window in it: the
//! coordinate maths (which cells a drag covers, where a word ends, what bytes come out) is
//! the part that is easy to get wrong and worth testing directly. The view's only job is
//! to turn a mouse position into a [`SelectionPoint`] and to ask which cells are selected.
//!
//! Coordinates are *buffer* coordinates, not viewport rows: a drag that scrolls, or output
//! arriving mid-drag, must not slide the selection across the text it was made on. A
//! buffer line is counted from the top of scrollback, which [`GridSnapshot`] can compute
//! because it carries `history_size` and `display_offset`.
//!
//! The one case that still moves is a selection older than the 10k scrollback limit: once
//! the oldest line is evicted every index above it shifts by one. Living with that is the
//! trade — tracking it exactly means a generation counter per line, and a selection made
//! ten thousand lines ago is not a thing anyone is holding on to.

use crate::grid::GridSnapshot;

/// A cell in buffer coordinates: `line` counts from the top of scrollback, so line 0 is
/// the oldest line still retained and the live screen sits at the far end.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SelectionPoint {
    pub line: usize,
    pub column: usize,
}

impl SelectionPoint {
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

/// What a drag selects as it moves: characters, whole words, or whole lines.
///
/// Set by the click count, and kept for the whole drag — dragging after a double-click
/// extends word by word, which is what every terminal and editor does.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SelectionMode {
    #[default]
    Char,
    Word,
    Line,
}

/// An in-progress or finished selection.
///
/// Anchor and head rather than start and end: the anchor is where the mouse went down and
/// does not move, so a drag back above the start selects upwards without the range
/// flipping inside out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Selection {
    pub anchor: SelectionPoint,
    pub head: SelectionPoint,
    pub mode: SelectionMode,
}

impl Selection {
    pub fn new(point: SelectionPoint, mode: SelectionMode) -> Self {
        Self { anchor: point, head: point, mode }
    }

    /// Moves the head, leaving the anchor where the drag started.
    pub fn extend_to(&mut self, point: SelectionPoint) {
        self.head = point;
    }

    /// The ordered pair, regardless of drag direction.
    pub fn ordered(&self) -> (SelectionPoint, SelectionPoint) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }

    /// True when nothing is actually covered — a click with no drag, in char mode.
    ///
    /// Word and line mode always cover something, so a double-click that never moves is
    /// still a selection.
    pub fn is_empty(&self) -> bool {
        self.mode == SelectionMode::Char && self.anchor == self.head
    }
}

/// The selectable geometry of one snapshot: how buffer lines map onto visible rows.
///
/// A small struct rather than four loose arguments, because every function here needs the
/// same three numbers and getting the sign of `display_offset` wrong is the bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GridGeometry {
    /// Buffer line of the top visible row.
    pub top_line: usize,
    pub rows: usize,
    pub columns: usize,
}

impl GridGeometry {
    /// From the three numbers alacritty reports. The one place the `top_line` formula
    /// lives, so a caller that has the `Term` but not a snapshot cannot get it wrong.
    ///
    /// history_size lines sit above the live screen; display_offset scrolls the window up
    /// into them, so the top visible row is that far short of the screen's own top.
    pub fn new(history_size: usize, display_offset: usize, rows: usize, columns: usize) -> Self {
        Self { top_line: history_size.saturating_sub(display_offset), rows, columns }
    }

    pub fn of(snapshot: &GridSnapshot) -> Self {
        Self::new(
            snapshot.history_size,
            snapshot.display_offset,
            snapshot.lines.len(),
            snapshot.columns,
        )
    }

    /// Buffer point for a visible row and column.
    pub fn point_at(&self, row: usize, column: usize) -> SelectionPoint {
        SelectionPoint::new(self.top_line + row, column.min(self.columns.saturating_sub(1)))
    }

    /// Visible row for a buffer line, or `None` when it has scrolled out of view.
    pub fn row_of(&self, line: usize) -> Option<usize> {
        let row = line.checked_sub(self.top_line)?;
        (row < self.rows).then_some(row)
    }
}

/// Maps a pixel position inside the grid onto a cell.
///
/// Clamped, not optional: a drag that leaves the panel should keep extending to the edge
/// rather than stop dead, which is what every terminal does.
pub fn cell_at(
    x: f32,
    y: f32,
    cell_width: f32,
    line_height: f32,
    geometry: GridGeometry,
) -> SelectionPoint {
    // A zero cell size would divide by zero on the first frame, before layout has run.
    if cell_width <= 0.0 || line_height <= 0.0 || geometry.rows == 0 || geometry.columns == 0 {
        return SelectionPoint::new(geometry.top_line, 0);
    }

    let row = (y / line_height).floor().clamp(0.0, (geometry.rows - 1) as f32) as usize;
    // Floor, not round: the selection is inclusive of the cell under the pointer, so the
    // cell being named is the one the glyph is drawn in. Rounding would make the right
    // half of a character select the *next* one, which reads as an off-by-one.
    let column = (x / cell_width).floor().clamp(0.0, (geometry.columns - 1) as f32) as usize;
    SelectionPoint::new(geometry.top_line + row, column)
}

/// The half-open column range selected on one visible row, or `None` if the row is outside.
///
/// Half-open so an empty range is representable and so the caller can compare against
/// `column < end` without an off-by-one at the last column.
pub fn selected_columns(
    selection: &Selection,
    row: usize,
    geometry: GridGeometry,
    snapshot: &GridSnapshot,
) -> Option<std::ops::Range<usize>> {
    if selection.is_empty() {
        return None;
    }

    let (start, end) = resolved_range(selection, snapshot, geometry);
    let line = geometry.top_line.checked_add(row)?;
    if line < start.line || line > end.line {
        return None;
    }

    let columns = geometry.columns;
    let from = if line == start.line { start.column } else { 0 };
    // Inclusive end column, so the cell under the mouse is part of the selection.
    // saturating: `SelectionPoint`'s fields are public, so a column of usize::MAX is
    // constructible even though `cell_at` clamps and never produces one.
    let to = if line == end.line { end.column.saturating_add(1).min(columns) } else { columns };
    (from < to).then_some(from..to)
}

/// The selected text, ready for the clipboard.
///
/// Trailing blanks are trimmed per line and lines are joined with `\n`, which is what
/// makes a copied stack trace paste as a stack trace rather than as padded columns. Only
/// the part of the selection still inside the snapshot can be extracted — the view holds
/// the visible grid, not the whole buffer.
pub fn selected_text(
    selection: &Selection,
    snapshot: &GridSnapshot,
    geometry: GridGeometry,
) -> String {
    if selection.is_empty() {
        return String::new();
    }

    let (start, end) = resolved_range(selection, snapshot, geometry);
    let mut out = String::new();
    // Whether anything has been emitted yet, rather than `out.is_empty()`: a selection
    // that starts on a blank line must keep that blank line. Testing the buffer would
    // silently swallow it, and ⌘A over a mostly-empty screen is exactly that case.
    let mut first = true;

    for line in start.line..=end.line {
        // A line the snapshot does not hold has scrolled out of view; it contributes
        // nothing, not even a separator, because there is no text to separate.
        let Some(row) = geometry.row_of(line) else { continue };
        let Some(cells) = snapshot.lines.get(row) else { continue };

        // Every *visible* line in range emits, even an empty one — a blank line inside a
        // stack trace is part of it, and a leading blank must survive ⌘A.
        if !first {
            out.push('\n');
        }
        first = false;

        let from = if line == start.line { start.column } else { 0 };
        let to = if line == end.line {
            end.column.saturating_add(1).min(cells.len())
        } else {
            cells.len()
        };
        if from >= to {
            continue;
        }

        let mut text = String::new();
        for cell in &cells[from..to] {
            // The trailing half of a wide glyph is not a character; emitting it would put
            // a stray space inside every CJK word that gets copied.
            if cell.wide_spacer {
                continue;
            }
            // A never-written cell holds NUL, which would land in the clipboard as one.
            text.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
        text.truncate(text.trim_end().len());
        out.push_str(&text);
    }

    // Trailing blank lines are padding, not content: a selection dragged to the bottom of
    // a mostly-empty screen should not paste a screenful of newlines.
    out.truncate(out.trim_end_matches('\n').len());
    out
}

/// Expands the raw anchor/head pair according to the mode, and orders it.
fn resolved_range(
    selection: &Selection,
    snapshot: &GridSnapshot,
    geometry: GridGeometry,
) -> (SelectionPoint, SelectionPoint) {
    let (start, end) = selection.ordered();
    let last_column = geometry.columns.saturating_sub(1);

    match selection.mode {
        SelectionMode::Char => (start, end),
        SelectionMode::Line => {
            (SelectionPoint::new(start.line, 0), SelectionPoint::new(end.line, last_column))
        }
        SelectionMode::Word => {
            let start = word_start(snapshot, geometry, start);
            let end = word_end(snapshot, geometry, end);
            (start, end)
        }
    }
}

/// Characters that break a word.
///
/// `/`, `.`, `-` and `_` are deliberately *not* separators: the thing being double-clicked
/// in a terminal is usually a path or a `Class::method`, and splitting those is the
/// difference between a useful double-click and a useless one.
fn is_word_separator(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t'
            | '\0'
            | '"'
            | '\''
            | '`'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '<'
            | '>'
            | ','
            | ';'
    )
}

fn char_at(snapshot: &GridSnapshot, geometry: GridGeometry, point: SelectionPoint) -> Option<char> {
    let row = geometry.row_of(point.line)?;
    let cell = snapshot.lines.get(row)?.get(point.column)?;
    Some(if cell.c == '\0' { ' ' } else { cell.c })
}

fn word_start(
    snapshot: &GridSnapshot,
    geometry: GridGeometry,
    point: SelectionPoint,
) -> SelectionPoint {
    // Double-clicking whitespace selects just that cell rather than running to the end of
    // the indentation, which would be surprising.
    if char_at(snapshot, geometry, point).is_none_or(is_word_separator) {
        return point;
    }

    let mut column = point.column;
    while column > 0 {
        let candidate = SelectionPoint::new(point.line, column - 1);
        if char_at(snapshot, geometry, candidate).is_none_or(is_word_separator) {
            break;
        }
        column -= 1;
    }
    SelectionPoint::new(point.line, column)
}

fn word_end(
    snapshot: &GridSnapshot,
    geometry: GridGeometry,
    point: SelectionPoint,
) -> SelectionPoint {
    if char_at(snapshot, geometry, point).is_none_or(is_word_separator) {
        return point;
    }

    let last = geometry.columns.saturating_sub(1);
    let mut column = point.column;
    while column < last {
        let candidate = SelectionPoint::new(point.line, column + 1);
        if char_at(snapshot, geometry, candidate).is_none_or(is_word_separator) {
            break;
        }
        column += 1;
    }
    SelectionPoint::new(point.line, column)
}

/// A selection covering everything the snapshot can see.
///
/// Select-all over the *visible* grid, not the whole 10k scrollback: the snapshot is what
/// the view holds, and copying history it never rendered would need a second path through
/// the terminal lock for a case nobody has asked for.
pub fn select_all(snapshot: &GridSnapshot, geometry: GridGeometry) -> Option<Selection> {
    let last_row = snapshot.lines.len().checked_sub(1)?;
    Some(Selection {
        anchor: geometry.point_at(0, 0),
        head: geometry.point_at(last_row, geometry.columns.saturating_sub(1)),
        mode: SelectionMode::Char,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Cell;

    /// A snapshot with the given rows written into it, left-aligned.
    fn snapshot_of(rows: &[&str], columns: usize) -> GridSnapshot {
        let mut snapshot = GridSnapshot::empty(columns, rows.len());
        for (row, text) in rows.iter().enumerate() {
            let mut column = 0;
            for c in text.chars() {
                if column >= columns {
                    break;
                }
                snapshot.lines[row][column] = Cell { c, ..Cell::default() };
                column += 1;
                // A wide glyph occupies two cells, the second a spacer — the same shape
                // alacritty produces, so extraction is exercised against the real layout.
                if unicode_width(c) == 2 && column < columns {
                    snapshot.lines[row][column] =
                        Cell { c: ' ', wide_spacer: true, ..Cell::default() };
                    column += 1;
                }
            }
        }
        snapshot
    }

    fn unicode_width(c: char) -> usize {
        // Only the CJK block the tests use; the real widths come from alacritty.
        if ('\u{1100}'..='\u{9fff}').contains(&c) { 2 } else { 1 }
    }

    fn geometry(snapshot: &GridSnapshot) -> GridGeometry {
        GridGeometry::of(snapshot)
    }

    #[test]
    fn a_click_with_no_drag_selects_nothing() {
        let selection = Selection::new(SelectionPoint::new(0, 3), SelectionMode::Char);
        assert!(selection.is_empty());
        // But a double-click that never moves still selects its word.
        let word = Selection::new(SelectionPoint::new(0, 3), SelectionMode::Word);
        assert!(!word.is_empty());
    }

    #[test]
    fn dragging_backwards_selects_the_same_range() {
        let snapshot = snapshot_of(&["hello world"], 20);
        let geometry = geometry(&snapshot);

        let mut forward = Selection::new(SelectionPoint::new(0, 0), SelectionMode::Char);
        forward.extend_to(SelectionPoint::new(0, 4));

        let mut backward = Selection::new(SelectionPoint::new(0, 4), SelectionMode::Char);
        backward.extend_to(SelectionPoint::new(0, 0));

        assert_eq!(selected_text(&forward, &snapshot, geometry), "hello");
        assert_eq!(selected_text(&backward, &snapshot, geometry), "hello");
    }

    #[test]
    fn a_multi_line_selection_joins_with_newlines_and_trims_padding() {
        let snapshot = snapshot_of(&["first", "second", "third"], 20);
        let geometry = geometry(&snapshot);

        let mut selection = Selection::new(SelectionPoint::new(0, 0), SelectionMode::Char);
        selection.extend_to(SelectionPoint::new(2, 4));

        // The middle line runs to the full width; the padding must not be copied.
        assert_eq!(selected_text(&selection, &snapshot, geometry), "first\nsecond\nthird");
    }

    #[test]
    fn copying_multibyte_text_yields_exactly_those_bytes() {
        let snapshot = snapshot_of(&["ação"], 20);
        let geometry = geometry(&snapshot);

        let mut selection = Selection::new(SelectionPoint::new(0, 0), SelectionMode::Char);
        selection.extend_to(SelectionPoint::new(0, 3));

        let text = selected_text(&selection, &snapshot, geometry);
        assert_eq!(text, "ação");
        // The column maths is in cells; the bytes must still come out whole.
        assert_eq!(text.as_bytes(), "ação".as_bytes());
    }

    #[test]
    fn a_selection_landing_on_a_wide_spacer_does_not_duplicate_the_glyph() {
        let snapshot = snapshot_of(&["日本語"], 20);
        let geometry = geometry(&snapshot);

        // Columns 0..=5: three glyphs, each with a spacer behind it.
        let mut selection = Selection::new(SelectionPoint::new(0, 0), SelectionMode::Char);
        selection.extend_to(SelectionPoint::new(0, 5));
        assert_eq!(selected_text(&selection, &snapshot, geometry), "日本語");

        // Ending *on* a spacer still yields the glyph it belongs to, not a stray space.
        let mut half = Selection::new(SelectionPoint::new(0, 0), SelectionMode::Char);
        half.extend_to(SelectionPoint::new(0, 1));
        assert_eq!(selected_text(&half, &snapshot, geometry), "日");
    }

    #[test]
    fn double_click_selects_a_word_and_keeps_paths_whole() {
        let snapshot = snapshot_of(&["at app/Http/Kernel.php:42 line"], 40);
        let geometry = geometry(&snapshot);

        // Anywhere inside the path.
        let selection = Selection::new(SelectionPoint::new(0, 8), SelectionMode::Word);
        // Slashes, dots and colons are not separators, so a stack-trace path comes out
        // whole — the entire reason double-click is worth having here.
        assert_eq!(selected_text(&selection, &snapshot, geometry), "app/Http/Kernel.php:42");
    }

    #[test]
    fn double_clicking_a_space_selects_only_that_cell() {
        let snapshot = snapshot_of(&["a  b"], 10);
        let geometry = geometry(&snapshot);

        let selection = Selection::new(SelectionPoint::new(0, 1), SelectionMode::Word);
        // Trailing whitespace is trimmed out of the copy, so this is empty rather than
        // a run of spaces — the point is that it does not swallow the neighbouring words.
        assert_eq!(selected_text(&selection, &snapshot, geometry), "");
    }

    #[test]
    fn triple_click_takes_the_whole_line_whatever_column_it_started_in() {
        let snapshot = snapshot_of(&["one two three"], 40);
        let geometry = geometry(&snapshot);

        let selection = Selection::new(SelectionPoint::new(0, 7), SelectionMode::Line);
        assert_eq!(selected_text(&selection, &snapshot, geometry), "one two three");
    }

    #[test]
    fn word_mode_extends_word_by_word_when_dragged() {
        let snapshot = snapshot_of(&["alpha beta gamma"], 40);
        let geometry = geometry(&snapshot);

        // Down on "beta", dragged into the middle of "gamma".
        let mut selection = Selection::new(SelectionPoint::new(0, 7), SelectionMode::Word);
        selection.extend_to(SelectionPoint::new(0, 13));
        assert_eq!(selected_text(&selection, &snapshot, geometry), "beta gamma");
    }

    #[test]
    fn scrollback_lines_keep_their_identity_when_the_viewport_moves() {
        // A viewport scrolled two lines up into a ten-line history: the top visible row is
        // buffer line 8, not 0.
        let mut snapshot = snapshot_of(&["a", "b", "c"], 10);
        snapshot.history_size = 10;
        snapshot.display_offset = 2;

        let geometry = geometry(&snapshot);
        assert_eq!(geometry.top_line, 8);
        assert_eq!(geometry.point_at(0, 0), SelectionPoint::new(8, 0));
        assert_eq!(geometry.row_of(9), Some(1));
        // A line above the viewport has no row, and asking must not panic or wrap.
        assert_eq!(geometry.row_of(3), None);
        assert_eq!(geometry.row_of(11), None);
    }

    #[test]
    fn a_selection_partly_scrolled_out_extracts_only_what_is_visible() {
        let mut snapshot = snapshot_of(&["visible"], 10);
        snapshot.history_size = 5;
        snapshot.display_offset = 0;
        let geometry = geometry(&snapshot);
        assert_eq!(geometry.top_line, 5);

        // Anchored in history that the snapshot does not contain.
        let mut selection = Selection::new(SelectionPoint::new(0, 0), SelectionMode::Char);
        selection.extend_to(SelectionPoint::new(5, 6));
        assert_eq!(selected_text(&selection, &snapshot, geometry), "visible");
    }

    #[test]
    fn selected_columns_covers_the_cell_under_the_mouse() {
        let snapshot = snapshot_of(&["abcdef"], 10);
        let geometry = geometry(&snapshot);

        let mut selection = Selection::new(SelectionPoint::new(0, 1), SelectionMode::Char);
        selection.extend_to(SelectionPoint::new(0, 3));
        // Inclusive of column 3: the cell the mouse is over is selected.
        assert_eq!(selected_columns(&selection, 0, geometry, &snapshot), Some(1..4));
        // Rows outside the selection get nothing rather than an empty range.
        assert_eq!(selected_columns(&selection, 1, geometry, &snapshot), None);
    }

    #[test]
    fn selected_columns_spans_full_rows_in_the_middle_of_a_block() {
        let snapshot = snapshot_of(&["one", "two", "three"], 8);
        let geometry = geometry(&snapshot);

        let mut selection = Selection::new(SelectionPoint::new(0, 2), SelectionMode::Char);
        selection.extend_to(SelectionPoint::new(2, 1));

        assert_eq!(selected_columns(&selection, 0, geometry, &snapshot), Some(2..8));
        assert_eq!(selected_columns(&selection, 1, geometry, &snapshot), Some(0..8));
        assert_eq!(selected_columns(&selection, 2, geometry, &snapshot), Some(0..2));
    }

    #[test]
    fn a_pixel_position_maps_to_the_cell_under_it() {
        let snapshot = snapshot_of(&["abc", "def"], 10);
        let geometry = geometry(&snapshot);

        // 8px cells, 16px lines: the middle of row 1, column 2.
        assert_eq!(cell_at(20.0, 24.0, 8.0, 16.0, geometry), SelectionPoint::new(1, 2));
        // A drag past the right edge clamps to the last column rather than stopping.
        assert_eq!(cell_at(9999.0, 0.0, 8.0, 16.0, geometry), SelectionPoint::new(0, 9));
        // And past the bottom, to the last row.
        assert_eq!(cell_at(0.0, 9999.0, 8.0, 16.0, geometry), SelectionPoint::new(1, 0));
        // Negative (dragged above the panel) clamps to the first row.
        assert_eq!(cell_at(-50.0, -50.0, 8.0, 16.0, geometry), SelectionPoint::new(0, 0));
    }

    #[test]
    fn a_zero_sized_cell_does_not_divide_by_zero() {
        let snapshot = snapshot_of(&["a"], 10);
        let geometry = geometry(&snapshot);
        // The first frame lays out at zero before the canvas has measured anything.
        assert_eq!(cell_at(10.0, 10.0, 0.0, 0.0, geometry), SelectionPoint::new(0, 0));
    }

    #[test]
    fn select_all_covers_the_visible_grid() {
        let snapshot = snapshot_of(&["one", "two"], 8);
        let geometry = geometry(&snapshot);

        let selection = select_all(&snapshot, geometry).unwrap();
        assert_eq!(selected_text(&selection, &snapshot, geometry), "one\ntwo");
    }

    #[test]
    fn a_leading_blank_line_survives_the_copy() {
        // ⌘A on a real terminal: the prompt sits partway down a mostly-blank screen, so
        // the selection starts on blank lines. Dropping them shifts every line of a
        // copied stack trace up, which is the bug this asserts against.
        let snapshot = snapshot_of(&["", "  at Foo.php:1", "  at Bar.php:2"], 20);
        let geometry = geometry(&snapshot);

        let selection = select_all(&snapshot, geometry).unwrap();
        assert_eq!(
            selected_text(&selection, &snapshot, geometry),
            "\n  at Foo.php:1\n  at Bar.php:2"
        );
    }

    #[test]
    fn interior_blank_lines_are_kept_and_trailing_ones_are_not() {
        let snapshot = snapshot_of(&["one", "", "", "two", "", ""], 8);
        let geometry = geometry(&snapshot);
        let selection = select_all(&snapshot, geometry).unwrap();

        // The blanks between the text are part of it; the ones after are screen padding.
        assert_eq!(selected_text(&selection, &snapshot, geometry), "one\n\n\ntwo");
    }

    #[test]
    fn a_column_beyond_the_grid_does_not_overflow() {
        // `SelectionPoint`'s fields are public, so this is constructible even though
        // `cell_at` clamps. `end.column + 1` would panic on overflow in a debug build.
        let snapshot = snapshot_of(&["abc"], 8);
        let geometry = geometry(&snapshot);

        let mut selection = Selection::new(SelectionPoint::new(0, 0), SelectionMode::Char);
        selection.extend_to(SelectionPoint::new(0, usize::MAX));

        assert_eq!(selected_text(&selection, &snapshot, geometry), "abc");
        assert_eq!(selected_columns(&selection, 0, geometry, &snapshot), Some(0..8));
    }

    #[test]
    fn an_empty_selection_copies_nothing() {
        let snapshot = snapshot_of(&["text"], 8);
        let geometry = geometry(&snapshot);
        let selection = Selection::new(SelectionPoint::new(0, 0), SelectionMode::Char);
        assert_eq!(selected_text(&selection, &snapshot, geometry), "");
    }
}
