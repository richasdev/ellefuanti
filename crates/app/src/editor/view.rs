//! The gpui view that renders a [`Document`] and turns input into edits.
//!
//! Deliberately thin: every editing *semantic* lives in `Document` (plain Rust, unit
//! tested). This file translates input into `Document` calls and `Document` state into
//! elements, and owns nothing else.

use std::ops::Range;

use elle_syntax::HighlightSpan;
use elle_text::Point;
use gpui::{
    App, ClipboardItem, Context, FocusHandle, Focusable, HighlightStyle as GpuiHighlight,
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, ScrollStrategy, SharedString, StyledText,
    TextRun, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::actions::{
    Backspace, Copy, Cut, Delete, MoveDown, MoveLeft, MoveLineEnd, MoveLineStart, MoveRight,
    MoveUp, Newline, Paste, Redo, SelectAll, SelectDown, SelectLeft, SelectRight, SelectUp, Tab,
    Undo, context,
};
use crate::editor::state::Document;
use crate::theme::{Metrics, Theme};

/// Monospace family.
///
/// `Menlo` ships with macOS itself. `SF Mono` does *not* — it comes with Xcode/Terminal —
/// and a missing family does not error: gpui's `resolve_font` silently falls back to a
/// proportional font, which would break every column calculation below in a way that
/// looks like a layout bug rather than a missing font.
pub const FONT_FAMILY: &str = "Menlo";

/// How much text may be measured to map a click to a column.
///
/// Guards against a pathological single-line file (a minified asset) making one click
/// shape a megabyte of text on the UI thread.
const MAX_MEASURE_BYTES: usize = 4096;

pub struct EditorView {
    pub document: Document,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    /// Visible row range from the last frame, captured because `uniform_list` exposes it
    /// only to the render closure and scroll-into-view needs it outside of one.
    visible_rows: Range<usize>,
}

impl EditorView {
    pub fn new(document: Document, cx: &mut Context<Self>) -> Self {
        Self {
            document,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            visible_rows: 0..0,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.document.buffer.is_dirty()
    }

    /// Scrolls so the cursor row is on screen. Called after any motion or edit.
    fn scroll_cursor_into_view(&mut self) {
        let row = self.document.cursor_point().row;
        // scroll_to_item is a no-op when the item is already visible, so this is cheap to
        // call unconditionally and does not fight the user's own scrolling.
        self.scroll.scroll_to_item(row, ScrollStrategy::Top);
    }

    /// Handles a raw keypress, for characters the action system does not cover.
    ///
    /// Everything with a command/control modifier, and every navigation key, is left to
    /// the keymap in `actions.rs`. What remains is literal text insertion.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = &keystroke.modifiers;

        // `platform` is cmd on macOS. Chords are keybindings, not text.
        if modifiers.platform || modifiers.control || modifiers.function {
            return;
        }

        // `key_char` is the literal character *after* the layout applies shift and dead
        // keys ("ß" for option-s), and is None for command chords. `key` is the
        // layout-independent label — right for bindings, wrong for insertion.
        let Some(text) = keystroke.key_char.as_deref() else { return };

        // Navigation and editing keys arrive with a key_char in some cases; they are
        // handled by actions, so ignore them here to avoid inserting control characters.
        if matches!(
            keystroke.key.as_str(),
            "backspace"
                | "delete"
                | "enter"
                | "tab"
                | "escape"
                | "left"
                | "right"
                | "up"
                | "down"
                | "home"
                | "end"
                | "pageup"
                | "pagedown"
        ) {
            return;
        }

        if text.is_empty() || text.chars().all(|c| c.is_control()) {
            return;
        }

        self.document.insert(text);
        self.scroll_cursor_into_view();
        cx.notify();
    }

    // --- action handlers ---------------------------------------------------------
    //
    // Each is a two-liner over Document plus notify. The pattern repeats because gpui
    // dispatches one handler per action type; the alternative is a single handler with a
    // match on a parameterised action, which trades this repetition for a less direct
    // keymap. Not worth it at this size.

    fn backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.backspace();
        self.after_edit(cx);
    }

    fn delete(&mut self, _: &Delete, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.delete_forward();
        self.after_edit(cx);
    }

    fn newline(&mut self, _: &Newline, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.insert("\n");
        self.after_edit(cx);
    }

    fn tab(&mut self, _: &Tab, _w: &mut Window, cx: &mut Context<Self>) {
        // ponytail: four spaces, which is PSR-12 and therefore right for Laravel. Reads
        // indent settings once a settings crate exists (Milestone 1 task 15+).
        self.document.insert("    ");
        self.after_edit(cx);
    }

    fn undo(&mut self, _: &Undo, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.undo();
        self.after_edit(cx);
    }

    fn redo(&mut self, _: &Redo, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.redo();
        self.after_edit(cx);
    }

    fn move_left(&mut self, _: &MoveLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_horizontal(false, false);
        self.after_move(cx);
    }

    fn move_right(&mut self, _: &MoveRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_horizontal(true, false);
        self.after_move(cx);
    }

    fn move_up(&mut self, _: &MoveUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_vertical(false, false);
        self.after_move(cx);
    }

    fn move_down(&mut self, _: &MoveDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_vertical(true, false);
        self.after_move(cx);
    }

    fn move_line_start(&mut self, _: &MoveLineStart, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_line_start(false);
        self.after_move(cx);
    }

    fn move_line_end(&mut self, _: &MoveLineEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_line_end(false);
        self.after_move(cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_horizontal(false, true);
        self.after_move(cx);
    }

    fn select_right(&mut self, _: &SelectRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_horizontal(true, true);
        self.after_move(cx);
    }

    fn select_up(&mut self, _: &SelectUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_vertical(false, true);
        self.after_move(cx);
    }

    fn select_down(&mut self, _: &SelectDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_vertical(true, true);
        self.after_move(cx);
    }

    fn select_all(&mut self, _: &SelectAll, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.select_all();
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.document.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, _: &Cut, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.document.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.document.insert("");
            self.after_edit(cx);
        }
    }

    fn paste(&mut self, _: &Paste, _w: &mut Window, cx: &mut Context<Self>) {
        // text() is Option because the clipboard may hold an image.
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.document.insert(&text);
            self.after_edit(cx);
        }
    }

    fn after_edit(&mut self, cx: &mut Context<Self>) {
        self.scroll_cursor_into_view();
        cx.notify();
    }

    fn after_move(&mut self, cx: &mut Context<Self>) {
        self.scroll_cursor_into_view();
        cx.notify();
    }

    /// Maps a click inside a row to a text offset and moves the cursor there.
    fn on_row_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        row: usize,
        text_origin_x: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let line = self.document.buffer.line(row);
        let x = event.position.x - text_origin_x;

        let column = if line.is_empty() || x <= px(0.0) {
            0
        } else {
            let measured = &line[..line.len().min(MAX_MEASURE_BYTES)];
            let runs = [TextRun {
                len: measured.len(),
                font: gpui::font(FONT_FAMILY),
                color: gpui::white(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            // closest_index_for_x clamps, unlike index_for_x which returns None past the
            // end of the line — clamping is what a click past end-of-line should do.
            window
                .text_system()
                .layout_line(measured, Metrics::FONT_SIZE, &runs, None)
                .closest_index_for_x(x)
        };

        let offset = self.document.buffer.point_to_offset(Point::new(row, column));
        // Shift-click extends the existing selection, matching every other editor.
        self.document.move_to(offset, event.modifiers.shift);
        cx.notify();
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::dark();
        let row_count = self.document.buffer.len_lines();
        let cursor = self.document.cursor_point();
        let entity = cx.entity();

        div()
            .key_context(context::EDITOR)
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::move_line_start))
            .on_action(cx.listener(Self::move_line_end))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .size_full()
            .bg(theme.background)
            .font_family(FONT_FAMILY)
            .text_size(Metrics::FONT_SIZE)
            .text_color(theme.text)
            .child(
                // uniform_list calls back only for visible rows, so a 50k-line file costs
                // the same per frame as a 50-line one.
                uniform_list("editor-rows", row_count, move |range, _window, cx| {
                    entity.update(cx, |editor, cx| editor.render_rows(range, cursor, cx))
                })
                .track_scroll(self.scroll.clone())
                .size_full(),
            )
    }
}

