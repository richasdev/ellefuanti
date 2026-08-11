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
    ScrollStrategy, SharedString, TextRun, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
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
use crate::editor::line::Line;
use crate::editor::state::Document;
use crate::fonts::Fonts;
use crate::lsp_session::Severity;
use crate::theme::{Metrics, Theme, Themed};

/// How much text may be measured to map a click to a column.
///
/// Guards against a pathological single-line file (a minified asset) making one click
/// shape a megabyte of text on the UI thread.
const MAX_MEASURE_BYTES: usize = 4096;

/// A diagnostic the mouse is resting on, ready to be drawn as a card.
#[derive(Clone, PartialEq)]
pub struct HoverDiagnostic {
    pub message: SharedString,
    /// Window coordinates for the card's top-left — under the mouse, one line down, so the
    /// card never covers the text it is about.
    pub position: gpui::Point<Pixels>,
    /// Which row produced it, so the row's hover-out can clear its own card without racing
    /// the neighbouring row's hover-in.
    pub row: usize,
}

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
    diagnostics: Vec<(Range<usize>, Severity, SharedString)>,
    /// The word under a ⌘-hover, underlined as a clickable link (#81's polish).
    ///
    /// Byte range in the buffer. `Some` only while ⌘ is held and the pointer rests on a
    /// word — punctuation and whitespace promise nothing, so they hint nothing. Cleared
    /// when ⌘ lifts, when the mouse leaves the rows, and on scroll, the same lifecycle as
    /// the hover card. An edit while ⌘ is held and the mouse is still can leave the
    /// underline one edit stale until the next mouse move; recomputing per keystroke for
    /// that corner is not worth wiring every edit path through here.
    link_hint: Option<Range<usize>>,
    /// The diagnostic under the mouse, if any: its message and where to draw the card.
    ///
    /// Editor-owned because the editor is the only thing that can turn a mouse position
    /// into a byte offset; workspace-rendered because the card must sit at *window*
    /// coordinates, above every panel — the same split the completion popup uses, where
    /// the editor measures and the workspace places.
    pub hover_diagnostic: Option<HoverDiagnostic>,
    /// Window x where the text column actually begins, measured at prepaint.
    ///
    /// `MouseDownEvent::position` is window-relative, so mapping a click to a column needs
    /// the text's window-relative origin. That is *not* the gutter width: every row sits
    /// inside the activity bar and the sidebar too. Guessing it from the constants the
    /// workspace happens to use today would re-introduce the same class of bug the moment
    /// a panel is added, resized or collapsed, so it is measured from the laid-out element
    /// instead. `None` until the first prepaint, which is before any click can arrive.
    text_origin_x: Option<Pixels>,
    /// Window y of the cursor's row, measured at prepaint (#61).
    ///
    /// The completion popup anchors to it. Measured rather than computed from the row index
    /// for two reasons that are both already load-bearing above: `uniform_list` scrolls, so
    /// the absolute row says nothing about where it is on screen, and the chrome above the
    /// editor (tab bar, and a find bar whose height varies with its replace field) is the
    /// workspace's business, not something this view can add up from constants.
    ///
    /// `None` when the cursor's row was not painted in the last frame — it is scrolled out
    /// of view, and there is no on-screen caret for a popup to sit under.
    cursor_row_origin_y: Option<Pixels>,
    /// Whether the caret is in its visible half of the blink cycle.
    ///
    /// Starts `true` so a freshly-opened editor shows a caret before the first tick.
    caret_visible: bool,
    /// The blink loop. Dropping it cancels it, which is the whole cancellation mechanism —
    /// see [`EditorView::stop_blinking`]. `None` means the caret is held solid: either
    /// nothing is focused, or the user is mid-keystroke.
    blink: Option<gpui::Task<()>>,
    /// Holds the window-activation observer that stops the blink when the window
    /// deactivates. Registered on first render for the same reason `WorkspaceView` does it:
    /// `observe_window_activation` needs a `&mut Window`, and `new` has none.
    window_activation: Option<gpui::Subscription>,
}

/// Half the blink period: the caret is shown for this long, then hidden for this long.
///
/// 530ms is the macOS system default (`NSTextInsertionPointBlinkPeriod`), so the editor's
/// caret beats in time with every native text field on the screen rather than slightly
/// against them.
///
/// **This interval is the entire cost of the feature.** Each tick is a `cx.notify()`, and
/// gpui has no partial repaint — `App::notify` sets the window's `dirty` flag and the next
/// frame redraws the whole window (`window.rs`, `WindowInvalidator::invalidate_view`).
/// There is no damage-rect API at the platform layer to reach for instead. So the lever
/// available is *frequency*, not *area*: ~1.9 repaints/second while the caret is idle and
/// focused, against the 60/second `with_animation` would drive — which is precisely why #71
/// rejected that pattern and why this is a timer rather than an animation.
const BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(530);

/// How long after the last keystroke the caret starts blinking again.
///
/// A caret that blinks *while* you type is worse than one that does not blink at all: the
/// motion competes with the character appearing under it. Every keystroke therefore holds
/// the caret solid and restarts this delay, so the blink only resumes once you have stopped.
const BLINK_RESUME_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// Rows of context kept above and below the cursor when the viewport follows it.
///
/// Zed's `vertical_scroll_margin`, whose default is `3`
/// (`zed/assets/settings/default.json:726`, read by
/// `crates/editor/src/editor_settings.rs:35`). Adopted as the number rather than derived:
/// it is a tuned default, and the point of reading it from Zed is not to re-derive it.
///
/// ponytail: a constant, not a setting. It joins the indent width (#60) whenever the editor
/// grows a settings surface for these — `crates/settings` already has the file layer.
const VERTICAL_SCROLL_MARGIN: usize = 3;

impl EditorView {
    pub fn new(document: Document, cx: &mut Context<Self>) -> Self {
        Self {
            document,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            visible_rows: 0..0,
            diagnostics: Vec::new(),
            link_hint: None,
            hover_diagnostic: None,
            text_origin_x: None,
            cursor_row_origin_y: None,
            caret_visible: true,
            blink: None,
            window_activation: None,
        }
    }

