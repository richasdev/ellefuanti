//! The gpui view that renders a [`Document`] and turns input into edits.
//!
//! Deliberately thin: every editing *semantic* lives in `Document` (plain Rust, unit
//! tested). This file translates input into `Document` calls and `Document` state into
//! elements, and owns nothing else.

use std::ops::Range;

use elle_syntax::HighlightSpan;
use elle_text::Point;
use gpui::{
    App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
    HighlightStyle as GpuiHighlight, KeyDownEvent, MouseButton, MouseDownEvent, Pixels,
    ScrollStrategy, SharedString, StyledText, TextRun, UniformListScrollHandle, Window, div,
    prelude::*, px, uniform_list,
};

use crate::actions::{
    Backspace, Copy, Cut, Delete, DeleteLine, DeleteToLineEnd, DeleteToLineStart, DeleteWordLeft,
    DeleteWordRight, DuplicateLineDown, DuplicateLineUp, Indent, MoveDocumentEnd,
    MoveDocumentStart, MoveDown, MoveLeft, MoveLineDown, MoveLineEnd, MoveLineStart, MoveLineUp,
    MoveRight, MoveUp, MoveWordLeft, MoveWordRight, Newline, OpenLineAbove, OpenLineBelow, Outdent,
    Paste, Redo, SelectAll, SelectDocumentEnd, SelectDocumentStart, SelectDown, SelectLeft,
    SelectLineEnd, SelectLineStart, SelectRight, SelectUp, SelectWordLeft, SelectWordRight, Tab,
    ToggleComment, Undo, context,
};
use crate::editor::state::Document;
use crate::fonts::Fonts;
use crate::lsp_session::Severity;
use crate::theme::{Metrics, Theme, Themed};

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
    /// Diagnostics for this file, as byte ranges in the current buffer.
    ///
    /// A copy pushed in by the workspace rather than a handle the editor reads from: the
    /// workspace owns the language server, and an editor holding a reference to it would
    /// make "the server died" a thing every open tab has to cope with. A `Vec` it was
    /// handed cannot go stale in a way that matters — it is simply the last thing the
    /// server said, and an empty one is the correct rendering when there is no server.
    diagnostics: Vec<(Range<usize>, Severity)>,
    /// Window x where the text column actually begins, measured at prepaint.
    ///
    /// `MouseDownEvent::position` is window-relative, so mapping a click to a column needs
    /// the text's window-relative origin. That is *not* the gutter width: every row sits
    /// inside the activity bar and the sidebar too. Guessing it from the constants the
    /// workspace happens to use today would re-introduce the same class of bug the moment
    /// a panel is added, resized or collapsed, so it is measured from the laid-out element
    /// instead. `None` until the first prepaint, which is before any click can arrive.
    text_origin_x: Option<Pixels>,
}

impl EditorView {
    pub fn new(document: Document, cx: &mut Context<Self>) -> Self {
        Self {
            document,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            visible_rows: 0..0,
            diagnostics: Vec::new(),
            text_origin_x: None,
        }
    }

    /// Replaces the diagnostics painted over this document.
    ///
    /// Called by the workspace when the server publishes, and with an empty slice when the
    /// server goes away — see `Lsp::shut_down` for why clearing matters more than it looks.
    pub fn set_diagnostics(&mut self, diagnostics: Vec<(Range<usize>, Severity)>) {
        self.diagnostics = diagnostics;
    }

    /// Puts the cursor at `target` and scrolls it on screen.
    ///
    /// The one place a navigation lands, so every jump — a route, a definition, a
    /// reference, a symbol — leaves the editor in the same state. `point_to_offset` clamps
    /// both row and column, which is what makes a stale target (a line number from an
    /// index built before the file shrank) a cursor at the end of the file rather than a
    /// panic.
    pub fn reveal(&mut self, target: Point) {
        let offset = self.document.buffer.point_to_offset(target);
        self.document.move_to(offset, false);
        self.scroll_cursor_into_view();
    }

    /// Where the text column starts, as measured at prepaint.
    ///
    /// Exposed for the render tests: the click arithmetic depends on this being a *measured*
    /// value rather than a constant, and a test that cannot read it can only check that
    /// clicking does not crash.
    #[cfg(test)]
    pub fn text_origin_x_for_test(&self) -> Option<Pixels> {
        self.text_origin_x
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

        // Brackets and quotes first: wrapping a selection, typing over a closer, and
        // auto-closing all replace the plain insert. `insert_with_pairs` reports whether it
        // handled the keystroke rather than deciding here, because what counts as a pair is
        // domain knowledge and `Document` is where that lives.
        if !self.document.insert_with_pairs(text) {
            self.document.insert(text);
        }
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
        self.document.newline_with_indent();
        self.after_edit(cx);
    }

    fn toggle_comment(&mut self, _: &ToggleComment, _w: &mut Window, cx: &mut Context<Self>) {
        // The false case is JSON, which has no comment syntax. Nothing happens, and that is
        // the whole behaviour — see `Language::line_comment`.
        self.document.toggle_comment();
        self.after_edit(cx);
    }

    fn tab(&mut self, _: &Tab, _w: &mut Window, cx: &mut Context<Self>) {
        // With a selection ⇥ shifts the whole block right; with a bare cursor it types.
        // ponytail: four spaces, which is PSR-12 and therefore right for Laravel. Reads
        // indent settings once a settings crate exists (Milestone 1 task 15+).
        if self.document.selection.is_empty() {
            self.document.insert("    ");
        } else {
            self.document.indent_lines(false);
        }
        self.after_edit(cx);
    }