impl EditorView {
    /// Builds the elements for one band of visible rows.
    fn render_rows(
        &mut self,
        range: Range<usize>,
        cursor: Point,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        // Capture the visible band for scroll-into-view, which runs outside render.
        self.visible_rows = range.clone();

        let theme = Theme::dark();
        let selection = self.document.selection.range();

        // Highlight once for the whole visible band rather than per row: one tree walk
        // instead of N, and the spans are already sorted so slicing per row is cheap.
        let band = self.visible_byte_range(&range);
        let spans = self.document.syntax.highlights(&self.document.buffer, band);

        let entity = cx.entity();

        range
            .map(|row| {
                let line = self.document.buffer.line(row);
                let line_start = self.document.buffer.point_to_offset(Point::new(row, 0));
                let line_end = line_start + line.len();

                let is_cursor_row = row == cursor.row;
                let row_selected = !selection.is_empty()
                    && selection.start < line_end.max(line_start + 1)
                    && selection.end > line_start;
                let entity = entity.clone();

                div()
                    .id(("row", row))
                    .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        let event = event.clone();
                        entity.update(cx, |editor, cx| {
                            // The text column starts after the gutter; the event position
                            // is window-relative, so the gutter width is the offset to
                            // subtract. Exact enough for click-to-place; a custom Element
                            // would give true row-local bounds.
                            editor.on_row_mouse_down(
                                &event,
                                row,
                                Metrics::GUTTER_WIDTH,
                                window,
                                cx,
                            );
                        });
                    })
                    .flex()
                    .h(Metrics::LINE_HEIGHT)
                    .w_full()
                    .when(row_selected, |el| el.bg(theme.selection))
                    .when(is_cursor_row && !row_selected, |el| el.bg(theme.hover))
                    .child(
                        // Gutter. Right-aligned so digits line up as numbers grow.
                        div()
                            .w(Metrics::GUTTER_WIDTH)
                            .flex()
                            .flex_none()
                            .justify_end()
                            .pr_3()
                            .text_color(if is_cursor_row { theme.text } else { theme.text_muted })
                            .child(SharedString::from((row + 1).to_string())),
                    )
                    .child(div().flex_1().child(styled_line(
                        &line,
                        line_start,
                        &spans,
                        &theme,
                        if is_cursor_row { Some(cursor.column) } else { None },
                    )))
                    .into_any_element()
            })
            .collect()
    }

    /// Byte range covered by a row range, for a single batched highlight query.
    fn visible_byte_range(&self, rows: &Range<usize>) -> Range<usize> {
        let buffer = &self.document.buffer;
        let start = buffer.point_to_offset(Point::new(rows.start, 0));
        let last_row = rows.end.saturating_sub(1).min(buffer.len_lines().saturating_sub(1));
        let end = buffer.point_to_offset(Point::new(last_row, buffer.line_len(last_row)));
        start..end.max(start)
    }
}