    /// Holds the caret solid and restarts the blink after a pause.
    ///
    /// Called from every path that moves the cursor or edits the buffer. Two things happen
    /// and both matter: the caret is forced *visible* (so a keystroke never lands during
    /// the dark half of the cycle, which reads as a dropped character), and the blink task
    /// is replaced — dropping the old one cancels it, so holding a key down keeps
    /// rescheduling the same single task rather than accumulating one per keystroke.
    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.caret_visible = true;
        self.blink = Some(cx.spawn(async move |this, cx| {
            // The pause before blinking resumes. A keystroke arriving inside this window
            // drops this task and starts a new one, so the caret stays solid for as long
            // as typing continues.
            cx.background_executor().timer(BLINK_RESUME_DELAY).await;
            loop {
                if this
                    .update(cx, |this, cx| {
                        this.caret_visible = !this.caret_visible;
                        // Notifies *this* entity, not the window. gpui still redraws the
                        // whole window (there is no partial repaint), but marking only the
                        // editor dirty is what lets any `.cached()` sibling view reuse its
                        // element tree instead of re-rendering.
                        cx.notify();
                    })
                    .is_err()
                {
                    // The editor is gone — the tab closed or the window did. Falling out
                    // of the loop lets the task finish and stops the wakeups.
                    return;
                }
                cx.background_executor().timer(BLINK_INTERVAL).await;
            }
        }));
    }

    /// Stops the blink and leaves the caret hidden.
    ///
    /// Dropping the `Task` cancels it, so this genuinely stops the timer rather than
    /// leaving it running against a flag — **the idle editor must cost nothing**, which is
    /// what the perf gate's 2% CPU limit is there to enforce (#93, #79).
    fn stop_blinking(&mut self) {
        self.blink = None;
        self.caret_visible = false;
    }

    /// Replaces the diagnostics painted over this document.
    ///
    /// Called by the workspace when the server publishes, and with an empty slice when the
    /// server goes away — see `Lsp::shut_down` for why clearing matters more than it looks.
    pub fn set_diagnostics(&mut self, diagnostics: Vec<(Range<usize>, Severity, SharedString)>) {
        self.diagnostics = diagnostics;
        // The card describes a diagnostic that may have just moved or vanished; a fresh
        // mouse move rebuilds it against the new list. Keeping it would show a message
        // about bytes that no longer carry it.
        self.hover_diagnostic = None;
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

    /// Whether a blink timer is currently running.
    ///
    /// Exposed for the render tests because this is the *perf* property, not a cosmetic
    /// one: an editor that keeps a timer alive while unfocused is a repaint on a timer on
    /// an idle window, which is the cost #93's gate exists to bound. A test can assert the
    /// task is gone; it cannot see CPU.
    #[cfg(test)]
    pub fn is_blinking_for_test(&self) -> bool {
        self.blink.is_some()
    }

    /// Whether the caret is in the visible half of its cycle.
    #[cfg(test)]
    pub fn caret_visible_for_test(&self) -> bool {
        self.caret_visible
    }

    /// Puts the caret in its hidden half, standing in for a timer tick.
    ///
    /// The blink is driven by a real timer on the background executor, so a test that
    /// wanted to observe a genuine tick would have to advance a clock and wait. What the
    /// test actually needs to pin is the *reaction* to an edit — that it forces the caret
    /// visible from wherever it was — and that is testable by putting it in the dark half
    /// directly.
    #[cfg(test)]
    pub fn set_caret_hidden_for_test(&mut self) {
        self.caret_visible = false;
    }

    /// Runs the post-edit path a keystroke takes.
    #[cfg(test)]
    pub fn after_edit_for_test(&mut self, cx: &mut Context<Self>) {
        self.after_edit(cx);
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

    /// Scrolls so the cursor row is on screen, keeping [`VERTICAL_SCROLL_MARGIN`] rows of
    /// context beyond it.
    ///
    /// This is Zed's `AutoscrollStrategy::Fit`
    /// (`crates/editor/src/scroll/autoscroll.rs:238-252`), transcribed into row terms:
    ///
    /// ```text
    /// let margin = margin.min(self.scroll_manager.vertical_scroll_margin);
    /// let target_top = (target_top - margin - ...).max(0.0);
    /// let target_bottom = target_bottom + margin;
    /// let needs_scroll_up = target_top < start_row;
    /// let needs_scroll_down = target_bottom >= end_row;
    /// if needs_scroll_up && !needs_scroll_down { scroll_position.y = target_top; }
    /// else if !needs_scroll_up && needs_scroll_down { scroll_position.y = target_bottom - visible_lines; }
    /// ```
    ///
    /// Three properties of that, all of which the previous one-liner got wrong:
    ///
    /// 1. **The margin.** Zed widens the cursor's target band by `vertical_scroll_margin`
    ///    rows on each side, so the viewport moves when the cursor comes *near* an edge, not
    ///    only once it has left. Arrowing down the file then keeps three lines of what comes
    ///    next in view instead of putting the caret on the bottom pixel row.
    /// 2. **Minimal movement, and direction-aware.** Scrolling up puts the target at the top;
    ///    scrolling down puts it at the bottom. The old call always used
    ///    [`ScrollStrategy::Top`], which meant a cursor leaving the bottom of a screenful
    ///    jumped the whole viewport so that row became row zero — a full-page lurch for one
    ///    line of movement.
    /// 3. **The `^` case does nothing.** When the row cannot be satisfied at both edges
    ///    (a viewport shorter than `2 * margin + 1` rows), Zed scrolls neither way rather
    ///    than oscillating. `needs_scroll_up ^ needs_scroll_down` is what says so, and it is
    ///    the reason this is written as two booleans rather than two `if`s.
    ///
    /// Deliberately *not* delegated to gpui's `scroll_to_item_with_offset`, which looks like
    /// it does this: its `offset` shrinks the viewport from the top only. In
    /// `gpui-0.2.2/src/elements/uniform_list.rs`, line 406 adds `offset_pixels` to the
    /// top comparison while line 409's bottom comparison does not, so the margin would apply
    /// above the cursor and not below it. Computing it here keeps it symmetric.
    ///
    /// Falls back to the old unconditional scroll before the first frame, when
    /// `visible_rows` is still empty and there is no viewport to reason about.
    fn scroll_cursor_into_view(&mut self) {
        let row = self.document.cursor_point().row;
        let last_row = self.document.buffer.len_lines().saturating_sub(1);

        if let Some((item, strategy)) = autoscroll_fit(row, &self.visible_rows, last_row) {
            self.scroll.scroll_to_item(item, strategy);
        }
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
        //
        // With multiple cursors (#82) the pair logic steps aside: auto-closing at five
        // sites needs per-cursor pair state that stage 1 does not carry, and a `(` that
        // closes at one cursor and not the others is worse than plain insertion at all.
        if self.document.has_multiple_cursors() {
            self.document.insert_at_all_cursors(text);
            self.restart_blink(cx);
            cx.emit(EditorEvent::Typed(text.to_string()));
            cx.notify();
            return;
        }
        let plain = !self.document.insert_with_pairs(text);
        if plain {
            self.document.insert(text);
        }
        self.scroll_cursor_into_view();
        // The case the blink exists to get right: a caret must not blink mid-keystroke.
        self.restart_blink(cx);
        cx.notify();

        // Reported *after* the buffer has it, so a listener asking for the cursor offset
        // reads the position the character now occupies rather than the one before it.
        //
        // Only for a plain insertion. The two `insert_with_pairs` branches are the same
        // exclusion `insert_typed` documents: typing over a closer inserts nothing and
        // auto-closing inserts two, so in neither case is "the user typed this character
        // here" a true description of what the buffer now contains — and a trigger fired on
        // an auto-inserted `"` would open a popup about a quote nobody typed.
        if plain {
            cx.emit(EditorEvent::Typed(text.to_string()));
        }
    }

    // --- action handlers ---------------------------------------------------------
    //
    // Each is a two-liner over Document plus notify. The pattern repeats because gpui
    // dispatches one handler per action type; the alternative is a single handler with a
    // match on a parameterised action, which trades this repetition for a less direct
    // keymap. Not worth it at this size.

    fn backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        self.document.backspace_at_all_cursors();
        self.after_edit(cx);
    }

    /// Escape with multiple cursors collapses to one; otherwise the key belongs to
    /// whoever else wants it (`propagate`), so find-dismissal and friends keep working.
    fn cancel_multi_cursor(
        &mut self,
        _: &crate::actions::Cancel,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.document.has_multiple_cursors() {
            self.document.clear_extra_selections();
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    /// ⌘D (#82): first press selects the word, each further press adds an occurrence.
    fn select_next_occurrence(
        &mut self,
        _: &crate::actions::SelectNextOccurrence,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document.select_next_occurrence();
        // The newest match is the primary, and it may be off-screen — a ⌘D that adds an
        // invisible cursor reads as a dead key.
        self.scroll_cursor_into_view();
        cx.notify();
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

    /// Inserts text the *popup* received, because the popup holds keyboard focus (#61).
    ///
    /// While a completion list is open the focus is on the popup — that is what makes its
    /// key context active and arrows navigate the list — so the character the user typed
    /// arrives there rather than here. It still has to end up in the buffer, and it has to
    /// go through the same path ordinary typing does: `insert_with_pairs` for auto-closing,
    /// then `after_edit` for the search rescan, the scroll and the blink. Splicing the
    /// buffer directly instead would give a completion-time keystroke different undo and
    /// bracket behaviour from the identical keystroke a second later with the popup closed.
    /// Returns whether the character landed in the buffer *as typed*.
    ///
    /// `false` means `insert_with_pairs` did something other than a plain insertion, and the
    /// caller must not assume the buffer grew by `text`. Two branches do that: typing over
    /// a closer moves the caret and inserts **nothing**, and auto-closing inserts **two**
    /// characters for one keystroke.
    ///
    /// The completion popup is why this is reported rather than swallowed. It mirrors each
    /// keystroke into its filter, and a filter that grew by `)` when the buffer did not
    /// leaves the replaced range and the query describing different spans — the same class
    /// of divergence as the dotted-route-name bug, whose rule was "both, or neither".
    pub fn insert_typed(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        let plain = !self.document.insert_with_pairs(text);
        if plain {
            self.document.insert(text);
        }
        self.after_edit(cx);
        plain
    }

    /// Deletes backwards for a backspace the popup received, for the same reason.
    pub fn backspace_typed(&mut self, cx: &mut Context<Self>) {
        self.document.backspace();
        self.after_edit(cx);
    }

    fn after_edit(&mut self, cx: &mut Context<Self>) {
        // Typing with the find bar open moves every match after the caret. A stale list
        // paints a highlight over bytes that have shifted, so the rescan happens here
        // rather than in `render` — a render pass should not mutate domain state, and the
        // version check inside makes this a comparison when nothing has changed.
        self.document.refresh_search();
        self.scroll_cursor_into_view();
        self.restart_blink(cx);
        cx.notify();
    }

    fn after_move(&mut self, cx: &mut Context<Self>) {
        self.scroll_cursor_into_view();
        // Motion holds the caret solid too, not just typing: holding ↓ to scroll through a
        // file with the caret strobing is the same distraction, and arriving somewhere with
        // the caret in its dark half means not being able to see where you landed.
        self.restart_blink(cx);
        cx.notify();
    }

    /// ⌘G / ⌘⇧G, driven by the find bar (#80).
    ///
    /// On the view rather than only on `Document` because jumping to a match off-screen
    /// has to scroll — the whole point of "next match" is arriving somewhere you can see.
    /// Returns whether a match was found.
    pub fn select_match(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let found = self.document.select_match(forward);
        if found {
            self.after_move(cx);
        }
        found
    }

    /// Maps a click inside a row to a text offset and moves the cursor there.
    /// The byte offset under a window x on `row` — the shared hit-test for click and hover.
    ///
    /// Extracted from `on_row_mouse_down` when hover needed the identical conversion; two
    /// copies of "window x to buffer offset" is how a click and a hover end up disagreeing
    /// about which character the mouse is on.
    fn offset_at(
        &self,
        window_x: Pixels,
        row: usize,
        window: &Window,
        cx: &Context<Self>,
    ) -> usize {
        let fonts = Fonts::get(cx);
        let line = self.document.buffer.line(row);
        let x = text_local_x(window_x, self.text_origin_x, &fonts);

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

        self.document.buffer.point_to_offset(Point::new(row, column))
    }

    /// Updates the hover card for a mouse position over `row`.
    ///
    /// Notifies only when the answer changes: this runs on every mouse-move event, and
    /// re-rendering the window per pixel of travel would put the whole frame on the hover
    /// path for nothing.
    fn on_row_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        row: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.offset_at(event.position.x, row, window, cx);
        let fonts = Fonts::get(cx);
        let position = gpui::point(event.position.x, event.position.y + fonts.line_height());
        self.hover_for_offset(offset, row, position, cx);

        // The ⌘-hover link hint. Recomputed on every move while ⌘ is held; notified only
        // on change, for the same per-pixel reason as the card above.
        let hint = if event.modifiers.platform { self.document.word_span_at(offset) } else { None };
        if hint != self.link_hint {
            self.link_hint = hint;
            cx.notify();
        }
    }

    /// The hover decision itself, split from the mouse event for the reason
    /// `should_open_on_trigger` was: the pixel-to-offset conversion cannot be exercised
    /// headlessly (the fake text system's advances are fiction — see `fonts.rs`), and a
    /// test that could only reach this through pixels would be a test of that fiction.
    /// The conversion is what the click tests already pin; this is the part with branches.
    fn hover_for_offset(
        &mut self,
        offset: usize,
        row: usize,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Inclusive of the end, matching `FileDiagnostics::at`: the cursor just past the
        // last character of a problem is still "on" it.
        let hit = self
            .diagnostics
            .iter()
            .filter(|(range, _, _)| range.start <= offset && offset <= range.end)
            .min_by_key(|(range, _, _)| range.end - range.start)
            .map(|(_, _, message)| HoverDiagnostic { message: message.clone(), position, row });

        // Position changes with every pixel; the card must not chase the mouse. Two states
        // are the same when they describe the same diagnostic on the same row.
        let same = match (&self.hover_diagnostic, &hit) {
            (Some(a), Some(b)) => a.message == b.message && a.row == b.row,
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.hover_diagnostic = hit;
            cx.notify();
        }
    }

    /// Clears the card when the mouse leaves `row`, unless a neighbour already owns it.
    fn on_row_hover_out(&mut self, row: usize, cx: &mut Context<Self>) {
        let mut changed = false;
        if self.hover_diagnostic.as_ref().is_some_and(|hover| hover.row == row) {
            self.hover_diagnostic = None;
            changed = true;
        }
        if self.link_hint.take().is_some() {
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// The hover decision, at the offset level the mouse handler delegates to.
    #[cfg(test)]
    pub fn hover_at_for_test(&mut self, offset: usize, row: usize, cx: &mut Context<Self>) {
        self.hover_for_offset(offset, row, gpui::point(px(0.0), px(0.0)), cx);
    }

    #[cfg(test)]
    pub fn hover_out_for_test(&mut self, row: usize, cx: &mut Context<Self>) {
        self.on_row_hover_out(row, cx);
    }

    fn on_row_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        row: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.offset_at(event.position.x, row, window, cx);

        // Click count, the way Zed branches on it in `begin_selection`
        // (`crates/editor/src/selection.rs:1277`): 1 places the cursor, 2 takes the
        // surrounding word, 3 takes the line, and anything beyond takes the document.
        // gpui counts the repeats and puts the count on the event, so this is a match rather
        // than a timer — see `MouseDownEvent::click_count` in gpui 0.2.2
        // (`src/interactive.rs:104`).
        //
        // Shift is checked first and only for a single click. Zed routes shift-click to
        // `extend_selection` (`selection.rs:1179`), which starts from the existing selection
        // rather than replacing it; shift-double-click there extends *by word*, which needs
        // the `SelectMode` this editor does not have (one `Selection`, no pending mode —
        // see the `ponytail` note on `Selection`). Rather than half-implement it,
        // shift-double-click here falls through to plain word selection, which is at least
        // not surprising.
        match event.click_count {
            1 => {
                // Shift-click extends the existing selection, matching every other editor.
                self.document.move_to(offset, event.modifiers.shift);
            }
            2 => self.document.select_word_at(offset),
            3 => self.document.select_line_at(row),
            _ => self.document.select_all(),
        }

        // ⌘click is go-to-definition, the way it is in every IDE. The cursor moves first
        // either way, so a ⌘click that finds nothing still behaves like the ordinary click
        // it also was — and the workspace, not the editor, owns the language server.
        if event.modifiers.platform {
            cx.emit(EditorEvent::GoToDefinition);
        }
        // Clicking is how you find the cursor when you have lost it; the caret must be
        // solid at the moment the click lands rather than possibly dark.
        self.restart_blink(cx);
        cx.notify();
    }

    /// Stops the blink when the window deactivates and restarts it on return.
    ///
    /// **A blinking caret in a window you are not looking at is pure cost** — it is the
    /// repaint-on-a-timer #79 spent three wrong conclusions chasing and #93's gate now
    /// bounds. Registered on the first render rather than in `new`, because
    /// `observe_window_activation` needs a `&mut Window`; `WorkspaceView::observe_window_focus`
    /// does the same thing for the same reason and documents the trade.
    ///
    /// Idempotent by the `is_some` guard, so this costs one branch per frame after the
    /// first.
    fn observe_window_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_activation.is_some() {
            return;
        }
        self.window_activation = Some(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                // Only if this editor still holds focus: returning to the window with the
                // palette open must not start a caret blinking behind it.
                if this.focus_handle.is_focused(window) {
                    this.restart_blink(cx);
                }
            } else {
                this.stop_blinking();
                cx.notify();
            }
        }));
    }
}