    fn indent(&mut self, _: &Indent, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.indent_lines(false);
        self.after_edit(cx);
    }

    fn outdent(&mut self, _: &Outdent, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.indent_lines(true);
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
        self.document.move_line_home(false);
        self.after_move(cx);
    }

    fn move_line_end(&mut self, _: &MoveLineEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_line_end(false);
        self.after_move(cx);
    }

    fn move_word_left(&mut self, _: &MoveWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_word(false, false);
        self.after_move(cx);
    }

    fn move_word_right(&mut self, _: &MoveWordRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_word(true, false);
        self.after_move(cx);
    }

    fn move_document_start(
        &mut self,
        _: &MoveDocumentStart,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document.move_document_edge(false, false);
        self.after_move(cx);
    }

    fn move_document_end(&mut self, _: &MoveDocumentEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_document_edge(true, false);
        self.after_move(cx);
    }

    fn select_line_start(&mut self, _: &SelectLineStart, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_line_home(true);
        self.after_move(cx);
    }

    fn select_line_end(&mut self, _: &SelectLineEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_line_end(true);
        self.after_move(cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_word(false, true);
        self.after_move(cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_word(true, true);
        self.after_move(cx);
    }

    fn select_document_start(
        &mut self,
        _: &SelectDocumentStart,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document.move_document_edge(false, true);
        self.after_move(cx);
    }

    fn select_document_end(
        &mut self,
        _: &SelectDocumentEnd,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document.move_document_edge(true, true);
        self.after_move(cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.delete_word(false);
        self.after_edit(cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.delete_word(true);
        self.after_edit(cx);
    }

    fn delete_to_line_start(
        &mut self,
        _: &DeleteToLineStart,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document.delete_to_line_edge(false);
        self.after_edit(cx);
    }

    fn delete_to_line_end(&mut self, _: &DeleteToLineEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.delete_to_line_edge(true);
        self.after_edit(cx);
    }

    fn move_line_up(&mut self, _: &MoveLineUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_lines(false);
        self.after_edit(cx);
    }

    fn move_line_down(&mut self, _: &MoveLineDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.move_lines(true);
        self.after_edit(cx);
    }

    fn duplicate_line_up(&mut self, _: &DuplicateLineUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.duplicate_lines(false);
        self.after_edit(cx);
    }

    fn duplicate_line_down(
        &mut self,
        _: &DuplicateLineDown,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document.duplicate_lines(true);
        self.after_edit(cx);
    }

    fn delete_line(&mut self, _: &DeleteLine, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.delete_lines();
        self.after_edit(cx);
    }

    fn open_line_below(&mut self, _: &OpenLineBelow, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.open_line(false);
        self.after_edit(cx);
    }

    fn open_line_above(&mut self, _: &OpenLineAbove, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.open_line(true);
        self.after_edit(cx);
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
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let fonts = Fonts::get(cx);
        let line = self.document.buffer.line(row);
        let x = text_local_x(event.position.x, self.text_origin_x, &fonts);

        let column = if line.is_empty() || x <= px(0.0) {
            0
        } else {
            let measured = &line[..line.len().min(MAX_MEASURE_BYTES)];
            let runs = [TextRun {
                len: measured.len(),
                font: fonts.font(),
                color: gpui::white(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            // closest_index_for_x clamps, unlike index_for_x which returns None past the
            // end of the line — clamping is what a click past end-of-line should do.
            window
                .text_system()
                .layout_line(measured, fonts.size, &runs, None)
                .closest_index_for_x(x)
        };

        let offset = self.document.buffer.point_to_offset(Point::new(row, column));
        // Shift-click extends the existing selection, matching every other editor.
        self.document.move_to(offset, event.modifiers.shift);

        // ⌘click is go-to-definition, the way it is in every IDE. The cursor moves first
        // either way, so a ⌘click that finds nothing still behaves like the ordinary click
        // it also was — and the workspace, not the editor, owns the language server.
        if event.modifiers.platform {
            cx.emit(EditorEvent::GoToDefinition);
        }
        cx.notify();
    }
}

/// What the editor tells the workspace.
///
/// One variant, because there is one thing an editor knows that the workspace has to act
/// on: the user asked to navigate from a place only the editor knows the coordinates of.
/// An event rather than a call into the workspace keeps the editor unaware that a language
/// server exists — the same split `set_diagnostics` already has in the other direction.
pub enum EditorEvent {
    GoToDefinition,
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fonts = Fonts::get(cx);
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
            .on_action(cx.listener(Self::move_word_left))
            .on_action(cx.listener(Self::move_word_right))
            .on_action(cx.listener(Self::move_document_start))
            .on_action(cx.listener(Self::move_document_end))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_document_start))
            .on_action(cx.listener(Self::select_document_end))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::delete_to_line_end))
            .on_action(cx.listener(Self::move_line_up))
            .on_action(cx.listener(Self::move_line_down))
            .on_action(cx.listener(Self::duplicate_line_up))
            .on_action(cx.listener(Self::duplicate_line_down))
            .on_action(cx.listener(Self::delete_line))
            .on_action(cx.listener(Self::open_line_below))
            .on_action(cx.listener(Self::open_line_above))
            .on_action(cx.listener(Self::indent))
            .on_action(cx.listener(Self::outdent))
            .on_action(cx.listener(Self::toggle_comment))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .size_full()
            .bg(theme.background)
            .font_family(fonts.family.clone())
            .text_size(fonts.size)
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

        let theme = cx.theme().clone();
        let fonts = Fonts::get(cx);
        let selection = self.document.selection.range();

        // Once per frame, not once per row: the lookup is a tree descent, and the answer is
        // the same for every visible row. Both offsets are document-wide; `line_runs` clips
        // them the same way it clips a syntax span.
        let brackets = self.document.matching_bracket();

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

                // Sliced per row rather than passed whole: `line_runs` would otherwise
                // scan every diagnostic in the file for each of the ~40 visible rows.
                // A file with hundreds of problems is exactly when that starts to matter.
                let row_diagnostics: Vec<_> = self
                    .diagnostics
                    .iter()
                    .filter(|(range, _)| range.start < line_end && range.end > line_start)
                    .cloned()
                    .collect();

                let is_cursor_row = row == cursor.row;
                let row_selected = !selection.is_empty()
                    && selection.start < line_end.max(line_start + 1)
                    && selection.end > line_start;
                let entity = entity.clone();

                let measuring_entity = entity.clone();

                div()
                    // The click handler needs the window x where the text starts, and only
                    // the layout engine knows it. Children are [gutter, text]; the second
                    // one's origin is the answer. Registered before `.id()` because
                    // `on_children_prepainted` is a `Div` method and `.id()` wraps the Div
                    // in a `Stateful`, which does not forward it.
                    .on_children_prepainted(move |bounds, _window, cx| {
                        if let Some(text) = bounds.get(1) {
                            measuring_entity.update(cx, |editor, _cx| {
                                editor.text_origin_x = Some(text.origin.x);
                            });
                        }
                    })
                    .id(("row", row))
                    .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        let event = event.clone();
                        entity.update(cx, |editor, cx| {
                            editor.on_row_mouse_down(&event, row, window, cx);
                        });
                    })
                    .flex()
                    .h(fonts.line_height())
                    .w_full()
                    .when(row_selected, |el| el.bg(theme.selection))
                    .when(is_cursor_row && !row_selected, |el| el.bg(theme.hover))
                    .child(
                        // Gutter. Right-aligned so digits line up as numbers grow.
                        div()
                            .w(fonts.gutter_width())
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
                        &row_diagnostics,
                        brackets,
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
    diagnostics: &[(Range<usize>, Severity)],
    brackets: Option<(usize, usize)>,
    theme: &Theme,
    cursor_column: Option<usize>,
) -> StyledText {
    let (text, highlights) =
        line_runs(line, line_start, spans, diagnostics, brackets, theme, cursor_column);
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
    diagnostics: &[(Range<usize>, Severity)],
    brackets: Option<(usize, usize)>,
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

    // Diagnostics underline; they do not recolour. A squiggle that also repainted the
    // text would fight the syntax highlighting for the same bytes and win, so an error on
    // a keyword would turn it red and lose the one cue that says it is a keyword. gpui
    // composes `underline` with a `color` from an earlier run only if they are the *same*
    // run, so each diagnostic range is merged into the runs already covering it rather
    // than pushed as a competing one.
    for (range, severity) in diagnostics {
        if range.end <= line_start || range.start >= line_end {
            continue;
        }
        let start = floor_boundary(&text, range.start.max(line_start) - line_start);
        // A zero-width diagnostic (a server pointing *between* two characters) would be
        // invisible; widen it to one character so there is something to underline.
        let end = ceil_boundary(&text, (range.end.min(line_end) - line_start).max(start + 1));
        if start >= end {
            continue;
        }

        let underline = Some(gpui::UnderlineStyle {
            color: Some(theme.diagnostic(*severity)),
            thickness: px(1.0),
            wavy: true,
        });

        highlights = merge_underline(highlights, start..end, underline);
    }

    // Indent guides and the trailing-whitespace tint, both computed from this line's own
    // bytes and nothing else — no buffer scan, no state carried between rows. That is what
    // keeps them viewport-scoped: `render_rows` calls this once per *visible* row, so the
    // cost is the same for a 50-line file and a 50,000-line one.
    //
    // They go in before the cursor so the cursor's `retain` below removes any that overlap
    // it, which is the same precedence the syntax colours already get.
    //
    // Each is dropped where a run already exists rather than splitting one. Whitespace is
    // normally uncoloured, so this almost never fires — but a multi-line block comment
    // *does* span a line's own indent, and a guide pushed on top of it would be a second
    // run over the same bytes, which gpui paints unpredictably. Losing a guide inside a
    // comment is invisible; an overlapping run is not.
    let mut decorations: Vec<(Range<usize>, GpuiHighlight)> = Vec::new();
    for range in indent_guide_columns(&text) {
        decorations.push((
            range,
            GpuiHighlight { background_color: Some(theme.indent_guide), ..Default::default() },
        ));
    }

    // `line`, not `text`: the padding space a cursor at end-of-line adds is not the file's
    // trailing whitespace, and tinting it would mark every line the cursor rests on.
    if let Some(range) = trailing_whitespace_range(line) {
        decorations.push((
            range,
            GpuiHighlight {
                background_color: Some(theme.trailing_whitespace),
                ..Default::default()
            },
        ));
    }

    decorations.retain(|(range, _)| {
        highlights.iter().all(|(taken, _)| taken.end <= range.start || taken.start >= range.end)
    });
    highlights.append(&mut decorations);

    // The matching pair. Unlike the guides this one *must* win over the syntax colour
    // underneath it — a bracket is nearly always inside a coloured node, so dropping it on
    // overlap would mean never drawing it at all. Same `retain`-then-push shape the cursor
    // uses below, and for the same reason.
    if let Some((a, b)) = brackets {
        for at in [a, b] {
            if at < line_start || at >= line_end {
                continue;
            }
            let start = floor_boundary(&text, at - line_start);
            let end = ceil_boundary(&text, (start + 1).min(text.len()));
            if start >= end {
                continue;
            }
            highlights.retain(|(range, _)| range.end <= start || range.start >= end);
            highlights.push((
                start..end,
                GpuiHighlight { background_color: Some(theme.selection), ..Default::default() },
            ));
        }
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

/// One byte range per indent guide: the single space at columns 4, 8, 12… of the indent.
///
/// A guide is drawn as a background on one space rather than as a positioned rule, for the
/// same reason the cursor is a background and not a caret (see [`styled_line`]): no absolute
/// layout and no measuring, so it stays put on a proportional fallback font.
///
/// Only *leading* whitespace produces guides, and only where the indent actually reaches
/// that column — a line indented 8 columns gets guides at 0 and 4, never at 8, because a
/// guide at the first code character would sit under the code.
///
/// ponytail: four columns, hardcoded, matching `indent_lines` and `EditorView::tab`. All of
/// them read the setting together (#60), and a guide at a width the indenter does not use
/// would be visibly wrong, so they must move as one.
///
/// Tabs are counted as one column, not expanded. A tab-indented file therefore gets a guide
/// only every fourth tab, which is under-drawn rather than wrong; expanding them properly
/// means a tab width setting, which is the same #60 change.
fn indent_guide_columns(text: &str) -> Vec<Range<usize>> {
    const INDENT: usize = 4;

    // Bytes of leading whitespace. ASCII space and tab only: any other whitespace is not
    // indentation anyone typed, and treating it as such would put a guide inside a
    // non-breaking space someone pasted.
    let indent_bytes = text.bytes().take_while(|&b| b == b' ' || b == b'\t').count();

    // A whitespace-only line has no code to align to, so guides on it would be a grid over
    // nothing. Skipping is also what makes a blank line between two blocks stay blank.
    if indent_bytes == text.len() {
        return Vec::new();
    }

    // ASCII throughout, so byte offsets are column offsets here and no boundary snapping is
    // needed — which is exactly why the take_while above excludes multi-byte whitespace.
    (0..indent_bytes).step_by(INDENT).map(|column| column..column + 1).collect()
}

/// The trailing whitespace at the end of `line`, if any.
///
/// `None` for a line that is entirely whitespace: an empty line in the middle of a function
/// is normal, and tinting every one of them turns a file into a barcode. What this is for is
/// the space left after `return $x; ` — the one that shows up as a diff hunk nobody meant.
fn trailing_whitespace_range(line: &str) -> Option<Range<usize>> {
    let trimmed = line.trim_end_matches([' ', '\t']);
    (!trimmed.is_empty() && trimmed.len() < line.len()).then(|| trimmed.len()..line.len())
}

/// Adds an underline over `span`, splitting any colour runs it partly covers.
///
/// gpui requires sorted, non-overlapping runs, so an underline cannot simply be pushed on
/// top of the syntax colours — a run half-covered by a diagnostic has to become two runs,
/// one underlined and one not. That splitting is the whole function, and it is why
/// diagnostics could not be expressed as "one more span" alongside the highlight spans:
/// those carry a colour each and never overlap, where a diagnostic overlaps by nature.
///
/// Bytes not covered by any existing run still need the underline, so the gaps inside
/// `span` become runs of their own with no colour — they inherit the element's text colour,
/// which is what an uncoloured character already renders as.
fn merge_underline(
    runs: Vec<(Range<usize>, GpuiHighlight)>,
    span: Range<usize>,
    underline: Option<gpui::UnderlineStyle>,
) -> Vec<(Range<usize>, GpuiHighlight)> {
    let mut merged: Vec<(Range<usize>, GpuiHighlight)> = Vec::with_capacity(runs.len() + 2);
    // Where inside `span` the next uncovered byte starts, so gaps between existing runs
    // get an underline-only run rather than being skipped.
    let mut uncovered = span.start;

    for (range, style) in runs {
        if range.end <= span.start || range.start >= span.end {
            merged.push((range, style));
            continue;
        }

        // The part of this run before the diagnostic keeps its style unchanged.
        if range.start < span.start {
            merged.push((range.start..span.start, style));
        }

        // Any gap since the last run, inside the diagnostic, is underline only.
        if uncovered < range.start.max(span.start) {
            merged.push((
                uncovered..range.start.max(span.start),
                GpuiHighlight { underline, ..Default::default() },
            ));
        }

        // The overlap keeps the colour and gains the underline.
        let overlap = range.start.max(span.start)..range.end.min(span.end);
        if overlap.start < overlap.end {
            merged.push((overlap.clone(), GpuiHighlight { underline, ..style }));
            uncovered = overlap.end;
        }

        // And the part after it is unchanged again.
        if range.end > span.end {
            merged.push((span.end..range.end, style));
        }
    }

    // Trailing bytes of the diagnostic that no run covered.
    if uncovered < span.end {
        merged.push((uncovered..span.end, GpuiHighlight { underline, ..Default::default() }));
    }

    merged.sort_by_key(|(range, _)| range.start);
    merged
}

/// Turns a window-relative click x into an x relative to the start of the text.
///
/// Split out as a free function because the subtraction is the whole bug: the original
/// version subtracted only the gutter width from a **window**-relative coordinate,
/// ignoring the activity bar and the sidebar the row sits inside. Every click therefore
/// resolved a column far to the right of the character under the pointer, and no click
/// could reach column 0 at all, because the smallest x a row could ever receive was
/// already well past the gutter.
///
/// `origin` is the measured text origin. Until the first prepaint there is none, and
/// [`fallback_text_origin_x`] stands in — it is the full chrome offset, not the gutter
/// alone. The result is clamped at zero so a click in the gutter, or anywhere left of the
/// text, lands on column 0 instead of going negative.
fn text_local_x(window_x: Pixels, origin: Option<Pixels>, fonts: &Fonts) -> Pixels {
    let origin = origin.unwrap_or_else(|| fallback_text_origin_x(fonts));
    (window_x - origin).max(px(0.0))
}

/// Where the text column sits before the first prepaint has measured it.
///
/// Activity bar + sidebar + gutter. A guess, but the *right* guess for the default layout,
/// where the old code's gutter-only version was out by 284 px.
///
/// Takes the fonts because the gutter is derived from the font size now (#49). A constant
/// here would be a *second* place the gutter width is decided, and the two would disagree
/// the moment someone zoomed — reintroducing the same class of bug in the fallback path,
/// which is the path nothing has measured yet.
fn fallback_text_origin_x(fonts: &Fonts) -> Pixels {
    Metrics::ACTIVITY_BAR_WIDTH + Metrics::SIDEBAR_WIDTH + fonts.gutter_width()
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

    // --- click-to-column arithmetic -------------------------------------------------

    #[test]
    fn a_click_on_the_first_character_maps_to_the_start_of_the_text() {
        // The bug this pins: the old code subtracted only the gutter width from a
        // *window*-relative x. Every row is nested inside the 44 px activity bar and the
        // 240 px sidebar, so a click on column 0 arrived at window x 336 and resolved to
        // 284 px into the line — roughly 35 columns of Menlo 13 to the right of where the
        // user actually clicked.
        let fonts = Fonts::default();
        let first_char_x =
            Metrics::ACTIVITY_BAR_WIDTH + Metrics::SIDEBAR_WIDTH + fonts.gutter_width();

        assert_eq!(
            text_local_x(first_char_x, None, &fonts),
            px(0.0),
            "a click on the first character must resolve to the start of the line"
        );
        assert_eq!(
            first_char_x - fonts.gutter_width(),
            px(284.0),
            "and the old arithmetic put it 284 px into the line instead"
        );
    }

    #[test]
    fn the_measured_origin_wins_over_the_fallback() {
        // Prepaint reports where the text really is; a collapsed sidebar or an extra panel
        // changes it, and the fallback must not override the measurement.
        let fonts = Fonts::default();
        let measured = px(96.0);
        assert_eq!(text_local_x(px(150.0), Some(measured), &fonts), px(54.0));
        assert_ne!(
            text_local_x(px(150.0), Some(measured), &fonts),
            text_local_x(px(150.0), None, &fonts),
            "the measurement must actually be used"
        );
    }

    #[test]
    fn a_click_left_of_the_text_clamps_to_zero_rather_than_going_negative() {
        // Clicking the gutter, the sidebar, or the activity bar. `Pixels` is an f32
        // newtype, so an unclamped subtraction yields a negative x rather than
        // underflowing — which `closest_index_for_x` would then resolve against, and which
        // no caller downstream is expecting.
        let fonts = Fonts::default();
        let edge = fallback_text_origin_x(&fonts) - px(1.0);
        for window_x in [px(0.0), px(10.0), px(300.0), edge] {
            let x = text_local_x(window_x, None, &fonts);
            assert_eq!(x, px(0.0), "x={window_x:?} is left of the text and must clamp to 0");
            assert!(x >= px(0.0));
        }
    }

    #[test]
    fn the_fallback_origin_is_the_whole_chrome_offset_not_just_the_gutter() {
        // The regression guard: if someone "simplifies" this back to the gutter alone, or
        // adds a panel to the left of the editor without updating the fallback, this fails.
        let fonts = Fonts::default();
        assert_eq!(
            fallback_text_origin_x(&fonts),
            Metrics::ACTIVITY_BAR_WIDTH + Metrics::SIDEBAR_WIDTH + fonts.gutter_width()
        );
        assert!(fallback_text_origin_x(&fonts) > fonts.gutter_width());
    }

    /// The gutter is derived now, so the fallback origin has to move with the font size —
    /// otherwise a zoomed editor's pre-prepaint clicks land in the wrong column, in the one
    /// path where nothing has measured the real origin yet.
    #[test]
    fn the_fallback_origin_follows_the_font_size() {
        let small = Fonts { size: px(13.0), ..Fonts::default() };
        let large = Fonts { size: px(20.0), ..Fonts::default() };

        assert!(
            fallback_text_origin_x(&large) > fallback_text_origin_x(&small),
            "a wider gutter has to push the text origin right"
        );
    }

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

    /// `line_runs` with no diagnostics, which is what every pre-existing test means.
    ///
    /// A wrapper rather than `&[]` threaded through nine call sites: those tests are about
    /// syntax colours and the cursor, and an empty diagnostics argument in each would be
    /// noise in the one place their arguments should read as the thing under test.
    fn line_runs(
        line: &str,
        line_start: usize,
        spans: &[HighlightSpan],
        theme: &Theme,
        cursor_column: Option<usize>,
    ) -> (String, Vec<(Range<usize>, GpuiHighlight)>) {
        super::line_runs(line, line_start, spans, &[], None, theme, cursor_column)
    }

    /// Byte ranges carrying a foreground colour.
    ///
    /// Filters on `color` rather than listing the backgrounds to exclude: the cursor, the
    /// indent guides, the trailing-whitespace tint and the bracket match are all
    /// background-only runs, and a filter that named each one would need editing every time
    /// a decoration is added — which is how these tests would drift from what they mean.
    /// What every caller below actually asks is "which bytes got a syntax colour".
    fn coloured(runs: &[(Range<usize>, GpuiHighlight)]) -> Vec<Range<usize>> {
        runs.iter()
            .filter(|(_, style)| style.color.is_some() && style.background_color.is_none())
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
        assert_eq!(coloured(&runs), vec![4..10]);
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
        assert!(coloured(&runs).is_empty());
    }

    #[test]
    fn a_span_straddling_the_line_is_clipped_to_it() {
        let theme = Theme::dark();
        // A block comment opening on an earlier line and closing on a later one: the
        // visible part must still colour, clipped at both ends rather than overflowing.
        let line = "still inside";
        let (text, runs) =
            line_runs(line, 100, &[span(50..200, HighlightStyle::Comment)], &theme, None);

        assert_eq!(coloured(&runs), vec![0..line.len()]);
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

    // --- indent guides, trailing whitespace, bracket match ---------------------------
    //
    // All three are background-only runs, so they are read back by their colour. Same
    // reasoning as the blocks above: `StyledText` is opaque, and these assert the runs.

    /// Byte ranges painted with `background`.
    fn backgrounds(
        runs: &[(Range<usize>, GpuiHighlight)],
        background: gpui::Hsla,
    ) -> Vec<Range<usize>> {
        runs.iter()
            .filter(|(_, style)| style.background_color == Some(background))
            .map(|(range, _)| range.clone())
            .collect()
    }

    #[test]
    fn indent_guides_land_at_every_fourth_column_of_the_indent() {
        let theme = Theme::dark();
        // Eight spaces then code: guides at columns 0 and 4, and *not* at 8, which is the
        // first code character.
        let (_, runs) = line_runs("        return $x;", 0, &[], &theme, None);
        assert_eq!(backgrounds(&runs, theme.indent_guide), vec![0..1, 4..5]);
    }

    #[test]
    fn an_unindented_line_gets_no_guides() {
        let theme = Theme::dark();
        let (_, runs) = line_runs("class User {", 0, &[], &theme, None);
        assert!(backgrounds(&runs, theme.indent_guide).is_empty());
    }

    #[test]
    fn a_blank_line_gets_no_guides_and_no_trailing_tint() {
        // A whitespace-only line has no code to align to, and tinting every blank line
        // between two methods turns the file into a barcode.
        let theme = Theme::dark();
        for line in ["", "    ", "\t\t"] {
            let (_, runs) = line_runs(line, 0, &[], &theme, None);
            assert!(backgrounds(&runs, theme.indent_guide).is_empty(), "{line:?}");
            assert!(backgrounds(&runs, theme.trailing_whitespace).is_empty(), "{line:?}");
        }
    }

    #[test]
    fn trailing_whitespace_is_tinted_and_nothing_else_is() {
        let theme = Theme::dark();
        let line = "return $x;   ";
        let (_, runs) = line_runs(line, 0, &[], &theme, None);
        assert_eq!(backgrounds(&runs, theme.trailing_whitespace), vec![10..13]);

        // A line with none must get none, or the tint means nothing.
        let (_, runs) = line_runs("return $x;", 0, &[], &theme, None);
        assert!(backgrounds(&runs, theme.trailing_whitespace).is_empty());
    }

    #[test]
    fn the_space_padded_in_for_a_cursor_at_end_of_line_is_not_trailing_whitespace() {
        // `line_runs` appends a space so an end-of-line cursor has a cell. That space is
        // this function's own scaffolding, not the file's content — tinting it would mark
        // every line the cursor rests on as having trailing whitespace.
        let theme = Theme::dark();
        let (text, runs) = line_runs("return $x;", 0, &[], &theme, Some(10));
        assert_eq!(text, "return $x; ", "the padding is there");
        assert!(backgrounds(&runs, theme.trailing_whitespace).is_empty());
    }

    #[test]
    fn a_guide_never_splits_a_multibyte_character() {
        // Guides are computed from leading ASCII whitespace only, so a line whose *code*
        // is accented still produces boundary-safe ranges. Asserting it because a guide
        // range landing mid-codepoint would make StyledText debug-assert.
        let theme = Theme::dark();
        let line = "    $m = 'ação';";
        let (text, runs) = line_runs(line, 0, &[], &theme, None);

        assert_eq!(backgrounds(&runs, theme.indent_guide), vec![0..1]);
        for (range, _) in &runs {
            assert!(
                text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
                "run {range:?} splits a codepoint in {text:?}"
            );
        }
    }

    #[test]
    fn decorations_never_overlap_a_syntax_run() {
        // gpui needs sorted, disjoint runs. A multi-line block comment covers a line's own
        // indent, and a guide pushed on top of it would be a second run over those bytes.
        let theme = Theme::dark();
        let line = "        still in the comment   ";
        let (_, runs) =
            line_runs(line, 0, &[span(0..line.len(), HighlightStyle::Comment)], &theme, None);

        for pair in runs.windows(2) {
            assert!(
                pair[0].0.end <= pair[1].0.start,
                "runs must be sorted and disjoint: {:?} then {:?}",
                pair[0].0,
                pair[1].0
            );
        }
        assert!(
            backgrounds(&runs, theme.indent_guide).is_empty(),
            "the guide is dropped rather than splitting the comment run"
        );
    }

    #[test]
    fn every_theme_makes_the_guide_visible_against_its_own_background() {
        // The rule #82 names: a guide colour that works on 0x282c34 is invisible on
        // 0xffffff. Each variant has to differ from its *own* background, and stay quieter
        // than its own text — which is what "a guide, not a character" means.
        for theme in [
            Theme::dark(),
            Theme::light(),
            Theme::one_dark_pro(),
            Theme::github_dark(),
            Theme::github_light(),
        ] {
            assert_ne!(theme.indent_guide, theme.background, "a guide equal to the background");
            assert_ne!(theme.trailing_whitespace, theme.background, "an invisible tint");
            assert_ne!(theme.indent_guide, theme.text, "a guide as loud as the text");

            // And the guide is closer to the background than the text is, in both
            // directions — the light themes run darker, the dark ones lighter.
            let distance = |a: gpui::Hsla, b: gpui::Hsla| (a.l - b.l).abs();
            assert!(
                distance(theme.indent_guide, theme.background)
                    < distance(theme.text, theme.background),
                "the guide must be quieter than the text"
            );
        }
    }

    #[test]
    fn the_matching_pair_is_painted_on_both_brackets() {
        let theme = Theme::dark();
        // Both ends on the same line, rebased from document offsets like any other range.
        let (_, runs) = line_runs_with_brackets("f(1)", 0, Some((1, 3)), &theme);
        assert_eq!(backgrounds(&runs, theme.selection), vec![1..2, 3..4]);
    }

    #[test]
    fn a_partner_on_another_line_paints_only_the_end_that_is_here() {
        // The common case: a `{` at the top of a method and its `}` forty lines down. Each
        // row must paint its own end and clip the other, exactly as a syntax span does.
        let theme = Theme::dark();
        let (_, runs) = line_runs_with_brackets("}", 500, Some((12, 500)), &theme);
        assert_eq!(backgrounds(&runs, theme.selection), vec![0..1]);
    }

    #[test]
    fn the_bracket_highlight_wins_over_the_syntax_colour_beneath_it() {
        // A bracket is nearly always inside a coloured node, so dropping it on overlap —
        // which is what the guides do — would mean never drawing it at all.
        let theme = Theme::dark();
        let (_, runs) = super::line_runs(
            "f(1)",
            0,
            &[span(0..4, HighlightStyle::Function)],
            &[],
            Some((1, 3)),
            &theme,
            None,
        );

        assert_eq!(backgrounds(&runs, theme.selection), vec![1..2, 3..4]);
        for pair in runs.windows(2) {
            assert!(pair[0].0.end <= pair[1].0.start, "runs must stay disjoint");
        }
    }

    /// `line_runs` with a bracket pair and nothing else.
    fn line_runs_with_brackets(
        line: &str,
        line_start: usize,
        brackets: Option<(usize, usize)>,
        theme: &Theme,
    ) -> (String, Vec<(Range<usize>, GpuiHighlight)>) {
        super::line_runs(line, line_start, &[], &[], brackets, theme, None)
    }

    // --- diagnostics ----------------------------------------------------------------
    //
    // Same reasoning as the block above: `StyledText` is opaque, so these assert on the
    // runs. What they cover is the part that composes — a squiggle has to coexist with the
    // syntax colour under it, and gpui only honours both if they end up in the *same* run.

    /// Runs carrying a wavy underline, with the colour it was drawn in.
    fn underlined(
        runs: &[(Range<usize>, GpuiHighlight)],
    ) -> Vec<(Range<usize>, Option<gpui::Hsla>)> {
        runs.iter()
            .filter(|(_, style)| style.underline.is_some_and(|u| u.wavy))
            .map(|(range, style)| (range.clone(), style.underline.unwrap().color))
            .collect()
    }

    #[test]
    fn a_diagnostic_underlines_its_range_in_the_severity_colour() {
        let theme = Theme::dark();
        let (_, runs) =
            line_runs_with("$undefined = 1;", 0, &[], &[(0..10, Severity::Error)], &theme, None);

        assert_eq!(underlined(&runs), vec![(0..10, Some(theme.error))]);
    }

    #[test]
    fn every_severity_gets_its_own_colour_from_the_theme() {
        // The rule from the brief: diagnostic colours come from `Theme`, never hardcoded.
        // If someone inlines an `rgb(0xff0000)` here, this fails against every variant.
        for theme in [Theme::dark(), Theme::light(), Theme::one_dark_pro()] {
            for (severity, expected) in [
                (Severity::Error, theme.error),
                (Severity::Warning, theme.warning),
                (Severity::Information, theme.information),
                (Severity::Hint, theme.hint),
            ] {
                let (_, runs) = line_runs_with("abcd", 0, &[], &[(0..4, severity)], &theme, None);
                assert_eq!(
                    underlined(&runs),
                    vec![(0..4, Some(expected))],
                    "{severity:?} must use the theme's own colour"
                );
            }
        }
    }

    #[test]
    fn a_squiggle_keeps_the_syntax_colour_underneath_it() {
        // The composition case, and the reason diagnostics are not just another span. An
        // error on a keyword must stay keyword-coloured *and* gain an underline — if the
        // squiggle replaced the colour, the one cue that says "this is a keyword" is lost
        // exactly where the user is being told to look.
        let theme = Theme::dark();
        let (_, runs) = line_runs_with(
            "return $x;",
            0,
            &[span(0..6, HighlightStyle::Keyword)],
            &[(0..6, Severity::Error)],
            &theme,
            None,
        );

        let (range, style) = runs.iter().find(|(r, _)| r.start == 0).expect("a run at 0");
        assert_eq!(*range, 0..6);
        assert_eq!(style.color, Some(theme.keyword), "the syntax colour must survive");
        assert_eq!(
            style.underline.and_then(|u| u.color),
            Some(theme.error),
            "and the underline must be there too"
        );
    }

    #[test]
    fn a_diagnostic_partly_covering_a_span_splits_it() {
        // gpui needs sorted, disjoint runs. A diagnostic over half a keyword has to become
        // two runs — underlined and not — or the whole word gets a squiggle it did not earn.
        let theme = Theme::dark();
        let (_, runs) = line_runs_with(
            "function name",
            0,
            &[span(0..8, HighlightStyle::Keyword)],
            &[(4..8, Severity::Warning)],
            &theme,
            None,
        );

        assert_eq!(underlined(&runs), vec![(4..8, Some(theme.warning))]);
        // And the first half kept its colour with no underline.
        let plain = runs.iter().find(|(r, _)| *r == (0..4)).expect("the unmarked half");
        assert_eq!(plain.1.color, Some(theme.keyword));
        assert!(plain.1.underline.is_none());
    }

    #[test]
    fn a_diagnostic_over_uncoloured_text_still_underlines() {
        // Plain text carries no syntax span, so there is no run to merge into. The gap has
        // to become a run of its own or the squiggle silently does not render.
        let theme = Theme::dark();
        let (_, runs) =
            line_runs_with("plain words", 0, &[], &[(6..11, Severity::Hint)], &theme, None);

        assert_eq!(underlined(&runs), vec![(6..11, Some(theme.hint))]);
    }

    #[test]
    fn diagnostic_runs_stay_sorted_and_disjoint() {
        // The invariant gpui enforces. Several diagnostics over several spans is where a
        // naive merge produces overlaps that paint unpredictably.
        let theme = Theme::dark();
        let (_, runs) = line_runs_with(
            "public function name() { return 'x'; }",
            0,
            &[
                span(0..6, HighlightStyle::Keyword),
                span(7..15, HighlightStyle::Keyword),
                span(32..35, HighlightStyle::String),
            ],
            &[(3..10, Severity::Error), (30..36, Severity::Warning)],
            &theme,
            Some(20),
        );

        for pair in runs.windows(2) {
            assert!(
                pair[0].0.end <= pair[1].0.start,
                "runs must be sorted and disjoint: {:?} then {:?}",
                pair[0].0,
                pair[1].0
            );
        }
    }

    #[test]
    fn a_diagnostic_on_another_line_is_not_painted_on_this_one() {
        // Ranges are document-wide byte offsets; forgetting to clip paints one file's
        // squiggle across every visible row.
        let theme = Theme::dark();
        let (_, runs) =
            line_runs_with("this line", 100, &[], &[(0..50, Severity::Error)], &theme, None);

        assert!(underlined(&runs).is_empty());
    }

    #[test]
    fn a_multiline_diagnostic_is_clipped_to_the_visible_line() {
        let theme = Theme::dark();
        let line = "middle";
        let (_, runs) = line_runs_with(line, 100, &[], &[(50..200, Severity::Error)], &theme, None);

        assert_eq!(underlined(&runs), vec![(0..line.len(), Some(theme.error))]);
    }

    #[test]
    fn a_zero_width_diagnostic_still_shows_something() {
        // A server pointing *between* two characters — "expected ; here". An empty range
        // would produce no run at all, so the user is told nothing.
        let theme = Theme::dark();
        let (_, runs) = line_runs_with("$x = 1", 0, &[], &[(3..3, Severity::Error)], &theme, None);

        let marks = underlined(&runs);
        assert_eq!(marks.len(), 1, "a zero-width diagnostic must still be visible");
        assert!(!marks[0].0.is_empty());
    }

    #[test]
    fn a_squiggle_never_splits_a_multibyte_character() {
        // The same hazard the syntax spans have: a range landing mid-codepoint would make
        // StyledText debug-assert. Portuguese source is where this actually happens.
        let theme = Theme::dark();
        let line = "$mensagem = 'ação';";
        let (text, runs) = line_runs_with(line, 0, &[], &[(13..17, Severity::Error)], &theme, None);

        for (range, _) in &runs {
            assert!(
                text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
                "run {range:?} splits a codepoint in {text:?}"
            );
        }
    }

    #[test]
    fn no_diagnostics_produces_exactly_what_it_did_before() {
        // The regression guard for everything above: adding diagnostics must not change a
        // single run when there are none, which is the state every editor without a
        // language server is permanently in.
        let theme = Theme::dark();
        let spans = [span(0..6, HighlightStyle::Keyword)];
        let (text, with) = line_runs_with("return $x;", 0, &spans, &[], &theme, Some(2));
        let (plain_text, without) = line_runs("return $x;", 0, &spans, &theme, Some(2));

        assert_eq!(text, plain_text);
        assert_eq!(with.len(), without.len());
        for (a, b) in with.iter().zip(&without) {
            assert_eq!(a.0, b.0);
            assert_eq!(a.1.color, b.1.color);
            assert!(a.1.underline.is_none());
        }
    }

    /// `line_runs` with diagnostics, spelled out.
    fn line_runs_with(
        line: &str,
        line_start: usize,
        spans: &[HighlightSpan],
        diagnostics: &[(Range<usize>, Severity)],
        theme: &Theme,
        cursor_column: Option<usize>,
    ) -> (String, Vec<(Range<usize>, GpuiHighlight)>) {
        super::line_runs(line, line_start, spans, diagnostics, None, theme, cursor_column)
    }
}
