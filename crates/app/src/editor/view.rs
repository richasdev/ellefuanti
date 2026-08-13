//! The gpui view that renders a [`Document`] and turns input into edits.
//!
//! Deliberately thin: every editing *semantic* lives in `Document` (plain Rust, unit
//! tested). This file translates input into `Document` calls and `Document` state into
//! elements, and owns nothing else.

use std::ops::Range;

use elle_syntax::HighlightSpan;
use elle_text::Point;
use gpui::{
    App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
    HighlightStyle as GpuiHighlight, KeyDownEvent, MouseButton, MouseDownEvent, Pixels,
    ScrollStrategy, SharedString, TextRun, UniformListScrollHandle, Window, div, prelude::*, px,
    svg, uniform_list,
};

use crate::actions::{
    Backspace, Copy, Cut, Delete, DeleteLine, DeleteToLineEnd, DeleteToLineStart, DeleteWordLeft,
    DeleteWordRight, DuplicateLineDown, DuplicateLineUp, FoldBlock, Indent, MoveDocumentEnd,
    MoveDocumentStart, MoveDown, MoveLeft, MoveLineDown, MoveLineEnd, MoveLineStart, MoveLineUp,
    MoveRight, MoveUp, MoveWordLeft, MoveWordRight, Newline, OpenLineAbove, OpenLineBelow, Outdent,
    Paste, Redo, SelectAll, SelectDocumentEnd, SelectDocumentStart, SelectDown, SelectLeft,
    SelectLineEnd, SelectLineStart, SelectRight, SelectUp, SelectWordLeft, SelectWordRight, Tab,
    ToggleComment, Undo, UnfoldBlock, context,
};
use crate::editor::ghost::{self, GhostSuggestion};
use crate::editor::ime;
use crate::editor::inlay::{HintKind, ResolvedHint, hints_on_line};
use crate::editor::input_element::InputHandlerElement;
use crate::editor::line::Line;
use crate::editor::state::{Document, Selection};
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

/// The anchor of an ⌥-drag, held from mouse-down until release.
struct AltDrag {
    anchor_x: Pixels,
    anchor_row: usize,
    /// The mouse-down offset, applied as a plain ⌥click caret if no drag happens.
    click_offset: usize,
    moved: bool,
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
    /// Rows carrying a breakpoint, pushed in by the workspace (#30).
    ///
    /// Rows rather than byte offsets, because a breakpoint is a *line* — that is the unit
    /// DBGp addresses, and an offset would need remapping on every edit to answer a
    /// question the protocol only ever asks in line numbers.
    ///
    /// Pushed for the same reason diagnostics are: the workspace owns the debug session and
    /// the breakpoint store, and an editor reaching into either would make "the session
    /// died" something every open tab has to cope with.
    breakpoints: Vec<usize>,
    /// The row execution is stopped on, if it is in *this* file.
    ///
    /// `Option` rather than a flag on the row, because only one row in one file can be the
    /// current statement, and letting each editor decide would eventually show two arrows.
    debug_row: Option<usize>,
    /// True between a left mouse-down on a row and its release: drag-selection (#82).
    dragging: bool,
    /// An ⌥-drag in progress: the anchor's window-x and row, plus the offset to fall
    /// back to as a plain ⌥click if the mouse never moves before release.
    ///
    /// The pixel x, not a column: each row converts it through its own `offset_at`, so
    /// the column stays visually straight even where tabs make byte columns lie.
    alt_drag: Option<AltDrag>,
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
    /// Which lines are folded away (#82). View state, not document state: two views of
    /// one document (a future split) fold independently, and no fold survives in a file
    /// on disk.
    folds: crate::editor::folds::Folds,
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
    /// The AI ghost suggestion, if one is showing (#29).
    ///
    /// Stamped with the buffer version and cursor offset it was made for, and only ever
    /// consulted through its validity check — see [`GhostSuggestion::is_valid_for`]. The
    /// stamp is what makes stale state harmless: paths that move the cursor without
    /// passing through a dismissal leave a ghost that simply never renders or accepts.
    ghost: Option<GhostSuggestion>,
    /// The 400ms pause-then-request timer (#29). Dropping it cancels it — the blink's
    /// contract — so every keystroke replaces the task rather than stacking timers, and
    /// no timer exists at all while the feature is off or the editor sits unedited (#93).
    ghost_debounce: Option<gpui::Task<()>>,
    /// The completion request in flight, at most one (#93). The task owns the `curl`
    /// child via `kill_on_drop`, so replacing or clearing this slot kills the process —
    /// see `editor::ghost`'s module doc for the full cancellation chain.
    ghost_request: Option<gpui::Task<()>>,
    /// Bumped by every dismissal; a request result whose epoch no longer matches is
    /// discarded. Belt to the version-stamp's braces: the stamp proves the *document*
    /// is unchanged, the epoch proves the user did not dismiss in the meantime.
    ghost_epoch: u64,
    /// The server's inlay hints for the visible band, in byte offsets (#93 follow-up).
    ///
    /// Sorted ascending, which `render_rows` relies on when slicing them per row. Held here
    /// rather than fetched during render for `diagnostics`' reason: these are painted every
    /// frame and the offset conversion happens once, when the response lands.
    hints: Vec<ResolvedHint>,
    /// Where the OS is currently composing, if it is (#18).
    ///
    /// Non-`None` between `setMarkedText:` and the commit — the accent floating over `a`
    /// after ⌥N on a US layout, the underlined romaji before a Japanese candidate is
    /// chosen. See `editor::ime` for why this holds a *range* and not the text.
    marked: crate::editor::ime::Marked,
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
            breakpoints: Vec::new(),
            debug_row: None,
            link_hint: None,
            dragging: false,
            alt_drag: None,
            hover_diagnostic: None,
            text_origin_x: None,
            folds: crate::editor::folds::Folds::default(),
            cursor_row_origin_y: None,
            caret_visible: true,
            blink: None,
            window_activation: None,
            ghost: None,
            ghost_debounce: None,
            ghost_request: None,
            ghost_epoch: 0,
            hints: Vec::new(),
            marked: crate::editor::ime::Marked::default(),
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

    /// Replaces the breakpoint rows drawn in the gutter (#30).
    ///
    /// Called by the workspace whenever the store changes, and with an empty vector for a
    /// file that has none — clearing matters for the reason `set_diagnostics` documents.
    pub fn set_breakpoints(&mut self, rows: Vec<usize>, cx: &mut Context<Self>) {
        self.breakpoints = rows;
        cx.notify();
    }

    /// Marks the row execution is stopped on, or `None` when it is not in this file.
    pub fn set_debug_row(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        self.debug_row = row;
        cx.notify();
    }

    /// The cursor's row, for the actions that operate on a line rather than a selection.
    pub fn cursor_row(&self) -> usize {
        self.document.cursor_point().row
    }

    /// Replaces the inlay hints drawn over this document (#93 follow-up).
    ///
    /// Called by the workspace when a response lands, and with an empty vector when the
    /// server goes away or the buffer is edited — see `WorkspaceView::clear_inlay_hints` for
    /// why clearing on edit is not optional: a hint's offset describes the text it was
    /// computed against, and after an edit above it, it points at something else.
    pub fn set_hints(&mut self, hints: Vec<ResolvedHint>) {
        self.hints = hints;
    }

    /// Whether any hints are currently drawn.
    ///
    /// Exists so a clear can skip its `notify` when there is nothing to clear — an editor
    /// with no hints must not be repainted by every edit on this feature's account (#93).
    pub fn has_hints(&self) -> bool {
        !self.hints.is_empty()
    }

