//! The bottom terminal panel: a tab strip, a grid, and keyboard input.
//!
//! Thin by the same rule as `EditorView`: every terminal *semantic* (PTY lifecycle, ANSI
//! parsing, key encoding, teardown) lives in `elle-terminal` and is tested without a
//! window. This file measures the panel, forwards keys, and turns a [`GridSnapshot`] into
//! elements.
//!
//! The polling loop is the one thing here worth explaining. A PTY has no way to wake gpui,
//! so the panel repaints on a timer while it is open, and skips the repaint when the
//! session's generation counter has not moved. That is a deliberate trade against wiring a
//! channel from the reader thread into the window's event loop.

use std::time::Duration;

use elle_terminal::{
    Cell, CellColor, GridGeometry, GridSnapshot, Key, Modifiers, Selection, SelectionMode,
    SessionStatus, TermFlags, TerminalManager, encode, encode_paste,
};
use gpui::{
    App, ClipboardItem, Context, FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, SharedString, StyledText, Task,
    TextRun, Window, div, prelude::*, px,
};

use crate::actions::{Copy, NewTerminal, Paste, SelectAll, context};
use crate::editor::FONT_FAMILY;
use crate::theme::{Metrics, Theme, Themed};

/// How often the panel checks for new PTY output.
///
/// 16ms is one frame at 60Hz: fast enough that typing feels immediate, and the check
/// itself is an atomic read that costs nothing when there is no output.
/// ponytail: a poll, not a wake-up. The reader thread could notify the window directly
/// (gpui's `AsyncApp` + a channel), which would drop idle cost to zero; this is the
/// smaller change and the cost is only paid while the panel is open.
const POLL_INTERVAL: Duration = Duration::from_millis(16);

/// Character-cell width, as a fraction of the font size.
///
/// Menlo's advance width is 0.6 em. Hardcoded rather than measured because the grid is
/// laid out per row (one `StyledText` per line, not per cell), so this only decides how
/// many columns fit the panel width — being a fraction of a pixel out costs nothing,
/// where measuring per frame would cost a text-system round trip.
const CELL_WIDTH_RATIO: f32 = 0.6;

pub struct TerminalView {
    focus_handle: FocusHandle,
    manager: TerminalManager,
    /// Error from the most recent failed action, shown in the panel body.
    error: Option<SharedString>,
    /// The repaint timer. Held so dropping the view cancels it (ADR-0007).
    poll: Option<Task<()>>,
    /// Generation of the active session at the last repaint, to skip idle frames.
    last_generation: u64,
    /// Panel size in cells, from the last layout. Used to resize the PTY when it changes.
    grid_size: (u16, u16),
    /// The current selection, in buffer coordinates. `None` when nothing is selected.
    selection: Option<Selection>,
    /// True while the mouse button is down and a drag is extending the selection.
    dragging: bool,
    /// Window-relative origin of the grid, measured at layout.
    ///
    /// Mouse positions are window-relative, so mapping one to a cell needs where the grid
    /// actually starts — which is not a constant: the panel sits below the editor, to the
    /// right of the sidebar, and under its own tab strip, all of which move. Measured from
    /// the laid-out element rather than derived from the constants the workspace happens
    /// to use today. `None` until the first layout, which is before any click can arrive.
    grid_origin: Option<Point<Pixels>>,
}