/// What the editor tells the workspace.
///
/// Both variants exist for the same reason: the editor knows something only it can know —
/// a coordinate, a keystroke — and the workspace owns what to *do* about it. An event
/// rather than a call keeps the editor unaware that a language server exists, which is the
/// same split `set_diagnostics` already has in the other direction.
pub enum EditorEvent {
    GoToDefinition,
    /// A character was typed straight into the buffer, and it is already there.
    ///
    /// The editor emits this without knowing why anyone would care. It is the workspace
    /// that holds the server's declared trigger characters and decides whether this one
    /// opens a completion popup (#61) — deciding here would mean the editor consulting
    /// capabilities, which is precisely the coupling `EditorEvent` exists to avoid.
    ///
    /// Not emitted while a popup already has focus: this fires from the *editor's* key
    /// handler, and with a popup open the keystroke goes to
    /// [`CompletionEvent::Typed`](crate::completion::CompletionEvent::Typed) instead.
    Typed(String),
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fonts = Fonts::get(cx);
        let row_count = self.document.buffer.len_lines();
        let cursor = self.document.cursor_point();

        self.observe_window_focus(window, cx);

        // The caret is drawn only when this editor holds keyboard focus *and* the window is
        // active. A caret in an unfocused editor claims input goes there when it does not —
        // which is #95's bug ("navigation leaves focus behind") made visible rather than
        // silent, and the reason #98 lists the two issues together.
        let focused = self.focus_handle.is_focused(window) && window.is_window_active();

        // Start the blink lazily, on the first frame this editor is focused, rather than in
        // `new`: a background tab must not run a timer, and `new` cannot see focus anyway.
        // The `blink.is_none()` guard makes this one branch per frame, not a restart.
        if focused && self.blink.is_none() {
            self.restart_blink(cx);
        } else if !focused && self.blink.is_some() {
            self.stop_blinking();
        }

        // Solid whenever unfocused, so the caret is simply absent rather than frozen
        // mid-cycle in whatever half the timer happened to stop in.
        let caret_visible = focused && self.caret_visible;
        let entity = cx.entity();

        div()
            .key_context(context::EDITOR)
            .track_focus(&self.focus_handle(cx))
            // The hover card is anchored at window coordinates captured when the mouse
            // stopped; scrolling moves the text out from under it, leaving a card pinned
            // over whatever scrolled in. Clearing on wheel is honest: the mouse is now
            // over different bytes, and the next pause re-asks.
            .on_scroll_wheel(cx.listener(|editor, _event: &gpui::ScrollWheelEvent, _window, cx| {
                let cleared =
                    editor.hover_diagnostic.take().is_some() | editor.link_hint.take().is_some();
                if cleared {
                    cx.notify();
                }
            }))
            // ⌘ lifting must take the underline and the hand with it, even with the mouse
            // still — a hint that outlives its modifier promises a jump a plain click will
            // not make.
            .on_modifiers_changed(cx.listener(
                |editor, event: &gpui::ModifiersChangedEvent, _window, cx| {
                    if !event.modifiers.platform && editor.link_hint.take().is_some() {
                        cx.notify();
                    }
                },
            ))
            // The pointing hand, whenever a word is hinted. Root-level is right, not
            // coarse: the hint follows the pointer, so over whitespace it is None and the
            // arrow returns.
            .when(self.link_hint.is_some(), |el| el.cursor_pointer())
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::select_next_occurrence))
            .on_action(cx.listener(Self::cancel_multi_cursor))
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
            // Must match the `.h()` each row is given below, and gpui's default does not:
            // it lays text out at roughly 1.618 em while the row is `fonts.line_height()`
            // (1.5 em by default). Left unset, every line's text is ~1.5px taller than the
            // box containing it, so the overflow accumulates down the file — by row 7 the
            // glyphs sit half a line above their own row. That is what made the caret
            // appear on one line while typing landed on the one above it, and what made
            // the indent guides paint as tall grey blocks spanning two rows.
            .line_height(fonts.line_height())
            .text_color(theme.text)
            .child(
                // uniform_list calls back only for visible rows, so a 50k-line file costs
                // the same per frame as a 50-line one.
                uniform_list("editor-rows", row_count, move |range, _window, cx| {
                    entity.update(cx, |editor, cx| {
                        editor.render_rows(range, cursor, caret_visible, cx)
                    })
                })
                .track_scroll(self.scroll.clone())
                .size_full(),
            )
    }
}