    /// The rows currently on screen, for the caller deciding what to ask the server about.
    ///
    /// `0..0` until the first frame has rendered — a caller must treat that as "nothing to
    /// ask about yet" rather than as row zero, which is what `request_inlay_hints` does.
    pub fn visible_rows(&self) -> Range<usize> {
        self.visible_rows.clone()
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

    /// Whether the OS is composing into this editor right now (#18).
    ///
    /// The one piece of IME state a test cannot read off the buffer: marked text *is* the
    /// buffer's text, so "is this provisional?" is invisible from the outside — which is
    /// the whole reason `Marked` exists as a separate field.
    #[cfg(test)]
    pub fn is_composing_for_test(&self) -> bool {
        self.marked.is_composing()
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
    /// Folds the block containing the cursor (#82, ⌥⌘[).
    ///
    /// The cursor cannot be left inside the fold — the render pass would reveal it
    /// again next frame (its safety rule) — so it moves to the end of the header, which
    /// is where VS Code leaves it and where typing visibly continues.
    pub fn fold_block_at_cursor(&mut self, cx: &mut Context<Self>) {
        let text = self.document.buffer.text();
        let cursor = self.document.cursor_point();
        let Some((header, body)) = crate::editor::folds::enclosing_block(&text, cursor.row) else {
            return;
        };
        self.folds.fold(body, self.document.buffer.len_lines());
        if self.folds.is_hidden(cursor.row) {
            let offset = self
                .document
                .buffer
                .point_to_offset(Point::new(header, self.document.buffer.line_len(header)));
            self.document.move_to(offset, false);
        }
        cx.notify();
    }

    /// Unfolds the fold whose header is the cursor's line (#82, ⌥⌘]).
    pub fn unfold_at_cursor(&mut self, cx: &mut Context<Self>) {
        if self.folds.unfold_at_header(self.document.cursor_point().row) {
            cx.notify();
        }
    }

    /// Folds every top-level block (#82). The cursor's own block is revealed again if
    /// the sweep hid it — same rule as everywhere: no invisible caret.
    pub fn fold_all(&mut self, cx: &mut Context<Self>) {
        let text = self.document.buffer.text();
        let lines = self.document.buffer.len_lines();
        for body in crate::editor::folds::top_level_blocks(&text) {
            self.folds.fold(body, lines);
        }
        let cursor = self.document.cursor_point().row;
        if self.folds.is_hidden(cursor) {
            self.folds.unfold_containing(cursor);
        }
        cx.notify();
    }

    pub fn unfold_all(&mut self, cx: &mut Context<Self>) {
        self.folds.clear();
        cx.notify();
    }

    fn on_fold_block(&mut self, _: &FoldBlock, _w: &mut Window, cx: &mut Context<Self>) {
        self.fold_block_at_cursor(cx);
    }

    fn on_unfold_block(&mut self, _: &UnfoldBlock, _w: &mut Window, cx: &mut Context<Self>) {
        self.unfold_at_cursor(cx);
    }

    /// The buffer line a list row shows — the fold conversion, shared by the render
    /// callback and the tests so they cannot diverge.
    fn row_line_index(&self, row: usize) -> usize {
        self.folds.line_of_row(row, self.document.buffer.len_lines())
    }

    /// The visible rows' buffer lines, for tests — through `row_line_index`, the same
    /// function the render callback uses.
    #[cfg(test)]
    pub fn visible_lines_for_test(&self) -> Vec<usize> {
        let lines = self.document.buffer.len_lines();
        (0..self.folds.visible_count(lines)).map(|row| self.row_line_index(row)).collect()
    }

    fn scroll_cursor_into_view(&mut self) {
        let line = self.document.cursor_point().row;
        // A cursor inside a fold reveals it before scrolling — the render pass enforces
        // the same rule, but scrolling runs first and needs a row to aim at now.
        if self.folds.is_hidden(line) {
            self.folds.unfold_containing(line);
        }
        let Some(row) = self.folds.row_of_line(line) else { return };
        let last_row = self.folds.visible_count(self.document.buffer.len_lines()).saturating_sub(1);

        if let Some((item, strategy)) = autoscroll_fit(row, &self.visible_rows, last_row) {
            self.scroll.scroll_to_item(item, strategy);
        }
    }

    /// Filters a raw keypress down to the literal text it means, or `None`.
    ///
    /// Split out of [`Self::on_key_down`] as a pure decision so the "is this text?" rule can
    /// be tested without a window, and so the one caller left is obviously a filter plus a
    /// call. Everything with a command/control modifier, and every navigation key, is left
    /// to the keymap in `actions.rs`.
    fn typed_text(event: &KeyDownEvent) -> Option<&str> {
        let keystroke = &event.keystroke;
        let modifiers = &keystroke.modifiers;

        // `platform` is cmd on macOS. Chords are keybindings, not text.
        if modifiers.platform || modifiers.control || modifiers.function {
            return None;
        }

        // `key_char` is the literal character *after* the layout applies shift and dead
        // keys ("ß" for option-s), and is None for command chords. `key` is the
        // layout-independent label — right for bindings, wrong for insertion.
        let text = keystroke.key_char.as_deref()?;

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
            return None;
        }

        if text.is_empty() || text.chars().all(|c| c.is_control()) {
            return None;
        }

        Some(text)
    }

    /// Handles a raw keypress, for characters the action system does not cover.
    ///
    /// # Why this no longer inserts anything on macOS (#18)
    ///
    /// It used to, and that was correct while nothing else could. Now that the editor
    /// registers an [`EntityInputHandler`], insertion has to happen in exactly one place,
    /// and gpui's key path makes it clear which: `handle_key_event` in
    /// `gpui/src/platform/mac/window.rs` dispatches `KeyDown` **first** and only hands the
    /// event to the IME if the callback reports it unhandled. Since a `div`'s
    /// `on_key_down` listener does not stop propagation, the event reaches *both* — so a
    /// handler that still inserted here would type every character twice the moment an
    /// input handler existed.
    ///
    /// Letting the IME own it is not merely the way to avoid the double: it is the whole
    /// feature. A dead key (⌥N, then `a`) produces **no `key_char` at all** on the first
    /// press — the layout is holding a combining state that only `NSTextInputClient` can
    /// see. There is nothing for this function to insert, which is exactly why `ã` was
    /// unreachable before.
    ///
    /// What remains here is the platforms-without-an-IME fallback. `handle_input` only
    /// registers a handler while this editor is focused, and gpui's test platform reaches
    /// the input handler through the same two-step path, so this branch fires only when
    /// nothing else will take the keystroke — never in addition to it.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = Self::typed_text(event) else { return };

        // The `!` of the double-insert rule above. When the platform will route this
        // keystroke to `replace_text_in_range`, doing anything here is the bug; when it
        // will not, doing nothing here loses the character. `window.is_focused` on this
        // editor's handle is the same condition `handle_input` gates registration on, so
        // the two answers cannot drift.
        if self.focus_handle.is_focused(window) {
            return;
        }

        self.insert_text(text, cx);
    }

    /// Puts typed text into the buffer and runs everything a keystroke owes afterwards.
    ///
    /// The single insertion tail, shared by [`Self::on_key_down`] and the platform's
    /// `replace_text_in_range`. It exists as one function precisely because those two must
    /// not diverge: a character typed through the IME has to auto-close its bracket, re-arm
    /// the ghost debounce, and emit [`EditorEvent::Typed`] identically to one that did not.
    fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
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
            // Multi-cursor typing still discards a ghost and re-arms the debounce (#29);
            // the fire itself declines multi-cursor states, so no request results.
            self.ghost_after_edit(cx);
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
        // This path skips `after_edit` (no search rescan for a plain keystroke without
        // the find bar involved), so the ghost's edit hook is called by hand (#29).
        self.ghost_after_edit(cx);
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