impl TerminalView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            manager: TerminalManager::new(),
            error: None,
            poll: None,
            last_generation: 0,
            grid_size: (0, 0),
            selection: None,
            dragging: false,
            grid_origin: None,
        }
    }

    /// Points new sessions at the workspace root, so a terminal opens where the project is.
    pub fn set_cwd(&mut self, cwd: Option<std::path::PathBuf>) {
        self.manager.set_cwd(cwd);
    }

    /// How many sessions are open, for the status bar.
    pub fn session_count(&self) -> usize {
        self.manager.len()
    }

    /// Opens a session, starting the poll loop if this is the first one.
    ///
    /// `Session::spawn` forks a process, which is why this is not called from render. It is
    /// fast enough (measured ~8ms) to run on the main thread in response to a command
    /// rather than round-tripping through the background executor, and doing it inline
    /// keeps the error immediately reportable.
    /// ponytail: move to `cx.background_spawn` if spawn ever grows slow enough to drop a
    /// frame — the manager's API is already blocking and executor-agnostic for that.
    pub fn open_session(&mut self, cx: &mut Context<Self>) {
        match self.manager.open() {
            Ok(_) => {
                self.error = None;
                // Size the new session to the panel before its shell draws a prompt, so
                // the first line is not wrapped at the default 80 columns.
                let (rows, cols) = self.grid_size;
                if rows > 0 && cols > 0 {
                    self.manager.resize_all(rows, cols);
                }
                self.ensure_polling(cx);
            }
            // §24: a shell that will not start is a message in the panel, not a crash.
            Err(err) => self.error = Some(format!("could not start a terminal: {err:#}").into()),
        }
        cx.notify();
    }

    pub fn close_active(&mut self, cx: &mut Context<Self>) {
        self.manager.close_active();
        if self.manager.is_empty() {
            // Nothing to poll for; dropping the task stops the timer.
            self.poll = None;
        }
        cx.notify();
    }

    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        self.manager.activate(index);
        cx.notify();
    }

    /// Repaints while any session may be producing output.
    ///
    /// One timer for the whole panel rather than one per session: only the active session
    /// is visible, so a background session's output does not need a frame.
    fn ensure_polling(&mut self, cx: &mut Context<Self>) {
        if self.poll.is_some() {
            return;
        }

        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;

                // Ends the loop when the view is gone, which is what stops the timer from
                // outliving the window.
                let keep_going = this.update(cx, |this, cx| {
                    if this.manager.is_empty() {
                        return false;
                    }
                    let generation =
                        this.manager.active().map(|session| session.generation()).unwrap_or(0);
                    // Repaint only on new output. An idle terminal costs one atomic read
                    // per frame instead of a full grid copy and re-layout.
                    if generation != this.last_generation {
                        this.last_generation = generation;
                        cx.notify();
                    }
                    true
                });

                match keep_going {
                    Ok(true) => {}
                    _ => return,
                }
            }
        }));
    }

    /// Forwards a keypress to the active session.
    ///
    /// The translation from gpui's keystroke to `elle_terminal::Key` is the only terminal
    /// knowledge in this file, and it is deliberately a mapping rather than logic: the
    /// bytes each key produces are decided in the domain crate and tested there.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = &keystroke.modifiers;

        // cmd/fn chords are application keybindings, not terminal input. ctrl is *not*
        // excluded: ctrl-c is the single most important thing a terminal forwards.
        if modifiers.platform || modifiers.function {
            return;
        }

        let key = match keystroke.key.as_str() {
            "enter" => Key::Enter,
            "backspace" => Key::Backspace,
            "tab" => Key::Tab,
            "escape" => Key::Escape,
            "delete" => Key::Delete,
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" => Key::PageUp,
            "pagedown" => Key::PageDown,
            "space" => Key::Char(' '),
            _ => {
                // `key_char` is the literal character after the layout applies shift and
                // dead keys, and is None for command chords. `key` is the
                // layout-independent label — right for the matches above, wrong for text.
                //
                // With ctrl held, gpui reports key_char as the already-translated control
                // character, which `encode` would then translate a second time. So the
                // *label* is used for ctrl chords and key_char for everything else.
                let source = if modifiers.control {
                    keystroke.key.as_str()
                } else {
                    match keystroke.key_char.as_deref() {
                        Some(text) => text,
                        None => return,
                    }
                };

                let mut chars = source.chars();
                match (chars.next(), chars.next()) {
                    // Exactly one char: a key press. Anything longer is an IME commit or a
                    // named key this panel does not handle.
                    (Some(c), None) => Key::Char(c),
                    _ => {
                        // A multi-character commit (IME, paste-like input) is sent as-is.
                        if !modifiers.control && !source.is_empty() {
                            self.send_text(source, cx);
                        }
                        return;
                    }
                }
            }
        };

        // Read per press: a program turns application cursor mode on the moment it starts,
        // and a cached copy would send the wrong arrows for the first few presses.
        let flags = self.term_flags();
        // Typing dismisses the selection, as it does in every terminal: the text under it
        // is about to scroll and a highlight left behind would point at the wrong cells.
        // Before the encode check, so a chord that sends nothing (ctrl-1) still clears it.
        let had_selection = self.selection.take().is_some();

        let encoded =
            encode(&key, Modifiers { control: modifiers.control, alt: modifiers.alt }, flags);
        let Some(bytes) = encoded else {
            if had_selection {
                cx.notify();
            }
            return;
        };

        let Some(session) = self.manager.active_mut() else { return };
        // Typing while scrolled back should show what is being typed.
        session.scroll_to_bottom();

        if let Err(err) = session.write(&bytes) {
            // Writing to a shell that just exited is an ordinary race, not a bug.
            self.error = Some(format!("{err:#}").into());
        }
        self.last_generation = 0;
        cx.notify();
    }

    fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(session) = self.manager.active_mut() {
            session.scroll_to_bottom();
            if let Err(err) = session.write_str(text) {
                self.error = Some(format!("{err:#}").into());
            }
            cx.notify();
        }
    }

    /// The active session's DEC private modes, or the defaults when there is no session.
    fn term_flags(&self) -> TermFlags {
        self.manager.active().map(|session| session.flags()).unwrap_or_default()
    }

    // --- selection ------------------------------------------------------------------

    /// The geometry of the grid as it is currently displayed.
    ///
    /// Read fresh rather than cached, because `display_offset` moves under the view
    /// whenever output arrives or the wheel turns, and a stale `top_line` anchors the
    /// selection to the wrong text. Cheap enough to do per mouse event because it reads
    /// three integers rather than copying the grid — see `Session::geometry`.
    fn geometry(&self) -> Option<GridGeometry> {
        self.manager.active().map(|session| session.geometry())
    }

    /// Maps a window-relative mouse position onto a buffer cell.
    ///
    /// `None` before the grid has been laid out. Falling back to the window origin would
    /// silently map the click several rows off — better to ignore a click that arrives
    /// before the first frame than to anchor a selection to the wrong text.
    fn point_at(
        &self,
        position: Point<Pixels>,
        geometry: GridGeometry,
    ) -> Option<elle_terminal::SelectionPoint> {
        let origin = self.grid_origin?;
        Some(elle_terminal::cell_at(
            f32::from(position.x - origin.x),
            f32::from(position.y - origin.y),
            f32::from(Metrics::FONT_SIZE * CELL_WIDTH_RATIO),
            f32::from(Metrics::TERMINAL_LINE_HEIGHT),
            geometry,
        ))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A click in the terminal body focuses it, which is what makes the next keystroke
        // reach the shell rather than whatever had focus before.
        window.focus(&self.focus_handle);

        let Some(geometry) = self.geometry() else { return };
        let Some(point) = self.point_at(event.position, geometry) else { return };

        // gpui reports a running click count, so the mode falls straight out of it.
        let mode = match event.click_count {
            1 => SelectionMode::Char,
            2 => SelectionMode::Word,
            _ => SelectionMode::Line,
        };

        self.selection = Some(Selection::new(point, mode));
        self.dragging = true;
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.dragging {
            return;
        }
        let Some(geometry) = self.geometry() else { return };
        let Some(point) = self.point_at(event.position, geometry) else { return };

        if let Some(selection) = self.selection.as_mut()
            && selection.head != point
        {
            selection.extend_to(point);
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        self.dragging = false;
        // A click that selected nothing clears the highlight rather than leaving a
        // zero-width one behind.
        if self.selection.is_some_and(|selection| selection.is_empty()) {
            self.selection = None;
        }
        cx.notify();
    }

    /// The selected text, or empty when there is no selection.
    ///
    /// `pub` so a test can assert on what would land on the clipboard without a window —
    /// `cx.write_to_clipboard` needs one and the extraction is the part worth testing.
    pub fn selected_text(&self) -> String {
        let Some(selection) = self.selection.as_ref() else { return String::new() };
        let Some(session) = self.manager.active() else { return String::new() };
        // The full snapshot here, unlike the drag path: extraction needs the cells, and
        // this runs once per copy rather than once per mouse move.
        let snapshot = session.snapshot();
        let geometry = GridGeometry::of(&snapshot);
        elle_terminal::selected_text(selection, &snapshot, geometry)
    }

    /// ⌘C. Note this is *not* ⌃C: that stays SIGINT and goes through `on_key_down`, which
    /// is the entire reason macOS terminals put copy on the command key.
    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// ⌘V, bracketed if the shell asked for it.
    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        if text.is_empty() {
            return;
        }

        let flags = self.term_flags();
        let bytes = encode_paste(&text, flags);

        self.selection = None;
        let Some(session) = self.manager.active_mut() else { return };
        session.scroll_to_bottom();
        if let Err(err) = session.write(&bytes) {
            self.error = Some(format!("{err:#}").into());
        }
        self.last_generation = 0;
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.manager.active() else { return };
        let snapshot = session.snapshot();
        let geometry = GridGeometry::of(&snapshot);
        self.selection = elle_terminal::select_all(&snapshot, geometry);
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, window: &Window, cx: &mut Context<Self>) {
        let Some(session) = self.manager.active_mut() else { return };

        // Positive delta is a scroll up, which moves *back* into history.
        let lines = event.delta.pixel_delta(window.line_height()).y / window.line_height();
        let lines = lines.round() as i32;
        if lines != 0 {
            session.scroll(lines);
            cx.notify();
        }
    }

    /// Resizes every session to the panel's current cell dimensions.
    ///
    /// Called from render, where the pixel size is known, but only acts on a *change* — a
    /// resize syscall plus a grid reflow on every frame would be wasteful and would fight
    /// the shell's own redraw.
    fn sync_size(&mut self, width: gpui::Pixels, height: gpui::Pixels) {
        let cell_width = Metrics::FONT_SIZE * CELL_WIDTH_RATIO;
        let cols = (f32::from(width) / f32::from(cell_width)).floor().max(1.0) as u16;
        let rows =
            (f32::from(height) / f32::from(Metrics::TERMINAL_LINE_HEIGHT)).floor().max(1.0) as u16;

        if (rows, cols) != self.grid_size {
            self.grid_size = (rows, cols);
            self.manager.resize_all(rows, cols);
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .key_context(context::TERMINAL)
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(|this, _: &NewTerminal, _w, cx| this.open_session(cx)))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .on_scroll_wheel(cx.listener(|this, event, window, cx| {
                this.on_scroll(event, window, cx);
            }))
            .h(Metrics::TERMINAL_HEIGHT)
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme.background)
            .border_t_1()
            .border_color(theme.border)
            .child(self.render_tab_strip(&theme, cx))
            .child(self.render_body(&theme, cx))
    }
}