/// Renders one line with syntax colours, and a visible cursor when it is the cursor row.
///
/// The cursor is drawn as a background highlight on the character under it rather than a
/// separate positioned element: no absolute layout, no measuring, and it stays correct on
/// a proportional fallback font. A real caret (thin, blinking, between characters) needs
/// pixel positioning via `LineLayout::x_for_index`.
/// ponytail: block cursor for now; swap in a caret when the editor gets its own custom
/// `Element` impl, which is the same change that unlocks IME (see MILESTONE-1 task 11).
fn styled_line(
    line: &str,
    line_start: usize,
    spans: &[HighlightSpan],
    theme: &Theme,
    cursor_column: Option<usize>,
) -> StyledText {
    let (text, highlights) = line_runs(line, line_start, spans, theme, cursor_column);
    StyledText::new(SharedString::from(text)).with_highlights(highlights)
}

/// The text and colour runs for one rendered line.
///
/// Split out from [`styled_line`] because `StyledText` is opaque once built — there is no
/// way to ask it what it will paint. Returning the runs first makes the part that can
/// actually be wrong (clipping, rebasing, cursor placement, char boundaries) assertable
/// without a GPU, which is the only slice of "does it render correctly" a machine can check
/// here. See `crates/app/tests/render.rs`.
fn line_runs(
    line: &str,
    line_start: usize,
    spans: &[HighlightSpan],
    theme: &Theme,
    cursor_column: Option<usize>,
) -> (String, Vec<(Range<usize>, GpuiHighlight)>) {
    let line_end = line_start + line.len();

    // A cursor at end-of-line has no character to paint under, so pad with one space.
    // Doing it up front means the highlight logic below has a single code path.
    let cursor_at_end = cursor_column.is_some_and(|column| column >= line.len());
    let text = if cursor_at_end { format!("{line} ") } else { line.to_string() };

    let mut highlights: Vec<(Range<usize>, GpuiHighlight)> = Vec::new();

    for span in spans {
        // Clip the span to this line and rebase onto line-local offsets.
        if span.range.end <= line_start || span.range.start >= line_end {
            continue;
        }
        let start = span.range.start.max(line_start) - line_start;
        let end = span.range.end.min(line_end) - line_start;
        if start >= end {
            continue;
        }
        // StyledText debug-asserts on non-boundary indices, and a clipped multibyte span
        // can land mid-codepoint. Snap rather than panic in a debug build.
        let start = floor_boundary(&text, start);
        let end = ceil_boundary(&text, end);
        highlights.push((
            start..end,
            GpuiHighlight { color: Some(theme.syntax(span.style)), ..Default::default() },
        ));
    }

    if let Some(column) = cursor_column {
        let start = floor_boundary(&text, column.min(text.len()));
        let end = ceil_boundary(&text, (start + 1).min(text.len()));
        if start < end {
            // The cursor must win the overlap with any syntax colour underneath, so it is
            // pushed last and later spans are dropped rather than blended.
            highlights.retain(|(range, _)| range.end <= start || range.start >= end);
            highlights.push((
                start..end,
                GpuiHighlight {
                    background_color: Some(theme.cursor),
                    color: Some(theme.background),
                    ..Default::default()
                },
            ));
        }
    }

    highlights.sort_by_key(|(range, _)| range.start);
    (text, highlights)
}