    /// Escape with a ghost showing dismisses it (#29); with multiple cursors it collapses
    /// to one; otherwise the key belongs to whoever else wants it (`propagate`), so
    /// find-dismissal and friends keep working. The ghost goes first because it is the
    /// most recent thing on screen — Escape peels the newest layer, every editor's rule.
    fn cancel_multi_cursor(
        &mut self,
        _: &crate::actions::Cancel,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cancel_ghost(cx) {
            return;
        }
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
        self.tab_impl(cx);
    }

    /// Split from the handler so tests can drive it without a `Window` (unused anyway).
    fn tab_impl(&mut self, cx: &mut Context<Self>) {
        // A visible ghost claims Tab first (#29): accepting is what the dimmed text has
        // been promising, and the validity stamp means this can never fire on a stale one.
        if self.accept_ghost(cx) {
            return;
        }
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
            // Through the multi-cursor door, so ⌘X with several selections removes all
            // of them as one undo step — `insert("")` would collapse to the primary and
            // leave the other selections' text behind with the clipboard claiming it.
            self.document.insert_at_all_cursors("");
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
        // An edit is the one thing that both discards a ghost and asks for a new one (#29).
        self.ghost_after_edit(cx);
        cx.notify();
    }

    fn after_move(&mut self, cx: &mut Context<Self>) {
        self.scroll_cursor_into_view();
        // Motion holds the caret solid too, not just typing: holding ↓ to scroll through a
        // file with the caret strobing is the same distraction, and arriving somewhere with
        // the caret in its dark half means not being able to see where you landed.
        self.restart_blink(cx);
        // Movement discards the ghost and cancels any pending request, but does not ask
        // for a new one — only edits do (#29). The validity stamp already keeps a moved
        // cursor from rendering or accepting it; this reclaims the memory and the socket.
        self.dismiss_ghost();
        cx.notify();
    }

    // --- ghost text (#29) --------------------------------------------------------
    //
    // The suggestion itself, its cleaning and its request live in `editor::ghost`; what
    // belongs here is the *lifecycle* — when a request may fire, when a result may land,
    // and when everything is thrown away. The rules, from the roadmap:
    //
    //   - nothing fires unless `ai.autocomplete` is on (off by default);
    //   - 400ms after the last edit, and only while the document is still idle;
    //   - at most one request in flight, a newer trigger kills the older curl (#93);
    //   - any edit or cursor move discards; Tab accepts; Escape dismisses.

    /// The ghost, if it is showing *right now* — stamped for exactly this buffer state.
    fn visible_ghost(&self) -> Option<&GhostSuggestion> {
        self.ghost.as_ref().filter(|ghost| ghost.is_valid_for(&self.document))
    }

    /// Throws away the suggestion, the pending timer and the in-flight request.
    ///
    /// Dropping the tasks is the cancellation (the blink's contract), and dropping the
    /// request task kills its `curl` child — so after this returns, the feature costs
    /// nothing until the next edit, which is #93's idle rule.
    fn dismiss_ghost(&mut self) {
        self.ghost_epoch += 1;
        self.ghost_debounce = None;
        self.ghost_request = None;
        self.ghost = None;
    }

    /// The edit half of the lifecycle: discard, then re-arm the debounce while enabled.
    fn ghost_after_edit(&mut self, cx: &mut Context<Self>) {
        self.dismiss_ghost();
        // Tests drive edits by the thousand and must never find a timer or a subprocess
        // behind one — the same blanket guard the update check uses.
        if cfg!(test) {
            return;
        }
        if !crate::settings::current(cx).ai_autocomplete_enabled() {
            return;
        }
        let epoch = self.ghost_epoch;
        self.ghost_debounce = Some(cx.spawn(async move |this, cx| {
            // An edit inside this window drops the task and starts a new one, so a burst
            // of typing costs zero requests — the tree watcher's latest-wins shape.
            cx.background_executor().timer(ghost::DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.ghost_fire(epoch, cx));
        }));
    }