impl TerminalView {
    fn render_tab_strip(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let active = self.manager.active_index();
        let statuses = self.manager.statuses();

        div()
            .h(Metrics::TAB_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex().flex_1().items_center().gap_1().overflow_hidden().children(
                self.manager.sessions().iter().enumerate().map(|(index, session)| {
                    let entity = entity.clone();
                    let is_active = index == active;
                    // A dead session keeps its tab so its output stays readable; the
                    // marker is what tells the user it is no longer live.
                    let dead =
                        !statuses.get(index).map(|(_, status)| status.is_running()).unwrap_or(true);

                    div()
                        .id(("terminal-tab", index))
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .h(px(22.0))
                        .rounded_md()
                        .cursor_pointer()
                        .when(is_active, |el| el.bg(theme.selected).text_color(theme.text))
                        .when(!is_active, |el| el.text_color(theme.text_muted))
                        .hover(|el| el.bg(theme.hover))
                        .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                            entity.update(cx, |this, cx| this.activate(index, cx));
                        })
                        .child(SharedString::from(format!(
                            "{}{}",
                            if dead { "✗ " } else { "" },
                            session.title()
                        )))
                }),
            ))
            .child(self.render_strip_button("+", "terminal-new", theme, cx, |this, cx| {
                this.open_session(cx)
            }))
            .child(self.render_strip_button("✕", "terminal-close", theme, cx, |this, cx| {
                this.close_active(cx)
            }))
    }

    /// One of the small square buttons on the right of the tab strip.
    fn render_strip_button(
        &self,
        glyph: &'static str,
        id: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let entity = cx.entity();

        div()
            .id(id)
            .size(px(22.0))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .text_color(theme.text_muted)
            .hover(|el| el.bg(theme.hover).text_color(theme.text))
            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                entity.update(cx, |this, cx| action(this, cx));
            })
            .child(glyph)
    }

    fn render_body(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        // An error replaces the grid only when there is no session to show; a write error
        // on a live terminal belongs beside its output, not instead of it.
        if self.manager.is_empty() {
            let message = self
                .error
                .clone()
                .unwrap_or_else(|| SharedString::from("No terminal. Press ⌃` to open one."));
            let is_error = self.error.is_some();

            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(if is_error { theme.accent } else { theme.text_muted })
                .child(message)
                .into_any_element();
        }

        let status = self.manager.active().map(|session| session.status());
        let snapshot = self.manager.active().map(|session| session.snapshot());

        let Some(snapshot) = snapshot else {
            return div().flex_1().into_any_element();
        };

        let entity = cx.entity();
        let geometry = GridGeometry::of(&snapshot);
        let selection = self.selection;

        div()
            .id("terminal-body")
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden()
            .font_family(FONT_FAMILY)
            .text_size(Metrics::FONT_SIZE)
            // Down/move/up rather than a drag handler: a drag that leaves the panel must
            // keep extending the selection to the edge, and gpui only delivers moves
            // outside an element's bounds while a button is held.
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            // The button can come up outside the panel, which is the common case for a
            // drag that ran off the bottom; without this the view stays stuck in a drag.
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(
                // A canvas purely to learn the panel's pixel size, which is what decides
                // the PTY's rows and columns. gpui gives layout bounds to an element, not
                // to a render function, so measuring needs one. The grid's *origin* is
                // measured separately, by the grid itself — see `render_grid_measured`.
                gpui::canvas(
                    move |bounds, _window, cx| {
                        entity.update(cx, |this, _cx| {
                            this.sync_size(bounds.size.width, bounds.size.height);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(self.render_status_banner(status, theme))
            .child(self.render_grid_measured(&snapshot, geometry, selection.as_ref(), theme, cx))
            .into_any_element()
    }

    /// The grid, with a canvas behind it that records where it actually starts.
    ///
    /// Not the panel's origin: a status banner pushes the first row down, and using the
    /// body's origin would put every click one row too low the moment a shell exits. The
    /// grid measures itself instead, so the offset stays right whatever is above it.
    fn render_grid_measured(
        &self,
        snapshot: &GridSnapshot,
        geometry: GridGeometry,
        selection: Option<&Selection>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();

        div()
            .relative()
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                gpui::canvas(
                    move |bounds, _window, cx| {
                        entity.update(cx, |this, _cx| this.grid_origin = Some(bounds.origin));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(render_grid(snapshot, geometry, selection, theme))
    }

    /// A one-line banner for a session that has exited or failed.
    fn render_status_banner(
        &self,
        status: Option<SessionStatus>,
        theme: &Theme,
    ) -> Option<impl IntoElement> {
        let message = match status {
            Some(SessionStatus::Exited { code }) => match code {
                Some(code) => {
                    format!("Process exited with code {code}. Press + for a new terminal.")
                }
                None => "Process exited. Press + for a new terminal.".to_string(),
            },
            Some(SessionStatus::Failed(err)) => format!("Terminal failed: {err}"),
            // A write error on a live session shows here too, without hiding the output.
            _ => return self.error.clone().map(|error| banner(error, theme)),
        };

        Some(banner(SharedString::from(message), theme))
    }
}

fn banner(message: SharedString, theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .px_2()
        .py_1()
        .bg(theme.panel)
        .text_color(theme.accent)
        .text_size(Metrics::UI_FONT_SIZE)
        .child(message)
}

/// Renders the grid, one `StyledText` per row.
///
/// Per row rather than per cell: a 24x80 grid is 1920 cells, and an element per cell is
/// ~2000 elements to lay out every frame. One text element per row with colour runs is 24,
/// and gpui shapes each row once. The cost is that a per-cell *background* needs its own
/// pass — which is also how the selection highlight is drawn: as a background colour on
/// the runs, not as a second layer of rectangles over the text.
fn render_grid(
    snapshot: &GridSnapshot,
    geometry: GridGeometry,
    selection: Option<&Selection>,
    theme: &Theme,
) -> impl IntoElement {
    div().flex_1().flex().flex_col().overflow_hidden().children(
        snapshot.lines.iter().enumerate().map(|(index, line)| {
            let cursor_column =
                snapshot.cursor.filter(|cursor| cursor.line == index).map(|cursor| cursor.column);

            // Computed here rather than per cell inside the run loop: one range lookup per
            // row instead of one per cell, which is what keeps this off the frame budget
            // (§21) on a grid that is mostly not selected.
            let selected = selection
                .and_then(|selection| {
                    elle_terminal::selected_columns(selection, index, geometry, snapshot)
                })
                .unwrap_or(0..0);

            div().h(Metrics::TERMINAL_LINE_HEIGHT).flex_none().child(styled_row(
                line,
                cursor_column,
                selected,
                theme,
            ))
        }),
    )
}

/// Builds one row as a single text element with colour and attribute runs.
///
/// The work happens in [`row_runs`], which returns plain data so it can be unit-tested:
/// `StyledText`'s own text and runs are private, and `with_runs` *panics* unless the run
/// lengths sum to exactly the text length — an invariant worth a test rather than a
/// crash discovered by scrolling.
fn styled_row(
    cells: &[Cell],
    cursor_column: Option<usize>,
    selected: std::ops::Range<usize>,
    theme: &Theme,
) -> StyledText {
    let (text, runs) = row_runs(cells, cursor_column, selected, theme);
    StyledText::new(SharedString::from(text)).with_runs(runs)
}

/// The text and colour runs for one row.
fn row_runs(
    cells: &[Cell],
    cursor_column: Option<usize>,
    selected: std::ops::Range<usize>,
    theme: &Theme,
) -> (String, Vec<TextRun>) {
    // A cursor past the last column (end of line) has no cell to invert, so the row is
    // padded with one blank. Doing it up front keeps the run loop single-path.
    let pad = cursor_column.is_some_and(|column| column >= cells.len());

    let mut text = String::with_capacity(cells.len() + 1);
    let mut runs: Vec<TextRun> = Vec::new();

    for (column, cell) in cells.iter().enumerate() {
        // The trailing half of a wide glyph is already covered by the glyph itself;
        // emitting anything here would shift the rest of the row right by one column.
        if cell.wide_spacer {
            continue;
        }

        let is_cursor = cursor_column == Some(column);
        // The cursor is drawn by inverting its cell rather than as a positioned caret:
        // no measuring, and it stays correct even on a font fallback.
        //
        // The selection is a background colour, and it wins over the cell's own — a
        // selected cell with a red background must still read as selected. It does *not*
        // win over the cursor: losing track of the cursor while dragging is worse. The
        // colour comes from the theme (`selection`), which all five variants define.
        let (foreground, background) = if is_cursor {
            (theme.background, Some(theme.cursor))
        } else if selected.contains(&column) {
            (theme.terminal(cell.fg), Some(theme.selection))
        } else {
            (theme.terminal(cell.fg), background_for(cell, theme))
        };

        let c = if cell.c == '\0' { ' ' } else { cell.c };
        text.push(c);

        let run = TextRun {
            len: c.len_utf8(),
            font: gpui::Font {
                family: FONT_FAMILY.into(),
                features: Default::default(),
                fallbacks: None,
                weight: if cell.bold { gpui::FontWeight::BOLD } else { gpui::FontWeight::NORMAL },
                style: if cell.italic { gpui::FontStyle::Italic } else { gpui::FontStyle::Normal },
            },
            color: foreground,
            background_color: background,
            underline: cell.underline.then(gpui::UnderlineStyle::default),
            strikethrough: None,
        };

        // Merging adjacent identical runs keeps the run count near the number of colour
        // changes rather than the number of characters, which is what makes shaping cheap
        // on a mostly-monochrome screen.
        match runs.last_mut() {
            Some(last) if runs_mergeable(last, &run) => last.len += run.len,
            _ => runs.push(run),
        }
    }

    if pad {
        text.push(' ');
        runs.push(TextRun {
            len: 1,
            font: gpui::font(FONT_FAMILY),
            color: theme.background,
            background_color: Some(theme.cursor),
            underline: None,
            strikethrough: None,
        });
    }

    (text, runs)
}

/// The default background is the panel's own, and painting it per cell would be thousands
/// of redundant rectangles — so only a *non-default* background becomes a run colour.
fn background_for(cell: &Cell, theme: &Theme) -> Option<gpui::Hsla> {
    match cell.bg {
        CellColor::Background => None,
        other => Some(theme.terminal(other)),
    }
}

fn runs_mergeable(a: &TextRun, b: &TextRun) -> bool {
    a.color == b.color
        && a.background_color == b.background_color
        && a.font == b.font
        && a.underline == b.underline
        && a.strikethrough == b.strikethrough
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(c: char) -> Cell {
        Cell { c, ..Cell::default() }
    }

    /// `StyledText::with_runs` panics unless the run lengths sum to the text's byte
    /// length. Asserting it here turns that from a runtime crash into a test failure.
    fn assert_runs_cover_text(text: &str, runs: &[TextRun]) {
        let total: usize = runs.iter().map(|run| run.len).sum();
        assert_eq!(total, text.len(), "run lengths must sum to the text byte length");
    }

    #[test]
    fn a_row_merges_adjacent_cells_with_identical_styling() {
        let theme = Theme::dark();
        let cells: Vec<Cell> = "hello".chars().map(cell).collect();
        let (text, runs) = row_runs(&cells, None, 0..0, &theme);

        // Five identically-styled cells must shape as one run, not five.
        assert_eq!(runs.len(), 1, "uniform text should collapse to one run");
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn a_colour_change_starts_a_new_run() {
        let theme = Theme::dark();
        let mut cells: Vec<Cell> = "ab".chars().map(cell).collect();
        cells[1].fg = CellColor::Ansi(1);

        let (text, runs) = row_runs(&cells, None, 0..0, &theme);
        assert_eq!(runs.len(), 2);
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn wide_spacers_are_skipped_so_columns_do_not_shift() {
        let theme = Theme::dark();
        let cells = vec![
            Cell { c: '漢', ..Cell::default() },
            Cell { wide_spacer: true, ..Cell::default() },
            cell('x'),
        ];

        // The spacer must contribute no character, or every later column moves right.
        let (text, runs) = row_runs(&cells, None, 0..0, &theme);
        assert_eq!(text, "漢x");
        // The multibyte case is where a run length in *chars* rather than bytes would
        // panic inside gpui.
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn a_cursor_past_the_last_column_still_renders() {
        let theme = Theme::dark();
        let cells: Vec<Cell> = "ab".chars().map(cell).collect();

        // End-of-line cursor: there is no cell to invert, so a blank is appended.
        let (text, runs) = row_runs(&cells, Some(2), 0..0, &theme);
        assert_eq!(text, "ab ");
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn the_cursor_cell_is_inverted() {
        let theme = Theme::dark();
        let cells: Vec<Cell> = "ab".chars().map(cell).collect();
        let (_, runs) = row_runs(&cells, Some(0), 0..0, &theme);

        // The cursor is drawn by inverting its own cell, so the first run carries the
        // cursor colour as a background.
        assert_eq!(runs[0].background_color, Some(theme.cursor));
        assert_eq!(runs[0].color, theme.background);
    }

    #[test]
    fn a_row_of_multibyte_text_produces_byte_accurate_runs() {
        let theme = Theme::dark();
        // Portuguese and CJK together: the exact case where char/byte confusion panics.
        let cells: Vec<Cell> = "ação日本".chars().map(cell).collect();
        let (text, runs) = row_runs(&cells, None, 0..0, &theme);
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn the_default_background_is_not_painted_per_cell() {
        let theme = Theme::dark();
        assert_eq!(background_for(&cell('x'), &theme), None);

        let coloured = Cell { bg: CellColor::Ansi(4), ..Cell::default() };
        assert_eq!(background_for(&coloured, &theme), Some(theme.ansi[4]));
    }

    #[test]
    fn nul_cells_render_as_spaces() {
        let theme = Theme::dark();
        // A NUL would otherwise reach the text system as an unprintable glyph.
        let (text, runs) = row_runs(&[cell('\0')], None, 0..0, &theme);
        assert_eq!(text, " ");
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn selected_cells_take_the_themes_selection_colour() {
        let theme = Theme::dark();
        let cells: Vec<Cell> = "abcd".chars().map(cell).collect();
        let (text, runs) = row_runs(&cells, None, 1..3, &theme);

        assert_runs_cover_text(&text, &runs);
        // Unselected, selected, unselected: three runs, and the middle one is the theme's
        // own selection colour rather than a hardcoded highlight that a light theme would
        // render invisible.
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].background_color, None);
        assert_eq!(runs[1].background_color, Some(theme.selection));
        assert_eq!(runs[1].len, 2);
        assert_eq!(runs[2].background_color, None);
    }

    #[test]
    fn every_theme_gives_the_selection_a_colour_distinct_from_its_background() {
        // A hardcoded highlight is invisible in at least one theme; this is the assertion
        // that keeps the selection visible in all five.
        for theme in [
            Theme::dark(),
            Theme::light(),
            Theme::one_dark_pro(),
            Theme::github_dark(),
            Theme::github_light(),
        ] {
            assert_ne!(
                theme.selection, theme.background,
                "a selection the same colour as the background cannot be seen"
            );
        }
    }

    #[test]
    fn the_cursor_wins_over_the_selection() {
        let theme = Theme::dark();
        let cells: Vec<Cell> = "ab".chars().map(cell).collect();
        // Dragging across the cursor must not hide it — losing the caret mid-drag is
        // worse than a cell that reads as unselected.
        let (_, runs) = row_runs(&cells, Some(0), 0..2, &theme);
        assert_eq!(runs[0].background_color, Some(theme.cursor));
    }

    #[test]
    fn a_selection_over_multibyte_text_keeps_the_runs_byte_accurate() {
        let theme = Theme::dark();
        // Selecting from the middle of a multibyte run is where a run length counted in
        // chars rather than bytes panics inside gpui.
        let cells: Vec<Cell> = "ação".chars().map(cell).collect();
        let (text, runs) = row_runs(&cells, None, 1..3, &theme);
        assert_runs_cover_text(&text, &runs);
        assert_eq!(text, "ação");
    }

    #[test]
    fn out_of_range_ansi_slots_fall_back_instead_of_panicking() {
        let theme = Theme::dark();
        // The slot arrives as a u8 off the wire; 200 is not a valid ANSI slot.
        assert_eq!(theme.terminal(CellColor::Ansi(200)), theme.text);
    }
}