/// Largest char boundary <= `index`.
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Smallest char boundary >= `index`.
fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_helpers_never_split_a_codepoint() {
        let s = "aébc";
        // "é" occupies bytes 1..3.
        assert_eq!(floor_boundary(s, 2), 1);
        assert_eq!(ceil_boundary(s, 2), 3);
        assert_eq!(floor_boundary(s, 0), 0);
        assert_eq!(ceil_boundary(s, 99), s.len());
        for i in 0..=s.len() {
            assert!(s.is_char_boundary(floor_boundary(s, i)));
            assert!(s.is_char_boundary(ceil_boundary(s, i)));
        }
    }

    // --- what actually gets painted -------------------------------------------------
    //
    // `StyledText` is opaque once constructed, so these test `line_runs`, which is the
    // same computation one step earlier. This is the largest slice of "does it render
    // correctly" that a machine can check without a GPU: whether the right bytes get the
    // right colour. What it cannot check is whether those runs reach the screen at the
    // right pixels — that is issue #35, and it needs a human.

    use elle_syntax::{HighlightSpan, HighlightStyle};

    fn span(range: Range<usize>, style: HighlightStyle) -> HighlightSpan {
        HighlightSpan { range, style }
    }

    /// Byte ranges carrying a foreground colour, ignoring the cursor's inverted run.
    fn coloured(runs: &[(Range<usize>, GpuiHighlight)], theme: &Theme) -> Vec<Range<usize>> {
        runs.iter()
            .filter(|(_, style)| style.background_color != Some(theme.cursor))
            .map(|(range, _)| range.clone())
            .collect()
    }

    #[test]
    fn spans_are_rebased_onto_line_local_offsets() {
        let theme = Theme::dark();
        // Line 3 of a document, starting at byte 100. A span at document byte 104..108
        // must paint at line-local 4..8, not 104..108 — getting this wrong paints the
        // wrong word, or panics past the end of a short line.
        let (text, runs) = line_runs(
            "    return $this;",
            100,
            &[span(104..110, HighlightStyle::Keyword)],
            &theme,
            None,
        );

        assert_eq!(text, "    return $this;");
        assert_eq!(coloured(&runs, &theme), vec![4..10]);
        assert_eq!(&text[4..10], "return");
    }

    #[test]
    fn spans_outside_the_line_are_dropped() {
        let theme = Theme::dark();
        let (_, runs) = line_runs(
            "middle line",
            100,
            &[
                span(0..50, HighlightStyle::Comment),    // entirely before
                span(200..250, HighlightStyle::Comment), // entirely after
            ],
            &theme,
            None,
        );
        assert!(coloured(&runs, &theme).is_empty());
    }

    #[test]
    fn a_span_straddling_the_line_is_clipped_to_it() {
        let theme = Theme::dark();
        // A block comment opening on an earlier line and closing on a later one: the
        // visible part must still colour, clipped at both ends rather than overflowing.
        let line = "still inside";
        let (text, runs) =
            line_runs(line, 100, &[span(50..200, HighlightStyle::Comment)], &theme, None);

        assert_eq!(coloured(&runs, &theme), vec![0..line.len()]);
        assert!(runs.iter().all(|(r, _)| r.end <= text.len()), "no run may exceed the text");
    }

    #[test]
    fn runs_never_split_a_multibyte_character() {
        let theme = Theme::dark();
        // "ção" — the span deliberately ends mid-codepoint, which StyledText
        // debug-asserts against. It must snap to a boundary instead of panicking.
        let line = "$mensagem = 'ação';";
        let (text, runs) =
            line_runs(line, 0, &[span(13..17, HighlightStyle::String)], &theme, None);

        for (range, _) in &runs {
            assert!(
                text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
                "run {range:?} splits a codepoint in {text:?}"
            );
        }
    }

    #[test]
    fn the_cursor_paints_over_the_syntax_colour_beneath_it() {
        let theme = Theme::dark();
        // Cursor inside a keyword: it must win the overlap, or it becomes invisible
        // exactly where the user is looking.
        let (_, runs) =
            line_runs("return $x;", 0, &[span(0..6, HighlightStyle::Keyword)], &theme, Some(2));

        let cursor: Vec<_> =
            runs.iter().filter(|(_, s)| s.background_color == Some(theme.cursor)).collect();
        assert_eq!(cursor.len(), 1, "exactly one cursor run");
        assert_eq!(cursor[0].0, 2..3);

        // And nothing else may overlap it.
        for (range, style) in &runs {
            if style.background_color != Some(theme.cursor) {
                assert!(range.end <= 2 || range.start >= 3, "run {range:?} overlaps the cursor");
            }
        }
    }

    #[test]
    fn a_cursor_past_the_end_of_the_line_still_has_something_to_paint() {
        let theme = Theme::dark();
        // At end-of-line there is no character under the cursor, so the line is padded.
        // Without this the cursor silently vanishes at the end of every line.
        let (text, runs) = line_runs("ab", 0, &[], &theme, Some(2));

        assert_eq!(text, "ab ", "padded so the cursor has a cell");
        let cursor = runs.iter().find(|(_, s)| s.background_color == Some(theme.cursor));
        assert_eq!(cursor.expect("a cursor run").0, 2..3);
    }

    #[test]
    fn the_cursor_lands_on_a_whole_multibyte_character() {
        let theme = Theme::dark();
        // Cursor on "ç" (2 bytes). A 1-byte cursor run would split it and panic.
        let line = "ação";
        let (text, runs) = line_runs(line, 0, &[], &theme, Some(1));

        let (range, _) = runs
            .iter()
            .find(|(_, s)| s.background_color == Some(theme.cursor))
            .expect("a cursor run");
        assert_eq!(&text[range.clone()], "ç");
    }

    #[test]
    fn runs_are_sorted_and_non_overlapping() {
        let theme = Theme::dark();
        // gpui expects ordered, disjoint runs; overlapping ones paint unpredictably.
        let (_, runs) = line_runs(
            "public function name() { return 'x'; }",
            0,
            &[
                span(0..6, HighlightStyle::Keyword),
                span(7..15, HighlightStyle::Keyword),
                span(32..35, HighlightStyle::String),
            ],
            &theme,
            Some(8),
        );

        for pair in runs.windows(2) {
            assert!(
                pair[0].range_end() <= pair[1].0.start,
                "runs must be sorted and disjoint: {:?} then {:?}",
                pair[0].0,
                pair[1].0
            );
        }
    }

    /// Small helper so the window assertion above reads cleanly.
    trait RangeEnd {
        fn range_end(&self) -> usize;
    }
    impl RangeEnd for (Range<usize>, GpuiHighlight) {
        fn range_end(&self) -> usize {
            self.0.end
        }
    }

    #[test]
    fn an_empty_line_produces_no_runs() {
        let theme = Theme::dark();
        let (text, runs) = line_runs("", 0, &[], &theme, None);
        assert_eq!(text, "");
        assert!(runs.is_empty());
    }
}