    /// The debounce elapsed with no further edits: snapshot the context and go.
    fn ghost_fire(&mut self, epoch: u64, cx: &mut Context<Self>) {
        self.ghost_debounce = None;
        if epoch != self.ghost_epoch {
            return; // dismissed while the timer slept
        }
        // "Insert at the cursor" must be one well-defined place.
        if !self.document.selection.is_empty() || self.document.has_multiple_cursors() {
            return;
        }
        // Mid-composition the buffer holds provisional text — half-typed romaji, an `a`
        // that is about to become `ã`. Completing *that* is asking a model to continue a
        // word the user has not finished spelling, and the suggestion would be invalidated
        // by the very next composition step. The debounce is 400ms and a composition
        // outlives it easily, so this is reached in practice rather than theoretically.
        if self.marked.is_composing() {
            return;
        }
        let settings = crate::settings::current(cx);
        if !settings.ai_autocomplete_enabled() {
            return; // switched off during the debounce
        }
        let provider = crate::ai::Provider::from_setting(settings.ai_provider());
        let base_url = settings.ai_base_url().to_string();
        let model = settings.ai_completion_model().to_string();

        let offset = self.document.selection.head;
        let version = self.document.buffer.version();
        let text = self.document.buffer.text();
        let user_turn = ghost::build_user_turn(&text, offset);
        // The cursor line's tail, for the echo-stripping half of `clean_completion`.
        let cursor = self.document.cursor_point();
        let line = self.document.buffer.line(cursor.row);
        let line_before_cursor = line[..cursor.column.min(line.len())].to_string();

        // Replacing the slot cancels any older request and kills its curl — at most one
        // in flight, which is the #93 budget for a feature that runs between keystrokes.
        self.ghost_request = Some(cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(ghost::fetch_completion(provider, base_url, model, user_turn))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.ghost_request = None;
                if this.ghost_epoch != epoch {
                    return; // dismissed while the request ran
                }
                let raw = match outcome {
                    Ok(raw) => raw,
                    Err(err) => {
                        // Silence, not a toast: nobody asked a question (the update
                        // check's rule). The log keeps the why for whoever goes looking.
                        tracing::debug!("ghost completion failed: {err}");
                        return;
                    }
                };
                let cleaned = ghost::clean_completion(&raw, &line_before_cursor);
                if cleaned.is_empty() {
                    return;
                }
                // Shown only if the document is byte-for-byte the one the request
                // described — otherwise the suggestion is about a file that no longer
                // exists, and showing it would be confident wrongness (RISKS.md #4).
                let unchanged = this.document.buffer.version() == version
                    && this.document.selection.is_empty()
                    && this.document.selection.head == offset
                    && !this.document.has_multiple_cursors();
                if !unchanged {
                    return;
                }
                this.ghost = Some(GhostSuggestion { text: cleaned, at_offset: offset, version });
                cx.notify();
            });
        }));
    }

    /// Tab's first meaning while a ghost shows: insert it whole, as one undo step.
    ///
    /// Through `Document::insert` — the same door every programmatic insertion uses — so
    /// undo, selection collapse and syntax sync all behave exactly as if the user had
    /// typed it. Returns whether a ghost was accepted, so `tab` knows to keep its hands
    /// off the indent behaviour.
    fn accept_ghost(&mut self, cx: &mut Context<Self>) -> bool {
        if self.visible_ghost().is_none() {
            return false;
        }
        let ghost = self.ghost.take().expect("visible_ghost checked");
        self.dismiss_ghost();
        self.document.insert(&ghost.text);
        self.after_edit(cx);
        true
    }

    /// Escape's first meaning while a ghost shows: dismiss it and consume the key.
    fn cancel_ghost(&mut self, cx: &mut Context<Self>) -> bool {
        if self.visible_ghost().is_none() {
            return false;
        }
        self.dismiss_ghost();
        cx.notify();
        true
    }

    /// The ghost's continuation lines and where to draw them, in window coordinates.
    ///
    /// The first ghost line is spliced into the cursor row's own text (see
    /// `render_rows`); lines two onward cannot be — `uniform_list` owns the row grid,
    /// and inserting rows would shift every line number and fold below the cursor. So
    /// they render as a workspace overlay at window coordinates, the hover card's
    /// arrangement: the editor measures (only it can), the workspace places (the card
    /// must sit above every panel).
    ///
    /// `None` when there is no ghost, no continuation, or the cursor row was not painted
    /// last frame — no on-screen caret means nowhere honest to anchor.
    pub fn ghost_overlay(&self, cx: &App) -> Option<(Vec<SharedString>, gpui::Point<Pixels>)> {
        let ghost = self.visible_ghost()?;
        let rest: Vec<SharedString> = ghost
            .text
            .split('\n')
            .skip(1)
            .map(|line| SharedString::from(line.to_string()))
            .collect();
        if rest.is_empty() {
            return None;
        }
        let fonts = Fonts::get(cx);
        let x = self.text_origin_x?;
        let y = self.cursor_row_origin_y? + fonts.line_height();
        Some((rest, gpui::point(x, y)))
    }

    /// Plants a suggestion at the current cursor, stamped as a fresh request would be.
    #[cfg(test)]
    pub fn set_ghost_for_test(&mut self, text: &str) {
        self.ghost = Some(GhostSuggestion {
            text: text.to_string(),
            at_offset: self.document.selection.head,
            version: self.document.buffer.version(),
        });
    }

    /// The stored ghost's text, valid or not — `None` proves a dismissal actually
    /// cleared state rather than leaving an invisible corpse behind.
    #[cfg(test)]
    pub fn ghost_for_test(&self) -> Option<String> {
        self.ghost.as_ref().map(|ghost| ghost.text.clone())
    }

    /// Whether the ghost would render this frame.
    #[cfg(test)]
    pub fn ghost_visible_for_test(&self) -> bool {
        self.visible_ghost().is_some()
    }

    /// Tab's decision path, minus the `Window` the action handler signature drags in.
    #[cfg(test)]
    pub fn tab_for_test(&mut self, cx: &mut Context<Self>) {
        self.tab_impl(cx);
    }

    /// Escape's ghost half, for the same reason.
    #[cfg(test)]
    pub fn cancel_ghost_for_test(&mut self, cx: &mut Context<Self>) -> bool {
        self.cancel_ghost(cx)
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

        // Column selection (#82): one selection per row of the ⌥-dragged rectangle, each
        // spanning the same two window-x edges through its own row's text. The pointer's
        // row is the primary, so typing after the drag flows from where the user is
        // looking. A row shorter than the anchor x contributes a caret at its end —
        // clamping is `offset_at`'s, the same as a click past end-of-line.
        if let Some(drag) = self.alt_drag.as_mut()
            && event.pressed_button == Some(MouseButton::Left)
        {
            drag.moved = true;
            // Copied out so the borrow of `alt_drag` ends before `offset_at` needs `self`.
            let (anchor_x, anchor_row) = (drag.anchor_x, drag.anchor_row);

            let (top, bottom) = (anchor_row.min(row), anchor_row.max(row));
            let mut selections: Vec<Selection> = Vec::with_capacity(bottom - top + 1);
            for r in top..=bottom {
                let start = self.offset_at(anchor_x, r, window, cx);
                let end = self.offset_at(event.position.x, r, window, cx);
                selections.push(Selection { anchor: start, head: end });
            }
            // The pointer's row leads; it is the last pushed when dragging down and the
            // first when dragging up.
            let primary_at = if row >= anchor_row { selections.len() - 1 } else { 0 };
            let primary = selections.remove(primary_at);
            self.document.set_selections(primary, selections);
            cx.notify();
            return;
        }

        // Drag-selection first: while the button is down the mouse is selecting, not
        // hovering, and a diagnostic card popping open mid-drag would sit on top of the
        // text being selected. Crossing rows works because every row runs this handler.
        if self.dragging && event.pressed_button == Some(MouseButton::Left) {
            self.document.move_to(offset, true);
            cx.notify();
            return;
        }

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
        // ⌥ starts either a caret click or a column drag, and which one is not knowable
        // at mouse-down — so the click is *deferred*: if the mouse moves before release
        // this becomes a column selection, and if it does not, mouse-up applies the
        // caret. Committing the caret here and dragging after would leave a stray cursor
        // at the anchor of every column selection.
        if event.modifiers.alt && event.click_count == 1 {
            self.alt_drag = Some(AltDrag {
                anchor_x: event.position.x,
                anchor_row: row,
                click_offset: offset,
                moved: false,
            });
            cx.notify();
            return;
        }

        match event.click_count {
            1 => {
                // Shift-click extends the existing selection, matching every other editor.
                self.document.move_to(offset, event.modifiers.shift);
                // The most basic gesture there is, missing until a user asked for it in
                // so many words: press, drag, and the selection follows the mouse. The
                // move handler extends while this flag holds; release clears it.
                self.dragging = true;
            }
            2 => self.document.select_word_at(offset),
            3 => self.document.select_line_at(row),
            _ => self.document.select_all(),
        }

        // A click is a cursor move: the ghost dies with it, and so does any pending
        // request — the after_move rule, arriving by mouse (#29).
        self.dismiss_ghost();

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
    /// The gutter's quick-fix bulb was clicked.
    ///
    /// Carries nothing, and that is deliberate: the workspace runs the *same* handler ⌘.
    /// runs, which reads the cursor. The click moves the cursor to that line first, so
    /// there is one definition of "where the fix is asked about" rather than two that can
    /// drift — a bulb that fixed a different line than the chord would be worse than no
    /// bulb.
    QuickFix,
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The OS's side of text input: marked text, candidate windows, the character palette (#18).
///
/// # The one rule
///
/// **Every offset in this trait is a UTF-16 code-unit offset, and every offset below it is a
/// byte offset.** The conversion happens in `editor::ime` and nowhere else; a method here
/// that passed a `range_utf16` straight to `Document` would compile, pass every ASCII test,
/// and corrupt the first line containing `ã`. Each method converts at its first statement,
/// which is why they all read the same way.
///
/// # Why this is worth having beyond CJK
///
/// The candidate window is the visible half of the feature; dead keys are the half used
/// daily here. On a US layout ⌥N then `a` gives `ã`, and the first press produces no
/// `key_char` at all — the layout is holding combining state that only `NSTextInputClient`
/// can observe. Without this trait implemented, that first press is simply lost and the
/// second inserts a bare `a`. That is the bug, and it is why the issue calls dead-key
/// composition the everyday case rather than an edge case.
impl gpui::EntityInputHandler for EditorView {
    /// The current selection, as the platform counts.
    ///
    /// `ignore_disabled_input` is about read-only fields, which this editor does not have —
    /// every open document is editable — so there is nothing to decline.
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        let selection = self.document.selection;
        let range = ime::byte_range_to_utf16(&self.document.buffer, selection.range());
        Some(gpui::UTF16Selection {
            range,
            // `reversed` means the *head* is at the low end — a selection made by dragging
            // backwards. The platform uses it to decide which end an arrow key collapses to,
            // so reporting it wrong makes shift-selection feel inverted inside a composition.
            reversed: selection.head < selection.anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let range = self.marked.range()?;
        Some(ime::byte_range_to_utf16(&self.document.buffer, range))
    }

    /// The text behind a range the platform is asking about.
    ///
    /// `adjusted_range` reports back the range actually served, which matters when the
    /// request lands mid-character: the platform asked in UTF-16 units that may split a
    /// surrogate pair, and it needs to know the answer covers slightly different ground
    /// rather than assuming a 1:1 correspondence it can index into.
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let bytes = ime::utf16_range_to_bytes(&self.document.buffer, range_utf16);
        *adjusted_range = Some(ime::byte_range_to_utf16(&self.document.buffer, bytes.clone()));
        Some(self.document.buffer.slice(bytes))
    }

    /// A commit: the composition is over and this text is now ordinary buffer content.
    ///
    /// This is also the path *every plain keystroke* takes on macOS — `insertText:` is what
    /// AppKit calls for an unaccented `a` just as much as for a chosen kanji — which is why
    /// it funnels into [`EditorView::insert_text`], the same tail `on_key_down` used to run
    /// inline. Committed text gets auto-pairing and the `Typed` event because it *is* typed
    /// text; that is the difference between here and `replace_and_mark_text_in_range`.
    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The range to replace, in bytes. `None` means "the marked text if composing,
        // otherwise the selection" — the platform's own default, and getting it wrong is
        // how a committed candidate ends up appended after the romaji instead of replacing
        // it.
        let target = replacement_range
            .map(|range| ime::utf16_range_to_bytes(&self.document.buffer, range))
            .or_else(|| self.marked.range());

        if let Some(target) = target {
            self.document.select_range(target);
        }
        self.marked.clear();

        self.insert_text(text, cx);
    }

    /// A composition step: text is in the buffer but the user has not committed to it.
    ///
    /// # Why this deliberately avoids `insert_text`
    ///
    /// Marked text is **not typed text**, and the difference is the whole reason this is a
    /// separate method rather than a flag on the one above. Composing `"` on a Brazilian
    /// layout must not auto-close into `""`, and a `=` mid-composition must not become
    /// `=>` — those conveniences fire on a decision the user has not made yet, and the
    /// pairs they insert are outside the marked range, so the next composition step
    /// replaces the wrong bytes and leaves the orphan behind. The same reasoning excludes
    /// the `Typed` event: a completion popup opened on a half-composed character would be
    /// filtering on text that is about to be replaced.
    ///
    /// So this splices plainly and records where. Everything a commit owes gets paid by
    /// `replace_text_in_range` when the user actually commits.
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = range_utf16
            .map(|range| ime::utf16_range_to_bytes(&self.document.buffer, range))
            .or_else(|| self.marked.range())
            .unwrap_or_else(|| self.document.selection.range());

        let start = self.document.replace_range_plainly(target, new_text);
        let marked = start..start + new_text.len();
        self.marked.set(marked.clone());

        // The caret inside the composition — where the IME says the user is within the
        // romaji, not necessarily its end. Offsets are relative to `new_text`, so they are
        // converted against the text just spliced rather than against the whole buffer.
        let caret = new_selected_range
            .map(|range| {
                let local = ime::utf16_range_to_bytes(&self.document.buffer, range);
                marked.start + local.end.min(new_text.len())
            })
            .unwrap_or(marked.end);
        self.document.select_range(caret..caret);

        self.scroll_cursor_into_view();
        self.restart_blink(cx);
        cx.notify();
    }

    /// The composition was abandoned — Escape, or a click elsewhere.
    ///
    /// The text stays where it is. That is the platform's contract: `unmarkText` means
    /// "stop treating this as provisional", not "undo it". Deleting it here is what makes a
    /// half-typed candidate vanish when the user clicks away, which is not what any other
    /// editor does.
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked.range().is_some() {
            self.marked.clear();
            cx.notify();
        }
    }

    /// Where to put the candidate window, in window coordinates.
    ///
    /// Anchored to the caret rather than to the requested range's own start. The range is
    /// almost always the marked text, whose start is where the composition *began* — put
    /// the palette there and it drifts left of the text as the user keeps typing. The
    /// height is one line so the popup clears the row it belongs to.
    ///
    /// `None` before the first prepaint, and `None` while the cursor's row is scrolled out
    /// of view, for the reason [`EditorView::cursor_position`] documents: there is no
    /// on-screen caret to anchor to.
    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let origin = self.cursor_position(window, cx)?;
        let line_height = Fonts::get(cx).line_height();
        Some(Bounds::new(origin, gpui::size(px(1.0), line_height)))
    }

    /// Which character a screen point is over, for the palette's own hit-testing.
    ///
    /// Not implemented: mapping a *window* point to an offset needs the row under it, and
    /// the row grid lives inside `uniform_list`, which does not expose a point-to-row query
    /// outside its render callback — the same reason `on_row_mouse_down` is registered per
    /// row rather than once on the editor. `None` is a legal answer here (AppKit falls back
    /// to not offering the lookup), and the feature it degrades is dragging *within* the
    /// candidate window, not composition itself.
    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fonts = Fonts::get(cx);
        let buffer_lines = self.document.buffer.len_lines();
        // Folding's two safety rules, enforced at the one funnel every change flows
        // through. An edit that changed the line count invalidates every fold (the
        // ranges name lines that moved); a cursor that lands inside a fold reveals it —
        // a caret the user cannot see is an edit about to land somewhere invisible,
        // which is exactly the corruption the issue warned about.
        self.folds.invalidate(buffer_lines);
        let cursor = self.document.cursor_point();
        if self.folds.is_hidden(cursor.row) {
            self.folds.unfold_containing(cursor.row);
        }
        let row_count = self.folds.visible_count(buffer_lines);

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
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|editor, _ev: &gpui::MouseUpEvent, _w, cx| {
                    editor.dragging = false;
                    // A deferred ⌥click that never became a drag lands its caret now.
                    if let Some(drag) = editor.alt_drag.take()
                        && !drag.moved
                    {
                        editor.document.add_cursor_at(drag.click_offset);
                        editor.restart_blink(cx);
                        cx.notify();
                    }
                }),
            )
            // Released outside the editor — over the sidebar, past the window edge. The
            // terminal's drag ends the same two ways for the same reason.
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|editor, _ev: &gpui::MouseUpEvent, _w, _cx| {
                    editor.dragging = false;
                    // Released off the editor: a column drag ends where it was; a
                    // deferred click aimed at text the pointer has left applies nothing.
                    editor.alt_drag = None;
                }),
            )
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
            .on_action(cx.listener(Self::on_fold_block))
            .on_action(cx.listener(Self::on_unfold_block))
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
            // The OS's input handler, registered during this element's paint (#18). A
            // zero-sized sibling of the row list rather than something inside it: the
            // registration is per *editor*, and `uniform_list`'s callback runs per visible
            // row. See `editor::input_element` for why paint is the only phase this can
            // happen in.
            .child(InputHandlerElement::new(cx.entity(), self.focus_handle.clone()))
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

        // The ghost's first line, spliced into the cursor row's own text (#29). Resolved
        // once per frame through the validity stamp, so a stale suggestion costs one
        // comparison and paints nothing. Lines beyond the first are the workspace
        // overlay's job — see `ghost_overlay`.
        let ghost_inline: Option<(usize, String)> = self
            .visible_ghost()
            .map(|ghost| {
                (cursor.column, ghost.text.split('\n').next().unwrap_or_default().to_string())
            })
            .filter(|(_, first)| !first.is_empty());

        let entity = cx.entity();

        range
            .map(|row| {
                // THE fold conversion: this row shows this buffer line, and everything
                // below — content, gutter number, mouse handlers — is built from the
                // line. `offset_at` and every consumer beneath it never see a folded
                // row at all (see `editor/folds.rs` module doc). One shared function
                // with `visible_lines_for_test`, so the test exercises the same code
                // the render callback runs — a private copy here is how the first
                // version survived a mutation that broke the render path.
                let line_index = self.row_line_index(row);
                let line = self.document.buffer.line(line_index);
                let line_start = self.document.buffer.point_to_offset(Point::new(line_index, 0));
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

                let is_cursor_row = line_index == cursor.row;
                // Both are lookups over lists that hold at most a handful of entries for
                // this file, done per visible row — a screenful, not the document.
                let has_breakpoint = self.breakpoints.contains(&line_index);
                let is_debug_row = self.debug_row == Some(line_index);
                // Each selection's slice of this row, in line-local bytes, for precise
                // painting (#82). The old full-row tint made a word selection look like a
                // line selection — and on themes where hover and selected share a value
                // (one_dark_pro does), ⌘D's first press changed nothing visible at all,
                // which is exactly how it got reported as dead.
                let row_selections: Vec<Range<usize>> = all_selections
                    .iter()
                    .map(|sel| sel.range())
                    .filter(|range| {
                        !range.is_empty() && range.start < line_end && range.end > line_start
                    })
                    .map(|range| {
                        range.start.max(line_start) - line_start
                            ..range.end.min(line_end) - line_start
                    })
                    .collect();
                let row_selected = !row_selections.is_empty();
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
                let handler_line = line_index;

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
                                if handler_line == cursor_row {
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
                                editor.on_row_mouse_down(&event, handler_line, window, cx);
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
                                editor.on_row_mouse_move(&event, handler_line, window, cx);
                            });
                        }
                    })
                    .on_hover({
                        let entity = entity.clone();
                        move |entered, _window, cx| {
                            if !entered {
                                entity.update(cx, |editor, cx| {
                                    editor.on_row_hover_out(handler_line, cx)
                                });
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
                            // The quick-fix bulb (the ⌘. discoverability fix), or the
                            // line's number. **Replacing** the number rather than sitting
                            // beside it, because the gutter is one fixed width and a bulb
                            // added alongside would either widen the column on the cursor
                            // row — every line of code jumping sideways as the caret moves
                            // — or squeeze the digits into a narrower space than the rows
                            // above. The number is the thing that can be spared: it is
                            // still readable on every other row, and the row it vanishes
                            // from is the one the caret is already marking.
                            //
                            // Shown only on the cursor row, and only from diagnostics
                            // already in hand — never an LSP round trip to decide whether
                            // to draw it, which would be a request per cursor move. See
                            // `row_diagnostics` above: the editor is told about
                            // diagnostics, so this costs nothing.
                            //
                            // A breakpoint and the current statement replace the number for
                            // the same reason, and they outrank the bulb: while stopped,
                            // where execution *is* matters more than an offer to rewrite the
                            // line. The arrow wins over the dot when a breakpoint is the
                            // thing that stopped us, because the arrow is the transient fact
                            // and the dot is still readable from the panel.
                            //
                            // Both are glyphs, not colours: a red dot alone says nothing to
                            // anyone who cannot see red, and this is the one margin mark
                            // that changes what the program does.
                            .child(if is_debug_row {
                                div()
                                    .text_color(theme.warning)
                                    .child(SharedString::from("▶"))
                                    .into_any_element()
                            } else if has_breakpoint {
                                div()
                                    .text_color(theme.error)
                                    .child(SharedString::from("●"))
                                    .into_any_element()
                            } else if is_cursor_row && !row_diagnostics.is_empty() {
                                quick_fix_bulb(&theme, &entity).into_any_element()
                            } else {
                                // The buffer line's own number — after a fold, rows are
                                // not consecutive and pretending they are would misnumber
                                // every line below the fold.
                                SharedString::from((line_index + 1).to_string()).into_any_element()
                            }),
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
                            // Only the cursor row can carry a ghost: `at_offset` equals
                            // the selection head, and the head is on the cursor row by
                            // definition (#29).
                            let ghost = if is_cursor_row {
                                ghost_inline
                                    .as_ref()
                                    .map(|(column, first)| (*column, first.as_str()))
                            } else {
                                None
                            };
                            // This row's hints, rebased to columns within the line. Cheap
                            // per row — the list holds a screenful, and `hints_on_line` is a
                            // filter over it rather than a lookup that grows with the file.
                            let row_hints: Vec<_> =
                                hints_on_line(&self.hints, line_start..line_end).collect();
                            let rendered = styled_line(
                                &line,
                                line_start,
                                &spans,
                                &row_diagnostics,
                                row_link,
                                &row_selections,
                                brackets,
                                &row_matches,
                                ghost,
                                &row_hints,
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
        // List rows, converted to buffer lines through the fold map. The band spans
        // hidden lines in between — highlights computed for them are clipped away per
        // row, which costs a few spans and cannot mis-colour anything.
        let first_line = self.folds.line_of_row(rows.start, buffer.len_lines());
        let start = buffer.point_to_offset(Point::new(first_line, 0));
        let last_row = rows.end.saturating_sub(1);
        let last_line = self
            .folds
            .line_of_row(last_row, buffer.len_lines())
            .min(buffer.len_lines().saturating_sub(1));
        let end = buffer.point_to_offset(Point::new(last_line, buffer.line_len(last_line)));
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

/// The gutter's quick-fix bulb: what makes ⌘. visible.
///
/// The chord has worked since #19 and nobody could tell it was there — PhpStorm and VS Code
/// both put a bulb in the margin, and that glyph is the entire difference between a feature
/// and a secret. Clicking it emits [`EditorEvent::QuickFix`], which the workspace answers
/// with the *same* code-action request the chord runs; the request logic is not duplicated
/// here, and this element deliberately knows nothing about the language server.
///
/// Drawn in the accent colour because it is an offer, not a problem — the problem already
/// has its own underline on the text, and a second red mark in the margin would double-count
/// one diagnostic.
fn quick_fix_bulb(theme: &Theme, entity: &gpui::Entity<EditorView>) -> impl IntoElement {
    let entity = entity.clone();
    div()
        .id("quick-fix-bulb")
        .flex()
        .items_center()
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
            entity.update(cx, |_editor, cx| cx.emit(EditorEvent::QuickFix));
        })
        .child(
            svg()
                .path(crate::icons::LIGHTBULB)
                .size(px(13.0))
                // Set on the svg itself: gpui fills an SVG's alpha mask from the element's
                // own `text.color` and does not inherit it (the trap the tree, tab and
                // activity-bar icons all document).
                .text_color(theme.accent),
        )
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
    selections: &[Range<usize>],
    brackets: Option<(usize, usize)>,
    matches: &[(Range<usize>, bool)],
    ghost: Option<(usize, &str)>,
    hints: &[(usize, &ResolvedHint)],
    theme: &Theme,
    fonts: &Fonts,
) -> Line {
    let (text, highlights) =
        line_runs(line, line_start, spans, diagnostics, link, selections, brackets, matches, theme);
    // Guides come from the line's own indent, so they are computed here and painted by the
    // element — not folded into the runs above, which is what made them blocks (#108).
    // From the *real* text, before any ghost splice: a suggestion must not move guides.
    let guides = indent_guide_columns(&text).into_iter().map(|range| range.start).collect();
    let mut runs = to_runs(&text, &highlights, theme, fonts);
    let mut text = text;

    // Hints go in **before** the ghost, and the order is load-bearing in both directions.
    // Both are columns into the *real* line, so whichever goes second is measured against a
    // string that has already grown. Hints first is the cheaper direction to correct: there
    // is at most one ghost and its column needs a single adjustment (below), where fixing up
    // N hint columns for a preceding ghost would be the running arithmetic that
    // `splice_hint_runs` walks backwards precisely to avoid.
    splice_hint_runs(&mut text, &mut runs, hints, theme, fonts);

    // The ghost's first line, visually inserted at the cursor (#29). Splicing a *run*
    // is what keeps every colour after the cursor honest for free: `TextRun`s are
    // relative lengths, so text after the insertion point shifts with its own colours
    // still attached — no range arithmetic, no drift. Dim (`text_muted`), the roadmap's
    // "rendered distinctly, never confused with LSP completions".
    if let Some((at, ghost_text)) = ghost
        && !ghost_text.is_empty()
    {
        // Rebased past every hint spliced at or before the cursor: those bytes are now in
        // `text` and the ghost's column was measured without them. A hint sitting exactly at
        // the cursor counts, so the ghost lands after it rather than splitting it — the
        // cursor is between the hint and the code, which is where the suggestion belongs.
        let shift: usize =
            hints.iter().filter(|(column, _)| *column <= at).map(|(_, hint)| hint.text.len()).sum();
        let at = floor_boundary(&text, at + shift);
        let dim = TextRun {
            len: ghost_text.len(),
            font: fonts.font(),
            color: theme.text_muted,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        splice_ghost_run(&mut text, &mut runs, at, ghost_text, dim);
    }

    Line::new(text, runs, fonts.size, fonts.line_height()).with_guides(guides, theme.indent_guide)
}

/// Splices a line's inlay hints into its text and runs (#93 follow-up).
///
/// # Why this is the ghost's primitive applied N times, right to left
///
/// A hint has the ghost's problem — draw text that is not in the buffer — with two twists:
/// several land on one line, and they land at arbitrary columns rather than at the cursor.
/// The first is what dictates the direction. Each splice shifts every byte after it, so
/// inserting left to right would invalidate all the offsets still to come and each hint after
/// the first would land further and further right of where the server put it. Walking
/// **descending** means every insertion happens after the offsets not yet used, so each hint
/// splices against a prefix it still describes exactly. No running adjustment, nothing to
/// keep in step — the arithmetic that could drift simply is not performed.
///
/// The run splice is [`splice_ghost_run`] unchanged, which is the point: it is already tested
/// for the mid-run, between-run and past-the-end cases, and a hint hits all three.
///
/// # What this does not disturb
///
/// Only the *shaped* line. The buffer is untouched, and `line_runs` has already resolved
/// every real overlay — selections, diagnostics, brackets, search hits — into runs whose
/// lengths are relative. Text after an insertion therefore shifts carrying its own colours,
/// which is why hints cannot recolour code the way an offset-based approach would. Click and
/// selection arithmetic reads the buffer, not this string, so a hint cannot move the caret's
/// idea of a column either — the property that keeps hints unselectable for free.
///
/// `hints` must be sorted ascending by column ([`inlay::resolve`] guarantees it); this walks
/// them in reverse.
fn splice_hint_runs(
    text: &mut String,
    runs: &mut Vec<TextRun>,
    hints: &[(usize, &ResolvedHint)],
    theme: &Theme,
    fonts: &Fonts,
) {
    for (column, hint) in hints.iter().rev() {
        // A column past the line's end describes text that has moved since the server
        // answered — the buffer was edited and the response is stale. Skipped rather than
        // clamped: a hint drawn at a plausible-looking wrong column misattributes a type,
        // which is worse than the hint simply not appearing until the next response lands.
        if *column > text.len() {
            continue;
        }
        let at = floor_boundary(text, *column);
        let run = TextRun {
            len: hint.text.len(),
            font: fonts.font(),
            color: hint_color(hint.kind, theme),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        splice_ghost_run(text, runs, at, &hint.text, run);
    }
}

/// The colour for a hint of a given kind.
///
/// Both kinds are muted, and a type hint is muted *further*. The distinction was cheap —
/// `text_muted` already exists and the theme's comment colour sits beside it — and it earns
/// its keep because the two kinds answer different questions: a parameter name labels an
/// argument the reader can see, while a type is information from elsewhere in the program.
/// PhpStorm and Zed both differentiate them. An unknown kind gets the undifferentiated muted
/// colour rather than a third one invented here.
fn hint_color(kind: Option<HintKind>, theme: &Theme) -> gpui::Hsla {
    match kind {
        // Dimmer than a parameter name: a type annotation is the more repetitive of the two
        // and the one most worth receding when the eye is scanning code rather than reading it.
        Some(HintKind::Type) => theme.comment,
        Some(HintKind::Parameter) | None => theme.text_muted,
    }
}

/// Inserts `ghost` into `text` at byte `at` and a matching run into `runs`, splitting
/// the run that spans the insertion point.
///
/// Pure on purpose (#29): whether the splice lands mid-run, between runs, or past the
/// last run is exactly the arithmetic that can be wrong without a GPU, so it is the part
/// with tests. `runs` must cover `text` contiguously — `to_runs`' guarantee — and
/// `ghost_run.len` must equal `ghost.len()`.
fn splice_ghost_run(
    text: &mut String,
    runs: &mut Vec<TextRun>,
    at: usize,
    ghost: &str,
    ghost_run: TextRun,
) {
    text.insert_str(at, ghost);
    let mut consumed = 0usize;
    for index in 0..runs.len() {
        if consumed == at {
            runs.insert(index, ghost_run);
            return;
        }
        let len = runs[index].len;
        if consumed + len > at {
            // The insertion point is inside this run: split it around the ghost.
            let head = at - consumed;
            let mut tail = runs[index].clone();
            runs[index].len = head;
            tail.len = len - head;
            runs.insert(index + 1, ghost_run);
            runs.insert(index + 2, tail);
            return;
        }
        consumed += len;
    }
    // At (or clamped to) the end of the line — the common case: a cursor at end-of-line.
    runs.push(ghost_run);
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
    selections: &[Range<usize>],
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

    // The selection tint, as precise background runs (#82): exactly the selected bytes,
    // not the whole row. Line-local already — the caller clipped. Before the matches
    // merge on purpose: a search hit inside a selection keeps its own colour, the same
    // priority every editor gives it.
    for range in selections {
        let start = floor_boundary(&text, range.start.min(text.len()));
        let end = ceil_boundary(&text, range.end.min(text.len()));
        if start < end {
            highlights = merge_background(highlights, start..end, theme.selection);
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

    // --- ghost splice (#29) ------------------------------------------------------------
    //
    // The splice is the arithmetic half of ghost rendering: whether the dim run lands
    // between runs, splits one, or appends, and whether every colour after the cursor
    // stays on its own bytes. The painting half needs a GPU and stays on #35's list.

    /// A run of `len` bytes; the font and colour are irrelevant to the arithmetic.
    fn plain_run(len: usize) -> TextRun {
        TextRun {
            len,
            font: gpui::font("Menlo"),
            color: gpui::white(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    #[test]
    fn a_ghost_at_end_of_line_appends_one_run() {
        let mut text = String::from("$a = 1;");
        let mut runs = vec![plain_run(7)];
        splice_ghost_run(&mut text, &mut runs, 7, " // done", plain_run(8));
        assert_eq!(text, "$a = 1; // done");
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![7, 8]);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len(), "runs cover the text");
    }

    #[test]
    fn a_ghost_mid_run_splits_it_and_shifts_nothing_else() {
        // Two syntax runs; the cursor sits inside the first. The second run's *length*
        // is untouched, which is exactly what keeps its colour on its own (shifted)
        // bytes — the property the run splice was chosen for.
        let mut text = String::from("$user->save();");
        let mut runs = vec![plain_run(5), plain_run(9)];
        splice_ghost_run(&mut text, &mut runs, 2, "XY", plain_run(2));
        assert_eq!(text, "$uXYser->save();");
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![2, 2, 3, 9]);
    }

    #[test]
    fn a_ghost_on_a_run_boundary_slots_between() {
        let mut text = String::from("abcdef");
        let mut runs = vec![plain_run(3), plain_run(3)];
        splice_ghost_run(&mut text, &mut runs, 3, "-", plain_run(1));
        assert_eq!(text, "abc-def");
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![3, 1, 3]);
    }

    #[test]
    fn a_ghost_on_an_empty_line_is_the_only_run() {
        let mut text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();
        splice_ghost_run(&mut text, &mut runs, 0, "return;", plain_run(7));
        assert_eq!(text, "return;");
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![7]);
    }

    // --- inlay hint splice (#93 follow-up) ---------------------------------------------
    //
    // Same division as the ghost above: this is the arithmetic half, and it is the half
    // that can put a hint at the wrong column — the failure worse than no hint at all,
    // because a misplaced type annotation reads as a fact about the wrong variable.

    /// A resolved hint at `column` with `text`, the two fields the splice reads.
    fn hint_at(column: usize, text: &str) -> ResolvedHint {
        ResolvedHint { offset: column, text: text.to_string(), kind: None }
    }

    /// The theme and fonts the splice needs; neither affects the arithmetic under test.
    fn splice_hints(text: &mut String, runs: &mut Vec<TextRun>, hints: &[ResolvedHint]) {
        let pairs: Vec<(usize, &ResolvedHint)> =
            hints.iter().map(|hint| (hint.offset, hint)).collect();
        splice_hint_runs(text, runs, &pairs, &Theme::dark(), &Fonts::default());
    }

    #[test]
    fn two_hints_on_one_line_both_land_where_the_server_put_them() {
        // The property the descending walk exists for. Splicing left to right would put the
        // second hint six bytes (`: int` plus padding) to the right of its column, and the
        // error would compound with every further hint on the line.
        let mut text = String::from("$a = f($b);");
        let mut runs = vec![plain_run(11)];
        // Columns 2 (after `$a`) and 9 (after `$b`), against the *original* line — both
        // counted on the untouched string, which is exactly what a server sends.
        splice_hints(&mut text, &mut runs, &[hint_at(2, ": int"), hint_at(9, ": string")]);
        assert_eq!(text, "$a: int = f($b: string);");
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len(), "runs cover the text");
    }

    #[test]
    fn hints_do_not_disturb_the_lengths_of_the_runs_they_sit_between() {
        // A hint must not recolour code. `TextRun` lengths are relative, so an untouched
        // run keeps its colour on its own (shifted) bytes — the reason a run splice was
        // chosen over range arithmetic in the first place.
        let mut text = String::from("abcdef");
        let mut runs = vec![plain_run(3), plain_run(3)];
        splice_hints(&mut text, &mut runs, &[hint_at(3, "|")]);
        assert_eq!(text, "abc|def");
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![3, 1, 3]);
    }

    #[test]
    fn a_hint_past_the_end_of_the_line_is_skipped_not_clamped() {
        // The stale-response case: the buffer shrank after the server answered. Clamping
        // would draw the hint at the line end, attaching a type to whatever now sits there.
        let mut text = String::from("$a;");
        let mut runs = vec![plain_run(3)];
        splice_hints(&mut text, &mut runs, &[hint_at(2, ": int"), hint_at(99, ": gone")]);
        assert_eq!(text, "$a: int;", "the in-range hint still renders");
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
    }

    #[test]
    fn a_hint_at_end_of_line_appends_rather_than_vanishing() {
        // Where a return-type hint sits: `function f()|` with nothing after it.
        let mut text = String::from("function f()");
        let mut runs = vec![plain_run(12)];
        splice_hints(&mut text, &mut runs, &[hint_at(12, ": void")]);
        assert_eq!(text, "function f(): void");
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
    }

    #[test]
    fn a_hint_never_splits_a_multibyte_character() {
        // Portuguese identifiers are ordinary in this codebase's target projects, and a
        // column landing mid-character would panic in `shape_line`. `floor_boundary` is what
        // turns that into a hint drawn one character earlier.
        let mut text = String::from("$ação = 1;");
        let mut runs = vec![plain_run(text.len())];
        // Byte 3 is inside the `ç`.
        splice_hints(&mut text, &mut runs, &[hint_at(3, "?")]);
        assert!(text.contains('?'), "the hint still rendered");
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len(), "runs cover the text");
        // The real proof: every run boundary is a character boundary, which is what
        // `shape_line` requires.
        let mut at = 0usize;
        for run in &runs {
            assert!(text.is_char_boundary(at), "run boundary at {at} splits a character");
            at += run.len;
        }
    }

    #[test]
    fn no_hints_leaves_the_line_exactly_as_it_was() {
        // The idle path (#93): the common case must cost nothing and change nothing.
        let mut text = String::from("$a = 1;");
        let mut runs = vec![plain_run(7)];
        splice_hints(&mut text, &mut runs, &[]);
        assert_eq!(text, "$a = 1;");
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![7]);
    }

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
        super::line_runs(line, line_start, spans, &[], None, &[], None, &[], theme)
    }

    /// `line_runs` with search matches, spelled out.
    fn line_runs_matching(
        line: &str,
        line_start: usize,
        spans: &[HighlightSpan],
        matches: &[(Range<usize>, bool)],
        theme: &Theme,
    ) -> (String, Vec<(Range<usize>, GpuiHighlight)>) {
        super::line_runs(line, line_start, spans, &[], None, &[], None, matches, theme)
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
            &[],
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
        super::line_runs(line, line_start, &[], &[], None, &[], brackets, &[], theme)
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
        super::line_runs(line, line_start, spans, diagnostics, None, &[], None, &[], theme)
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
            &[],
            None,
            &[(0..3, false)],
            &theme,
        );
        let run = runs.iter().find(|(r, _)| *r == (0..3)).expect("the covered run exists");
        assert!(run.1.underline.is_some(), "the diagnostic underline survived the match");
        assert_eq!(run.1.background_color, Some(theme.search_match()));
    }

    #[test]
    fn a_selection_tints_exactly_its_own_bytes() {
        // #82's visibility fix: the old full-row tint made a word selection look like a
        // line selection, and on themes where hover == selected (one_dark_pro) ⌘D's
        // first press changed nothing on screen — reported as the feature being dead.
        let theme = Theme::dark();
        let (_, runs) = super::line_runs(
            "abcdef",
            0,
            &[],
            &[],
            None,
            std::slice::from_ref(&(1..4)),
            None,
            &[],
            &theme,
        );

        assert_eq!(backgrounds(&runs, theme.selection), vec![1..4], "the bytes, not the row");

        // A search match inside a selection keeps its own colour — the merge order is the
        // priority every editor gives the thing being searched for.
        let (_, runs) = super::line_runs(
            "abcdef",
            0,
            &[],
            &[],
            None,
            std::slice::from_ref(&(0..6)),
            None,
            &[(2..4, false)],
            &theme,
        );
        let match_run = runs.iter().find(|(range, _)| *range == (2..4)).expect("match run");
        assert_eq!(match_run.1.background_color, Some(theme.search_match()));
    }

    #[test]
    fn the_link_hint_underlines_straight_where_diagnostics_are_wavy() {
        // "Clickable" and "broken" share the underline channel and must not be confusable:
        // straight accent for the ⌘-hover hint, wavy severity colour for a squiggle. If
        // either assertion here starts failing the two have collapsed into one claim.
        let theme = Theme::dark();

        let (_, runs) = super::line_runs("abcdef", 0, &[], &[], Some(1..4), &[], None, &[], &theme);
        let link = runs.iter().find(|(r, _)| *r == (1..4)).expect("the hinted run exists");
        let underline = link.1.underline.expect("the hint underlines");
        assert!(!underline.wavy, "a link hint is straight — wavy claims breakage");
        assert_eq!(underline.color, Some(theme.accent));

        let (_, runs) = super::line_runs(
            "abcdef",
            0,
            &[],
            &[(1..4, Severity::Error)],
            None,
            &[],
            None,
            &[],
            &theme,
        );
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
            super::line_runs("f(1)", 0, &[], &[], None, &[], Some((1, 3)), &[(0..4, true)], &theme);

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