impl EditorView {
    /// Where the caret is on screen, in window coordinates — what a popup anchors to (#61).
    ///
    /// This is the *inverse* of `on_row_mouse_down`, and deliberately built from the same
    /// two pieces so the two directions cannot disagree: `text_origin_x` for the window x
    /// where text begins, and a shaped line for how far into it byte `column` sits.
    ///
    /// Measured rather than `column * cell_width` for the reasons `editor::caret` documents
    /// at length — a proportional fallback font and multibyte text both defeat arithmetic on
    /// a byte offset. `x_for_index` asks the shaped line where the byte actually is.
    ///
    /// Both coordinates are **window**-absolute, because both were measured from a
    /// laid-out element rather than assembled from constants. `None` before the first
    /// prepaint, and `None` when the cursor's row is scrolled out of view — there is no
    /// on-screen caret to anchor to, and a popup pinned to a cursor nobody can see belongs
    /// nowhere.
    pub fn cursor_position(&self, window: &Window, cx: &App) -> Option<gpui::Point<Pixels>> {
        let fonts = Fonts::get(cx);
        let cursor = self.document.cursor_point();

        let origin_y = self.cursor_row_origin_y?;
        let origin_x = self.text_origin_x?;

        let line = self.document.buffer.line(cursor.row);
        let column = floor_boundary(&line, cursor.column.min(line.len()));
        let measured = &line[..column.min(MAX_MEASURE_BYTES)];
        let measured = &measured[..floor_boundary(measured, measured.len())];

        // Shaping an empty prefix to be told the answer is zero is the common case on a
        // fresh line, and the shortcut is exact rather than an approximation — the same one
        // `editor::caret` takes.
        let x = if measured.is_empty() {
            px(0.0)
        } else {
            let runs = [TextRun {
                len: measured.len(),
                font: fonts.font(),
                color: gpui::white(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            window
                .text_system()
                .layout_line(measured, fonts.size, &runs, None)
                .x_for_index(measured.len())
        };

        Some(gpui::point(origin_x + x, origin_y))
    }

    /// Builds the elements for one band of visible rows.
    fn render_rows(
        &mut self,
        range: Range<usize>,
        cursor: Point,
        caret_visible: bool,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        // Capture the visible band for scroll-into-view, which runs outside render.
        self.visible_rows = range.clone();

        let theme = cx.theme().clone();
        let fonts = Fonts::get(cx);
        // Every cursor, primary included, for the rows to paint (#82). Head offsets plus
        // ranges, resolved once per frame rather than per row.
        let all_selections = self.document.all_selections();

        // Once per frame, not once per row: the lookup is a tree descent, and the answer is
        // the same for every visible row. Both offsets are document-wide; `line_runs` clips
        // them the same way it clips a syntax span.
        let brackets = self.document.matching_bracket();

        // Highlight once for the whole visible band rather than per row: one tree walk
        // instead of N, and the spans are already sorted so slicing per row is cheap.
        let band = self.visible_byte_range(&range);
        let spans = self.document.syntax.highlights(&self.document.buffer, band.clone());

        // Search matches, sliced to the band the same way, and for the same reason (#80).
        // `Matches::in_range` is two binary searches over a sorted list, so this costs
        // `O(log n + visible)` no matter how many matches the file has — the property
        // `match_lookup_cost_does_not_grow_with_file_size` pins down in `editor/find.rs`.
        // Collected into a small `Vec` once rather than re-sliced per row: the band holds
        // a screenful of hits, so the per-row filter below runs over tens of entries.
        let current_match = self.document.search.current_range();
        let band_matches: Vec<(Range<usize>, bool)> = self
            .document
            .search
            .matches()
            .in_range(band)
            .iter()
            .map(|range| (range.clone(), Some(range) == current_match.as_ref()))
            .collect();

        let entity = cx.entity();

        range
            .map(|row| {
                let line = self.document.buffer.line(row);
                let line_start = self.document.buffer.point_to_offset(Point::new(row, 0));
                let line_end = line_start + line.len();

                // Sliced per row rather than passed whole: `line_runs` would otherwise
                // scan every diagnostic in the file for each of the ~40 visible rows.
                // A file with hundreds of problems is exactly when that starts to matter.
                let row_diagnostics: Vec<(Range<usize>, Severity)> = self
                    .diagnostics
                    .iter()
                    .filter(|(range, _, _)| range.start < line_end && range.end > line_start)
                    // The message is hover's; painting needs only where and how loud.
                    .map(|(range, severity, _)| (range.clone(), *severity))
                    .collect();

                let row_matches: Vec<_> = band_matches
                    .iter()
                    .filter(|(range, _)| range.start < line_end && range.end > line_start)
                    .cloned()
                    .collect();

                let is_cursor_row = row == cursor.row;
                let row_selected = all_selections.iter().any(|sel| {
                    let range = sel.range();
                    !range.is_empty()
                        && range.start < line_end.max(line_start + 1)
                        && range.end > line_start
                });
                // Extra carets whose head sits on this row, as columns into the line.
                // The primary is excluded here — it keeps its existing path below, with
                // the blink and focus rules that path already carries.
                let extra_carets: Vec<usize> = self
                    .document
                    .extra_selection_heads()
                    .into_iter()
                    .filter(|head| *head >= line_start && *head <= line_end)
                    .map(|head| head - line_start)
                    .collect();
                let entity = entity.clone();

                let measuring_entity = entity.clone();
                let cursor_row = cursor.row;

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
                                // The row's own window y, kept for the completion popup
                                // (#61). Measured for the same reason the x is: the chrome
                                // above the editor is a tab bar plus a find bar whose height
                                // depends on whether it is showing a replace field, and
                                // adding those up from constants is the bug `text_origin_x`
                                // exists because someone already wrote. Only the cursor row
                                // is recorded — it is the only one anything anchors to.
                                if row == cursor_row {
                                    editor.cursor_row_origin_y = Some(text.origin.y);
                                }
                            });
                        }
                    })
                    .id(("row", row))
                    .on_mouse_down(MouseButton::Left, {
                        let entity = entity.clone();
                        move |event, window, cx| {
                            let event = event.clone();
                            entity.update(cx, |editor, cx| {
                                editor.on_row_mouse_down(&event, row, window, cx);
                            });
                        }
                    })
                    // The hover card (#59). Move decides what the mouse is on; hover-out
                    // clears a card the row owns, so leaving the editor does not strand one
                    // on screen. Row-in beats row-out between neighbours because the card
                    // is keyed by row — see `on_row_hover_out`.
                    .on_mouse_move({
                        let entity = entity.clone();
                        move |event, window, cx| {
                            let event = event.clone();
                            entity.update(cx, |editor, cx| {
                                editor.on_row_mouse_move(&event, row, window, cx);
                            });
                        }
                    })
                    .on_hover({
                        let entity = entity.clone();
                        move |entered, _window, cx| {
                            if !entered {
                                entity.update(cx, |editor, cx| editor.on_row_hover_out(row, cx));
                            }
                        }
                    })
                    .flex()
                    .h(fonts.line_height())
                    // Set here and not only on the root: `uniform_list` builds its rows in a
                    // callback, and `StyledText` resolves its line height from
                    // `window.text_style()` at layout time rather than from an ancestor div.
                    // A root-level `.line_height()` therefore never reaches these rows, which
                    // is why setting it there alone did not fix the overflow.
                    .line_height(fonts.line_height())
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
                    .child(
                        // `relative` so the caret's absolute position is resolved against
                        // the start of the text, not the window. The caret is a sibling of
                        // the text rather than something `styled_line` knows about: it is
                        // painted, not a colour run, and keeping it out of `line_runs`
                        // leaves that function's four documented overlays
                        // (matches → guides → bracket → cursor) intact minus the one that
                        // no longer exists.
                        div().flex_1().child({
                            let row_link = self
                                .link_hint
                                .clone()
                                .filter(|range| range.start < line_end && range.end > line_start);
                            let rendered = styled_line(
                                &line,
                                line_start,
                                &spans,
                                &row_diagnostics,
                                row_link,
                                brackets,
                                &row_matches,
                                &theme,
                                &fonts,
                            );
                            // The caret is drawn by the row itself, not layered over it.
                            // As a sibling it was placed by the flex pass and stacked
                            // *below* the text; inside the element it shares the shaped
                            // line the glyphs came from.
                            let rendered = extra_carets.iter().fold(rendered, |line, column| {
                                line.with_caret(*column, theme.cursor)
                            });
                            if is_cursor_row && caret_visible {
                                rendered.with_caret(cursor.column, theme.cursor)
                            } else {
                                rendered
                            }
                        }),
                    )
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

/// Decides how the viewport should follow `row`, or `None` to leave it alone.
///
/// Split out of [`EditorView::scroll_cursor_into_view`] as a pure function purely so it can
/// be tested: it is arithmetic over row indices, and unlike everything else about scrolling
/// it does not need a window, a font or a rendered frame to be wrong in a way that matters.
///
/// `visible` is the row range the last frame drew, `last_row` the final row of the buffer.
/// An empty `visible` means no frame has been drawn yet, so there is no viewport to reason
/// about and the caller falls back to putting the row at the top.
fn autoscroll_fit(
    row: usize,
    visible: &Range<usize>,
    last_row: usize,
) -> Option<(usize, ScrollStrategy)> {
    if visible.is_empty() {
        return Some((row, ScrollStrategy::Top));
    }

    // Zed's `margin.min(self.scroll_manager.vertical_scroll_margin)` (`autoscroll.rs:238`),
    // where its own `margin` is half the viewport: on a viewport too short to hold the
    // margin twice over, the margin shrinks rather than putting every row out of bounds.
    let margin = VERTICAL_SCROLL_MARGIN.min(visible.len() / 2);

    let target_top = row.saturating_sub(margin);
    // Zed's `target_bottom` is the row *after* the cursor (`target_top + 1.`) plus the
    // margin, and it compares with `>=` — so this is the first row that is allowed to be
    // past the end of the viewport.
    let target_bottom = row + 1 + margin;

    let needs_scroll_up = target_top < visible.start;
    let needs_scroll_down = target_bottom >= visible.end;

    // Zed's `needs_scroll_up ^ needs_scroll_down`: when both are true the row cannot be
    // satisfied at either edge, and scrolling to one would immediately violate the other.
    // Doing nothing is what stops that oscillating.
    if needs_scroll_up && !needs_scroll_down {
        Some((target_top, ScrollStrategy::Top))
    } else if needs_scroll_down && !needs_scroll_up {
        // `Bottom` places the given item at the bottom edge, so the item to name is the
        // last row that must be visible — the margin row below the cursor — clamped to the
        // file, since a margin past EOF is rows that do not exist. `target_bottom` is one
        // past it.
        Some(((target_bottom - 1).min(last_row), ScrollStrategy::Bottom))
    } else {
        None
    }
}

/// Renders one line with syntax colours, diagnostics, search hits and the matching bracket.
///
/// The cursor is **not** here any more. It used to be a background highlight on the
/// character under it — cheap, needing no measurement, and correct even on a proportional
/// fallback font, which is why it lasted — but a block sits *on* the next character rather
/// than between two, and at end-of-line it had nothing to paint on without appending a
/// padding space. It is now a painted quad positioned by measurement; see
/// [`crate::editor::caret::Caret`], which `render_rows` overlays as a sibling of this text.
#[allow(clippy::too_many_arguments)]
fn styled_line(
    line: &str,
    line_start: usize,
    spans: &[HighlightSpan],
    diagnostics: &[(Range<usize>, Severity)],
    link: Option<Range<usize>>,
    brackets: Option<(usize, usize)>,
    matches: &[(Range<usize>, bool)],
    theme: &Theme,
    fonts: &Fonts,
) -> Line {
    let (text, highlights) =
        line_runs(line, line_start, spans, diagnostics, link, brackets, matches, theme);
    // Guides come from the line's own indent, so they are computed here and painted by the
    // element — not folded into the runs above, which is what made them blocks (#108).
    let guides = indent_guide_columns(&text).into_iter().map(|range| range.start).collect();
    Line::new(
        text.clone(),
        to_runs(&text, &highlights, theme, fonts),
        fonts.size,
        fonts.line_height(),
    )
    .with_guides(guides, theme.indent_guide)
}

/// Turns the sparse `(range, style)` list into the contiguous runs `shape_line` needs.
///
/// `StyledText` accepts gaps and fills them from the ambient style; `shape_line` does not —
/// it walks runs in order and their lengths must sum to the text. So every uncovered stretch
/// becomes an explicit run in the default colour.
///
/// Ranges are assumed sorted and disjoint, which `line_runs` already guarantees (it asserts
/// it in tests). A range landing off a character boundary would panic in `shape_line`, so
/// they are clamped here rather than trusted.
fn to_runs(
    text: &str,
    highlights: &[(Range<usize>, GpuiHighlight)],
    theme: &Theme,
    fonts: &Fonts,
) -> Vec<TextRun> {
    let font = fonts.font();
    let plain = |len: usize| TextRun {
        len,
        font: font.clone(),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };

    let mut runs: Vec<TextRun> = Vec::with_capacity(highlights.len() * 2 + 1);
    let mut at = 0usize;
    for (range, style) in highlights {
        let start = range.start.clamp(at, text.len());
        let end = range.end.clamp(start, text.len());
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) || start == end {
            continue;
        }
        if start > at {
            runs.push(plain(start - at));
        }
        runs.push(TextRun {
            len: end - start,
            font: font.clone(),
            color: style.color.unwrap_or(theme.text),
            background_color: style.background_color,
            underline: style.underline,
            strikethrough: style.strikethrough,
        });
        at = end;
    }
    if at < text.len() {
        runs.push(plain(text.len() - at));
    }
    runs
}

/// The text and colour runs for one rendered line.
///
/// Split out from [`styled_line`] because `StyledText` is opaque once built — there is no
/// way to ask it what it will paint. Returning the runs first makes the part that can
/// actually be wrong (clipping, rebasing, cursor placement, char boundaries) assertable
/// without a GPU, which is the only slice of "does it render correctly" a machine can check
/// here. See `crates/app/tests/render.rs`.
///
/// `matches` is `(range, is_current)`: search hits touching this line, already sliced to
/// the viewport by the caller (#80).
#[allow(clippy::too_many_arguments)]
fn line_runs(
    line: &str,
    line_start: usize,
    spans: &[HighlightSpan],
    diagnostics: &[(Range<usize>, Severity)],
    link: Option<Range<usize>>,
    brackets: Option<(usize, usize)>,
    matches: &[(Range<usize>, bool)],
    theme: &Theme,
) -> (String, Vec<(Range<usize>, GpuiHighlight)>) {
    let line_end = line_start + line.len();

    // No padding space any more. The block cursor needed one so a cursor at end-of-line had
    // a character to paint a background on; a caret is positioned past the last glyph by
    // `x_for_index` returning the line width, so the line is now rendered as it is.
    let text = line.to_string();

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

    // The ⌘-hover link hint: a *straight* underline in the accent colour, where a
    // diagnostic's is wavy in a severity colour — same channel, visibly different claim.
    // "This is clickable" and "this is broken" must not be confusable at a glance, and
    // straight-vs-wavy is how every IDE draws that distinction.
    if let Some(range) = link
        && range.end > line_start
        && range.start < line_end
    {
        let start = floor_boundary(&text, range.start.max(line_start) - line_start);
        let end = ceil_boundary(&text, range.end.min(line_end) - line_start);
        if start < end {
            let underline = Some(gpui::UnderlineStyle {
                color: Some(theme.accent),
                thickness: px(1.0),
                wavy: false,
            });
            highlights = merge_underline(highlights, start..end, underline);
        }
    }

    // Search matches paint a *background* and leave the foreground alone, for the same
    // reason diagnostics only underline: a match over a keyword must still look like a
    // keyword. `merge_background` splits the runs underneath rather than replacing them,
    // so a match that covers half a token keeps that half's colour.
    //
    // First of the three overlays below, and the order is deliberate (#80 and #87 each
    // added one, and they interact):
    //
    //   matches → guides → bracket
    //
    // Matches go first because they *merge* rather than replace, so everything after can
    // still win its own bytes. The guides come next and are dropped where any run already
    // exists — which now includes a match, so a hit on an indented line hides the guide
    // under it rather than fighting it, and that is the right way round. The bracket wins
    // outright over what is beneath.
    //
    // The cursor used to be a fourth layer here and is now painted on top of the whole
    // line as a quad (#98), so it no longer competes for bytes with any of these — which
    // also means a caret inside a search hit or on a bracket is drawn over it rather than
    // erasing it, the bug shape #80-on-#87 documents two of below.
    for (range, is_current) in matches {
        if range.end <= line_start || range.start >= line_end {
            continue;
        }
        let start = floor_boundary(&text, range.start.max(line_start) - line_start);
        let end = ceil_boundary(&text, range.end.min(line_end) - line_start);
        if start >= end {
            continue;
        }
        let background =
            if *is_current { theme.current_search_match() } else { theme.search_match() };
        highlights = merge_background(highlights, start..end, background);
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
    // Indent guides are *not* here. They were a character background, which is a block one
    // character wide — the grey rectangles reported twice (#108). They vanished when rows
    // moved to a painted element (#110) because this path stopped being drawn at all, and
    // came back the moment `paint_background` was restored for the cursor (#111).
    //
    // They are now quads drawn by `Line`, one pixel wide, which is what the feature has
    // always meant and what Zed does (`element.rs:5126`, width clamped to 1..=10, default 1).
    let mut decorations: Vec<(Range<usize>, GpuiHighlight)> = Vec::new();

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
    // overlap would mean never drawing it at all.
    //
    // Two things here changed when #80 landed on top of #87, and both were reachable bugs
    // rather than tidying:
    //
    // 1. `bracket_match()` rather than `theme.selection` written inline. The *current*
    //    search match is also `theme.selection`, so a bracket inside the hit ⌘G is on
    //    would have been invisible — and searching for `function foo(` puts the cursor
    //    beside a bracket by definition.
    //
    // 2. `merge_over` first, then a `retain` bounded to the bracket's own byte. The plain
    //    `retain`-then-push the cursor uses drops **any** run straddling the bracket
    //    rather than splitting it. With one-token syntax spans underneath that costs a
    //    token's colour and nobody notices; with a search match spanning several
    //    characters it wipes the whole match's background off the line. Splitting first
    //    means the bracket takes its one byte and the match keeps the rest.
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
            highlights = merge_over(highlights, start..end, |style| style);
            highlights.retain(|(range, _)| range.end <= start || range.start >= end);
            highlights.push((
                start..end,
                GpuiHighlight {
                    background_color: Some(theme.bracket_match()),
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
    (!trimmed.is_empty() && trimmed.len() < line.len()).then_some(trimmed.len()..line.len())
}

/// Adds an underline over `span`, splitting any colour runs it partly covers.
fn merge_underline(
    runs: Vec<(Range<usize>, GpuiHighlight)>,
    span: Range<usize>,
    underline: Option<gpui::UnderlineStyle>,
) -> Vec<(Range<usize>, GpuiHighlight)> {
    merge_over(runs, span, |style| GpuiHighlight { underline, ..style })
}

/// Adds a background colour over `span`, splitting any colour runs it partly covers.
///
/// This is how a search match composes with syntax colours (#80): the match paints a
/// background and the token underneath keeps its foreground, so highlighting a match does
/// not turn a keyword the colour of a string.
fn merge_background(
    runs: Vec<(Range<usize>, GpuiHighlight)>,
    span: Range<usize>,
    background: gpui::Hsla,
) -> Vec<(Range<usize>, GpuiHighlight)> {
    merge_over(runs, span, move |style| GpuiHighlight {
        background_color: Some(background),
        ..style
    })
}

/// Applies `restyle` to the part of `runs` covered by `span`, splitting where needed.
///
/// gpui requires sorted, non-overlapping runs, so an extra attribute cannot simply be
/// pushed on top of the syntax colours — a run half-covered by a diagnostic or a search
/// match has to become two runs, one with the attribute and one without. That splitting is
/// the whole function, and it is why neither diagnostics nor matches could be expressed as
/// "one more span" alongside the highlight spans: those carry a colour each and never
/// overlap, where a diagnostic or a match overlaps by nature.
///
/// Bytes not covered by any existing run still need the attribute, so the gaps inside
/// `span` become runs of their own with no colour — they inherit the element's text colour,
/// which is what an uncoloured character already renders as.
fn merge_over(
    runs: Vec<(Range<usize>, GpuiHighlight)>,
    span: Range<usize>,
    restyle: impl Fn(GpuiHighlight) -> GpuiHighlight,
) -> Vec<(Range<usize>, GpuiHighlight)> {
    let mut merged: Vec<(Range<usize>, GpuiHighlight)> = Vec::with_capacity(runs.len() + 2);
    // Where inside `span` the next uncovered byte starts, so gaps between existing runs
    // get an attribute-only run rather than being skipped.
    let mut uncovered = span.start;

    for (range, style) in runs {
        if range.end <= span.start || range.start >= span.end {
            merged.push((range, style));
            continue;
        }

        // The part of this run before the span keeps its style unchanged.
        if range.start < span.start {
            merged.push((range.start..span.start, style));
        }

        // Any gap since the last run, inside the span, gets the attribute alone.
        if uncovered < range.start.max(span.start) {
            merged
                .push((uncovered..range.start.max(span.start), restyle(GpuiHighlight::default())));
        }

        // The overlap keeps the colour and gains the attribute.
        let overlap = range.start.max(span.start)..range.end.min(span.end);
        if overlap.start < overlap.end {
            merged.push((overlap.clone(), restyle(style)));
            uncovered = overlap.end;
        }

        // And the part after it is unchanged again.
        if range.end > span.end {
            merged.push((span.end..range.end, style));
        }
    }

    // Trailing bytes of the span that no run covered.
    if uncovered < span.end {
        merged.push((uncovered..span.end, restyle(GpuiHighlight::default())));
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

    // --- autoscroll margin -----------------------------------------------------------
    //
    // These test the *decision*, which is arithmetic over row indices and is the part that
    // can be wrong without a GPU. What they deliberately do NOT test is that the viewport
    // then moves: `scroll_to_item` defers to `uniform_list`'s prepaint, which needs a real
    // frame with a real item height, and the headless platform draws none. That half stays
    // on #35's human list.

    /// `visible` as a range, so a test reads as "rows 10 through 29 are on screen".
    fn viewport(start: usize, end: usize) -> Range<usize> {
        start..end
    }

    #[test]
    fn a_cursor_in_the_middle_of_the_viewport_does_not_scroll() {
        // The property that makes this safe to call after every keystroke: it must not
        // fight the user's own scrolling.
        assert_eq!(autoscroll_fit(20, &viewport(10, 30), 100), None);
    }

    #[test]
    fn a_cursor_within_the_margin_of_the_bottom_edge_scrolls_before_it_leaves() {
        // The whole point of the margin, and what the previous implementation lacked. Row
        // 28 is *visible* in 10..30 — the old `scroll_to_item(row, Top)` was a no-op here —
        // but it has only one row of context below it, so Zed scrolls.
        let (item, strategy) = autoscroll_fit(28, &viewport(10, 30), 100).expect("must scroll");
        assert!(matches!(strategy, ScrollStrategy::Bottom));
        // Three rows of context below the cursor: 28 + 3 = 31 is the last row to reveal.
        assert_eq!(item, 31);
    }

    #[test]
    fn a_cursor_within_the_margin_of_the_top_edge_scrolls_before_it_leaves() {
        let (item, strategy) = autoscroll_fit(11, &viewport(10, 30), 100).expect("must scroll");
        assert!(matches!(strategy, ScrollStrategy::Top));
        // Three rows of context above: 11 - 3 = 8.
        assert_eq!(item, 8);
    }

    #[test]
    fn a_cursor_just_outside_the_margin_does_not_scroll() {
        // The boundary in both directions, and it is asymmetric because Zed's two
        // comparisons are (`autoscroll.rs:244-245`): the top is `target_top < start_row`
        // and the bottom is `target_bottom >= end_row`, with `end_row` exclusive.
        //
        // Viewport 10..30, margin 3. Upwards: row 13 gives target_top 10, and `10 < 10` is
        // false, so it stays. Downwards: row 25 gives target_bottom 29, and `29 >= 30` is
        // false, so it stays — one row tighter than the top, because `target_bottom` is
        // already one *past* the last row that must be shown.
        assert_eq!(autoscroll_fit(13, &viewport(10, 30), 100), None, "top boundary");
        assert_eq!(autoscroll_fit(25, &viewport(10, 30), 100), None, "bottom boundary");
        // And one row further out in each direction does scroll.
        assert!(autoscroll_fit(12, &viewport(10, 30), 100).is_some(), "one past the top");
        assert!(autoscroll_fit(26, &viewport(10, 30), 100).is_some(), "one past the bottom");
    }

    #[test]
    fn scrolling_down_puts_the_row_at_the_bottom_rather_than_the_top() {
        // The second thing the old one-liner got wrong. `ScrollStrategy::Top` on a cursor
        // leaving the bottom of a screenful made that row row zero — a full-page lurch for
        // one line of downward movement. Minimal movement means the *bottom* edge.
        let (_, strategy) = autoscroll_fit(60, &viewport(10, 30), 100).expect("must scroll");
        assert!(
            matches!(strategy, ScrollStrategy::Bottom),
            "a jump downwards must land at the bottom edge, not scroll the row to the top"
        );
    }

    #[test]
    fn the_bottom_margin_never_names_a_row_past_the_end_of_the_file() {
        // A cursor on the last line has no three rows below it to reveal. Naming row 102 of
        // a 101-row file is a row `uniform_list` has no item for.
        let (item, _) = autoscroll_fit(100, &viewport(10, 30), 100).expect("must scroll");
        assert_eq!(item, 100);
    }

    #[test]
    fn a_viewport_too_short_for_the_margin_still_scrolls() {
        // Zed's `margin.min(vertical_scroll_margin)`. A 4-row viewport cannot hold three
        // rows of context on both sides; the margin shrinks to 2 rather than making every
        // row simultaneously too high and too low, which would scroll nowhere forever.
        let result = autoscroll_fit(50, &viewport(10, 14), 100);
        assert!(result.is_some(), "a cursor far below a short viewport must still scroll");
    }

    #[test]
    fn a_row_that_cannot_satisfy_both_edges_scrolls_neither_way() {
        // Zed's `needs_scroll_up ^ needs_scroll_down` (`autoscroll.rs:253`): when the row
        // plus its margins does not fit in the viewport at all, scrolling to satisfy either
        // edge immediately violates the other, so it scrolls neither way.
        //
        // Viewport 10..14 is 4 rows, so the margin shrinks to 2 — but 2 + 1 + 2 = 5 rows
        // still do not fit in 4. Row 11: target_top 9 is above 10, *and* target_bottom 14
        // is at 14. Both true.
        assert_eq!(autoscroll_fit(11, &viewport(10, 14), 100), None);
    }

    #[test]
    fn before_the_first_frame_the_row_goes_to_the_top() {
        // `visible_rows` is `0..0` until `render_rows` has run once, and a navigation can
        // arrive before that — opening a file at a line number does exactly this.
        assert_eq!(autoscroll_fit(42, &viewport(0, 0), 100), Some((42, ScrollStrategy::Top)));
    }

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

    /// `line_runs` with no diagnostics and no search matches, which is what every
    /// pre-existing test means.
    ///
    /// A wrapper rather than `&[]` threaded through nine call sites: those tests are about
    /// syntax colours, and empty diagnostics and match arguments in each would be noise in
    /// the one place their arguments should read as the thing under test.
    fn line_runs(
        line: &str,
        line_start: usize,
        spans: &[HighlightSpan],
        theme: &Theme,
    ) -> (String, Vec<(Range<usize>, GpuiHighlight)>) {
        super::line_runs(line, line_start, spans, &[], None, None, &[], theme)
    }

    /// `line_runs` with search matches, spelled out.
    fn line_runs_matching(
        line: &str,
        line_start: usize,
        spans: &[HighlightSpan],
        matches: &[(Range<usize>, bool)],
        theme: &Theme,
    ) -> (String, Vec<(Range<usize>, GpuiHighlight)>) {
        super::line_runs(line, line_start, spans, &[], None, None, matches, theme)
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
        let (text, runs) =
            line_runs("    return $this;", 100, &[span(104..110, HighlightStyle::Keyword)], &theme);

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
        );
        assert!(coloured(&runs).is_empty());
    }

    #[test]
    fn a_span_straddling_the_line_is_clipped_to_it() {
        let theme = Theme::dark();
        // A block comment opening on an earlier line and closing on a later one: the
        // visible part must still colour, clipped at both ends rather than overflowing.
        let line = "still inside";
        let (text, runs) = line_runs(line, 100, &[span(50..200, HighlightStyle::Comment)], &theme);

        assert_eq!(coloured(&runs), vec![0..line.len()]);
        assert!(runs.iter().all(|(r, _)| r.end <= text.len()), "no run may exceed the text");
    }

    #[test]
    fn runs_never_split_a_multibyte_character() {
        let theme = Theme::dark();
        // "ção" — the span deliberately ends mid-codepoint, which StyledText
        // debug-asserts against. It must snap to a boundary instead of panicking.
        let line = "$mensagem = 'ação';";
        let (text, runs) = line_runs(line, 0, &[span(13..17, HighlightStyle::String)], &theme);

        for (range, _) in &runs {
            assert!(
                text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
                "run {range:?} splits a codepoint in {text:?}"
            );
        }
    }

    // The three tests that used to sit here — the cursor winning the overlap with a syntax
    // colour, a cursor at end-of-line getting a padding cell, and a cursor landing on a
    // whole multibyte character — all asserted properties of the *block cursor*, which
    // #98 deleted. They are not ported, because each one now tests something that cannot
    // fail by construction rather than something that could:
    //
    //   - "wins the overlap" — the caret is a quad painted after the text, not a colour
    //     run competing for bytes. There is no overlap to lose.
    //   - "padded at end of line" — the padding space is gone; `x_for_index` returns the
    //     line width for an index past the last glyph.
    //   - "lands on a whole multibyte character" — this one still matters and moved to
    //     where the arithmetic now lives, as `an_offset_inside_a_multibyte_character_snaps_down`
    //     in `editor::caret`.
    //
    // What is *not* covered anywhere, and is stated rather than quietly dropped: whether
    // the caret is at the right pixel. gpui's headless text system is a fake monospace
    // (see `crate::render_tests`), so no test here can tell a correct `x_for_index` from an
    // inverted one. That stays on #35's human list.

    #[test]
    fn no_run_carries_the_cursor_colour_any_more() {
        let theme = Theme::dark();
        // The block cursor was a background run in `theme.cursor`. If one ever reappears
        // here, two things are drawing the cursor and they will disagree.
        let (_, runs) = line_runs("return $x;", 0, &[span(0..6, HighlightStyle::Keyword)], &theme);

        assert!(
            runs.iter().all(|(_, style)| style.background_color != Some(theme.cursor)),
            "the caret is painted as a quad, not as a colour run"
        );
    }

    #[test]
    fn a_line_is_rendered_without_a_padding_space() {
        let theme = Theme::dark();
        // The block cursor appended a space so it had a cell to paint at end-of-line. That
        // space was real text as far as everything downstream was concerned — it shifted
        // the trailing-whitespace tint and widened the line. The caret needs no such cell.
        let (text, _) = line_runs("ab", 0, &[], &theme);
        assert_eq!(text, "ab", "no padding cell");
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
        let (text, runs) = line_runs("", 0, &[], &theme);
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
        //
        // Asserted against `indent_guide_columns` rather than through the runs a line
        // produces: guides are painted as quads by `Line` now, not folded into the colour
        // runs (#108), so a run-based assertion would test a path they no longer take.
        let _ = &theme;
        let columns: Vec<_> = indent_guide_columns("        return $x;");
        assert_eq!(columns, vec![0..1, 4..5]);
    }

    #[test]
    fn an_unindented_line_gets_no_guides() {
        let theme = Theme::dark();
        let (_, runs) = line_runs("class User {", 0, &[], &theme);
        assert!(backgrounds(&runs, theme.indent_guide).is_empty());
    }

    #[test]
    fn a_blank_line_gets_no_guides_and_no_trailing_tint() {
        // A whitespace-only line has no code to align to, and tinting every blank line
        // between two methods turns the file into a barcode.
        let theme = Theme::dark();
        for line in ["", "    ", "\t\t"] {
            let (_, runs) = line_runs(line, 0, &[], &theme);
            assert!(backgrounds(&runs, theme.indent_guide).is_empty(), "{line:?}");
            assert!(backgrounds(&runs, theme.trailing_whitespace).is_empty(), "{line:?}");
        }
    }

    #[test]
    fn trailing_whitespace_is_tinted_and_nothing_else_is() {
        let theme = Theme::dark();
        let line = "return $x;   ";
        let (_, runs) = line_runs(line, 0, &[], &theme);
        assert_eq!(backgrounds(&runs, theme.trailing_whitespace), vec![10..13]);

        // A line with none must get none, or the tint means nothing.
        let (_, runs) = line_runs("return $x;", 0, &[], &theme);
        assert!(backgrounds(&runs, theme.trailing_whitespace).is_empty());
    }

    // `the_space_padded_in_for_a_cursor_at_end_of_line_is_not_trailing_whitespace` used to
    // sit here. It guarded a real bug — the block cursor's padding space being tinted as
    // trailing whitespace, marking every line the cursor rested on — but the padding space
    // no longer exists, so the test can only assert that a thing that cannot happen does
    // not. `a_line_is_rendered_without_a_padding_space` above pins the replacement
    // invariant: the text is the line, unmodified.

    #[test]
    fn a_guide_never_splits_a_multibyte_character() {
        // Guides are computed from leading ASCII whitespace only, so a line whose *code*
        // is accented still produces boundary-safe ranges. Asserting it because a guide
        // range landing mid-codepoint would make StyledText debug-assert.
        let line = "    $m = 'ação';";
        let columns = indent_guide_columns(line);

        assert_eq!(columns, vec![0..1]);
        // The real hazard, unchanged by the move to quads: `Line::paint` calls
        // `x_for_index`, which panics on an index inside a codepoint just as StyledText
        // debug-asserted before it.
        for range in &columns {
            assert!(
                line.is_char_boundary(range.start) && line.is_char_boundary(range.end),
                "guide {range:?} splits a codepoint in {line:?}"
            );
        }
    }

    #[test]
    fn decorations_never_overlap_a_syntax_run() {
        // gpui needs sorted, disjoint runs. A multi-line block comment covers a line's own
        // indent, and a guide pushed on top of it would be a second run over those bytes.
        let theme = Theme::dark();
        let line = "        still in the comment   ";
        let (_, runs) = line_runs(line, 0, &[span(0..line.len(), HighlightStyle::Comment)], &theme);

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
        assert_eq!(backgrounds(&runs, theme.bracket_match()), vec![1..2, 3..4]);
    }

    #[test]
    fn a_partner_on_another_line_paints_only_the_end_that_is_here() {
        // The common case: a `{` at the top of a method and its `}` forty lines down. Each
        // row must paint its own end and clip the other, exactly as a syntax span does.
        let theme = Theme::dark();
        let (_, runs) = line_runs_with_brackets("}", 500, Some((12, 500)), &theme);
        assert_eq!(backgrounds(&runs, theme.bracket_match()), vec![0..1]);
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
            None,
            Some((1, 3)),
            &[],
            &theme,
        );

        assert_eq!(backgrounds(&runs, theme.bracket_match()), vec![1..2, 3..4]);
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
        super::line_runs(line, line_start, &[], &[], None, brackets, &[], theme)
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
            line_runs_with("$undefined = 1;", 0, &[], &[(0..10, Severity::Error)], &theme);

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
                let (_, runs) = line_runs_with("abcd", 0, &[], &[(0..4, severity)], &theme);
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
        let (_, runs) = line_runs_with("plain words", 0, &[], &[(6..11, Severity::Hint)], &theme);

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
        let (_, runs) = line_runs_with("this line", 100, &[], &[(0..50, Severity::Error)], &theme);

        assert!(underlined(&runs).is_empty());
    }

    #[test]
    fn a_multiline_diagnostic_is_clipped_to_the_visible_line() {
        let theme = Theme::dark();
        let line = "middle";
        let (_, runs) = line_runs_with(line, 100, &[], &[(50..200, Severity::Error)], &theme);

        assert_eq!(underlined(&runs), vec![(0..line.len(), Some(theme.error))]);
    }

    #[test]
    fn a_zero_width_diagnostic_still_shows_something() {
        // A server pointing *between* two characters — "expected ; here". An empty range
        // would produce no run at all, so the user is told nothing.
        let theme = Theme::dark();
        let (_, runs) = line_runs_with("$x = 1", 0, &[], &[(3..3, Severity::Error)], &theme);

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
        let (text, runs) = line_runs_with(line, 0, &[], &[(13..17, Severity::Error)], &theme);

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
        let (text, with) = line_runs_with("return $x;", 0, &spans, &[], &theme);
        let (plain_text, without) = line_runs("return $x;", 0, &spans, &theme);

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
    ) -> (String, Vec<(Range<usize>, GpuiHighlight)>) {
        super::line_runs(line, line_start, spans, diagnostics, None, None, &[], theme)
    }

    // --- search match highlighting (#80) --------------------------------------------
    //
    // The property that matters: a match paints a *background* and leaves the syntax
    // foreground alone. Getting that backwards would make every hit on a keyword look
    // like a string, which is the exact failure the diagnostics underline was designed
    // around and would be a regression of the same lesson.

    /// Byte ranges carrying `background`, which is how a match is expressed.
    fn backgrounded(
        runs: &[(Range<usize>, GpuiHighlight)],
        background: gpui::Hsla,
    ) -> Vec<Range<usize>> {
        runs.iter()
            .filter(|(_, style)| style.background_color == Some(background))
            .map(|(range, _)| range.clone())
            .collect()
    }

    #[test]
    fn a_match_paints_a_background_without_touching_the_syntax_colour() {
        let theme = Theme::dark();
        let line = "return $user;";
        let (_, runs) = line_runs_matching(
            line,
            0,
            &[span(0..6, HighlightStyle::Keyword)],
            &[(7..12, false)],
            &theme,
        );

        assert_eq!(backgrounded(&runs, theme.search_match()), vec![7..12]);
        // The keyword still has its own colour, and no background.
        let keyword = runs.iter().find(|(r, _)| *r == (0..6)).expect("keyword run survived");
        assert_eq!(keyword.1.color, Some(theme.syntax(HighlightStyle::Keyword)));
        assert_eq!(keyword.1.background_color, None);
    }

    #[test]
    fn a_match_over_a_token_keeps_the_tokens_colour() {
        // The composition the issue calls for: the match background and the syntax
        // foreground on the *same* run, not one replacing the other.
        let theme = Theme::dark();
        let (_, runs) = line_runs_matching(
            "return x;",
            0,
            &[span(0..6, HighlightStyle::Keyword)],
            &[(0..6, false)],
            &theme,
        );

        let run = runs.iter().find(|(r, _)| *r == (0..6)).expect("the covered run exists");
        assert_eq!(run.1.color, Some(theme.syntax(HighlightStyle::Keyword)), "colour survived");
        assert_eq!(run.1.background_color, Some(theme.search_match()), "and gained a background");
    }

    #[test]
    fn a_match_covering_half_a_token_splits_it() {
        // A run half-covered has to become two, or gpui gets overlapping runs.
        let theme = Theme::dark();
        let (_, runs) = line_runs_matching(
            "returned",
            0,
            &[span(0..8, HighlightStyle::Keyword)],
            &[(0..6, false)],
            &theme,
        );

        assert_eq!(backgrounded(&runs, theme.search_match()), vec![0..6]);
        let tail = runs.iter().find(|(r, _)| *r == (6..8)).expect("the uncovered half exists");
        assert_eq!(tail.1.color, Some(theme.syntax(HighlightStyle::Keyword)));
        assert_eq!(tail.1.background_color, None);
        assert_sorted_and_disjoint(&runs);
    }

    #[test]
    fn the_current_match_is_a_different_colour_from_the_others() {
        // Otherwise ⌘G moves an invisible cursor through identical-looking hits.
        let theme = Theme::dark();
        let (_, runs) = line_runs_matching(
            "a a a",
            0,
            &[],
            &[(0..1, false), (2..3, true), (4..5, false)],
            &theme,
        );

        assert_eq!(backgrounded(&runs, theme.search_match()), vec![0..1, 4..5]);
        assert_eq!(backgrounded(&runs, theme.current_search_match()), vec![2..3]);
        assert_ne!(theme.search_match(), theme.current_search_match());
    }

    #[test]
    fn a_match_is_clipped_and_rebased_like_a_span() {
        // A match straddling the line — the common case for a regex — must paint only the
        // visible part, in line-local coordinates.
        let theme = Theme::dark();
        let line = "middle";
        let (_, runs) = line_runs_matching(line, 100, &[], &[(95..103, false)], &theme);
        assert_eq!(backgrounded(&runs, theme.search_match()), vec![0..3]);

        // And one entirely outside is dropped rather than painting at a clamped offset.
        let (_, runs) = line_runs_matching(line, 100, &[], &[(0..50, false)], &theme);
        assert!(backgrounded(&runs, theme.search_match()).is_empty());
    }

    #[test]
    fn a_multibyte_match_never_splits_a_character() {
        // `ação` is bytes 8..14 (ç and ã are two bytes each). `Matches` only ever produces
        // boundaries, but a *clipped* match can still land mid-codepoint the same way a
        // clipped syntax span does, and `StyledText` debug-asserts on that. Feeding a
        // deliberately mid-codepoint 12 here checks the snap rather than trusting it.
        let theme = Theme::dark();
        let line = "$msg = 'ação';";
        assert!(!line.is_char_boundary(12), "12 must be inside ã for this test to mean anything");

        let (text, runs) = line_runs_matching(line, 0, &[], &[(8..12, false)], &theme);
        for (range, _) in &runs {
            assert!(text.is_char_boundary(range.start), "{range:?} starts mid-codepoint");
            assert!(text.is_char_boundary(range.end), "{range:?} ends mid-codepoint");
        }
        // Snapped outward to the end of the character it landed inside, never inward.
        assert_eq!(backgrounded(&runs, theme.search_match()), vec![8..13]);
        assert_eq!(&text[8..13], "açã");

        // And an exact, unclipped match paints exactly itself.
        let (_, runs) = line_runs_matching(line, 0, &[], &[(8..14, false)], &theme);
        assert_eq!(backgrounded(&runs, theme.search_match()), vec![8..14]);
    }

    #[test]
    fn a_match_under_the_caret_is_painted_whole() {
        // This replaces `the_cursor_still_wins_over_a_match_underneath_it`, and the reason
        // it is not simply deleted is that the *original* concern was real: the current
        // match is the selection, so the cursor always sits inside it, and the block cursor
        // and the match background were two runs fighting for the same bytes. #87-on-#80
        // documents the resulting bug — a `retain` that dropped any run straddling the
        // cursor wiped the match's background off the line.
        //
        // The caret cannot lose that fight because it is no longer in it: it is a quad
        // painted over the finished line. So the property worth pinning flipped. It is no
        // longer "the cursor survives the match" but "the match is now painted whole,
        // because nothing punches a hole in it any more".
        let theme = Theme::dark();
        let (_, runs) = line_runs_matching("needle", 0, &[], &[(0..6, true)], &theme);

        assert_eq!(
            backgrounded(&runs, theme.current_search_match()),
            vec![0..6],
            "an unbroken match; the block cursor used to split this into 0..2 and 3..6"
        );
        assert_sorted_and_disjoint(&runs);
    }

    #[test]
    fn a_match_over_a_diagnostic_keeps_the_squiggle() {
        // Two overlays on the same bytes. Losing the underline here would mean a search
        // hides errors, which is a worse outcome than either feature alone.
        let theme = Theme::dark();
        let (_, runs) = super::line_runs(
            "bad",
            0,
            &[],
            &[(0..3, Severity::Error)],
            None,
            None,
            &[(0..3, false)],
            &theme,
        );
        let run = runs.iter().find(|(r, _)| *r == (0..3)).expect("the covered run exists");
        assert!(run.1.underline.is_some(), "the diagnostic underline survived the match");
        assert_eq!(run.1.background_color, Some(theme.search_match()));
    }

    #[test]
    fn the_link_hint_underlines_straight_where_diagnostics_are_wavy() {
        // "Clickable" and "broken" share the underline channel and must not be confusable:
        // straight accent for the ⌘-hover hint, wavy severity colour for a squiggle. If
        // either assertion here starts failing the two have collapsed into one claim.
        let theme = Theme::dark();

        let (_, runs) = super::line_runs("abcdef", 0, &[], &[], Some(1..4), None, &[], &theme);
        let link = runs.iter().find(|(r, _)| *r == (1..4)).expect("the hinted run exists");
        let underline = link.1.underline.expect("the hint underlines");
        assert!(!underline.wavy, "a link hint is straight — wavy claims breakage");
        assert_eq!(underline.color, Some(theme.accent));

        let (_, runs) =
            super::line_runs("abcdef", 0, &[], &[(1..4, Severity::Error)], None, None, &[], &theme);
        let diag = runs.iter().find(|(r, _)| *r == (1..4)).expect("the squiggled run exists");
        assert!(diag.1.underline.expect("diagnostics underline").wavy);
    }

    #[test]
    fn no_matches_produces_exactly_what_it_did_before() {
        // The regression guard, mirroring `no_diagnostics_produces_exactly_what_it_did_before`.
        // Every editor without an open find bar is permanently in this state, so adding
        // search must not change a single run there.
        let theme = Theme::dark();
        let spans = [span(0..6, HighlightStyle::Keyword)];
        let (text, with) = line_runs_matching("return $x;", 0, &spans, &[], &theme);
        let (plain_text, without) = line_runs("return $x;", 0, &spans, &theme);

        assert_eq!(text, plain_text);
        assert_eq!(with.len(), without.len());
        for (a, b) in with.iter().zip(&without) {
            assert_eq!(a.0, b.0);
            assert_eq!(a.1.color, b.1.color);
            assert_eq!(a.1.background_color, b.1.background_color);
        }
    }

    /// gpui requires sorted, non-overlapping runs; violating that is a paint-time panic
    /// rather than a wrong colour, so it is worth asserting directly.
    fn assert_sorted_and_disjoint(runs: &[(Range<usize>, GpuiHighlight)]) {
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
    fn a_bracket_inside_the_current_match_is_still_visible() {
        // The collision the rebase surfaced. #87 painted the bracket pair as
        // `theme.selection`, and #80's *current* match is also `theme.selection`; #87 also
        // used the cursor's plain `retain`-then-push, which drops any run straddling the
        // bracket instead of splitting it. Together those two meant a bracket inside the
        // hit ⌘G is on vanished *and* took the match's whole background with it. Searching
        // for `function foo(` puts the cursor beside a bracket by definition, so this was
        // reachable rather than theoretical.
        let theme = Theme::dark();
        assert_ne!(
            theme.bracket_match(),
            theme.current_search_match(),
            "a bracket inside the current match would be invisible"
        );

        let (_, runs) =
            super::line_runs("f(1)", 0, &[], &[], None, Some((1, 3)), &[(0..4, true)], &theme);

        assert_eq!(
            backgrounds(&runs, theme.bracket_match()),
            vec![1..2, 3..4],
            "the brackets win their own bytes back from the match"
        );
        assert_eq!(
            backgrounds(&runs, theme.current_search_match()),
            vec![0..1, 2..3],
            "and the match keeps every byte the brackets did not take"
        );
        assert_sorted_and_disjoint(&runs);
    }
}
