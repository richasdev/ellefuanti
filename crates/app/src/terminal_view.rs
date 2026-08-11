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
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, SharedString, Task, TextRun,
    Window, div, prelude::*, px,
};

use crate::actions::{CloseTerminal, Copy, NewTerminal, Paste, SelectAll, SplitTerminal, context};
use crate::fonts::Fonts;
use crate::theme::{Metrics, Theme, Themed};

/// How often the panel checks for new PTY output.
///
/// 16ms is one frame at 60Hz: fast enough that typing feels immediate, and the check
/// itself is an atomic read that costs nothing when there is no output.
/// ponytail: a poll, not a wake-up. The reader thread could notify the window directly
/// (gpui's `AsyncApp` + a channel), which would drop idle cost to zero; this is the
/// smaller change and the cost is only paid while the panel is open.
const POLL_INTERVAL: Duration = Duration::from_millis(16);

// The cell width and row height that used to be `CELL_WIDTH_RATIO` here and
// `Metrics::TERMINAL_LINE_HEIGHT` in `theme.rs` now come from `Fonts::cell_size` (#49).
// Both derive from the editor font size, so ⌘+ scales the terminal too — the flat 16px row
// was why a zoomed terminal overlapped its own rows. One function returning both is what
// keeps the three consumers below (layout, PTY resize, selection hit-testing) agreeing.

/// What the terminal asks the workspace to do on its behalf.
pub enum TerminalViewEvent {
    /// A ⌘-clicked path, exactly as it appeared in the output. May be relative and may
    /// not exist — the receiver resolves against the project and declines what is not
    /// there, because only it can check without guessing.
    OpenPath { path: std::path::PathBuf, line: Option<u32> },
}

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
    /// The session in the pane that does *not* have focus, when the panel is split.
    ///
    /// One extra id rather than a pane tree: this splits in two, side by side, which is
    /// what was asked for. A general layout would be a `Vec` of panes plus a focused index
    /// plus an orientation, and none of that has a caller yet.
    ///
    /// Holding the *inactive* pane is what keeps typing unchanged: keys go to
    /// `manager.active()` exactly as they did before splits existed, and clicking the other
    /// pane activates it, which swaps the two ids without moving either grid. Pane order on
    /// screen comes from the session order in the manager, so the halves do not jump when
    /// focus moves.
    split: Option<elle_terminal::SessionId>,
    /// The close prompt's task. Held so dropping the view cancels it (ADR-0007), and so a
    /// second ⌘W replaces the first dialog rather than stacking two over one terminal.
    close_prompt: Option<Task<()>>,
    /// How many repaint timers have been spawned, for the test that #97's split does not
    /// add one. Counted rather than inferred from `poll`, which being an `Option` cannot
    /// distinguish "reused the timer" from "replaced it with a new one".
    #[cfg(test)]
    polls_started: usize,
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
            split: None,
            close_prompt: None,
            #[cfg(test)]
            polls_started: 0,
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

    /// Drives ⌘D through the same handler the keybinding fires, for tests.
    ///
    /// The action rather than setting `split` directly: a test that assigned the field
    /// would pass while the real command was broken — the same reason
    /// `toggle_terminal_for_test` exists on the workspace.
    #[cfg(test)]
    pub fn split_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.split_terminal(&SplitTerminal, window, cx);
    }

    /// Whether the repaint timer is running.
    #[cfg(test)]
    pub fn is_polling_for_test(&self) -> bool {
        self.poll.is_some()
    }

    /// How many timer tasks have been spawned over this view's life.
    ///
    /// A *count*, not `poll.is_some()`. The field is an `Option`, so asking whether it is
    /// set can only ever answer "one" and a test built on it passes even if `ensure_polling`
    /// spawns a fresh timer on every call — verified by making that mutation and watching
    /// the `is_some` version stay green. The cost this guards is the spawn itself, so the
    /// spawn is what gets counted.
    #[cfg(test)]
    pub fn polls_started_for_test(&self) -> usize {
        self.polls_started
    }

    /// Opens a session running a named program, so a test does not depend on the login
    /// shell. `cat` blocks on stdin: alive, silent, and it never draws a prompt.
    #[cfg(test)]
    pub fn open_shell_for_test(&mut self, shell: &str, cx: &mut Context<Self>) {
        if self.manager.open_with_shell(Some(shell)).is_ok() {
            self.ensure_polling(cx);
        }
        cx.notify();
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
        self.after_close(cx);
    }

    /// Closes a session by id, whatever index it currently sits at.
    ///
    /// By id rather than index because the only callers that need this are asynchronous —
    /// the confirm prompt below — and a tab opened or closed while the dialog was up would
    /// make an index point at a different shell. Closing the wrong terminal is exactly the
    /// data loss the prompt exists to prevent.
    fn close_id(&mut self, id: elle_terminal::SessionId, cx: &mut Context<Self>) {
        self.manager.close(id);
        self.after_close(cx);
    }

    /// Shared tail of every close: stop the timer once nothing is left to poll.
    fn after_close(&mut self, cx: &mut Context<Self>) {
        if self.manager.is_empty() {
            // Nothing to poll for; dropping the task stops the timer.
            self.poll = None;
        }
        // A split showing a session that just went away falls back to the active one.
        self.sync_split();
        cx.notify();
    }

    /// ⌘W, and the ✕ on the tab strip. Asks first when the shell is still running.
    ///
    /// The dirty-tab prompt (#40) is the model, for the same reason: killing a shell
    /// halfway through `php artisan migrate` costs work that cannot be recovered, and this
    /// is the one place in the panel where a mis-click is expensive. A shell that has
    /// already exited has nothing to lose, so its tab closes without a question — a prompt
    /// there would train the user to dismiss the one that matters.
    ///
    /// This deliberately does not try to name the running command. The PTY carries output,
    /// not a process table, so knowing that `migrate` in particular is running would mean
    /// walking the child's descendants — a lot of platform-specific machinery to make the
    /// message one word better.
    fn close_with_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.manager.active() else { return };

        if !session.status().is_running() {
            self.close_active(cx);
            return;
        }

        let id = session.id();
        let title = session.title().to_string();

        // Button order matches #40: index 0 is macOS's default, so Cancel sits there and a
        // stray Return cannot be the keystroke that kills a running process.
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("{title} is still running."),
            Some("Closing this terminal will end it and anything it is running."),
            &["Cancel", "Close Terminal"],
            cx,
        );

        self.close_prompt = Some(cx.spawn(async move |this, cx| {
            // A dropped receiver means the dialog went away without an answer; treat that
            // as Cancel, because the safe default is to keep the shell alive.
            let Ok(choice) = answer.await else { return };
            if choice != 1 {
                return;
            }
            this.update(cx, |this, cx| this.close_id(id, cx)).ok();
        }));
    }

    fn close_terminal(&mut self, _: &CloseTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.close_with_confirm(window, cx);
    }

    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        // Selecting a tab while split replaces the pane that had focus, leaving the other
        // where it is — the same rule as clicking the other half, so a tab click never
        // silently changes what the *unfocused* pane is showing.
        self.manager.activate(index);
        self.sync_split();
        cx.notify();
    }

    /// Focuses the pane showing `id`. The click target on the unfocused half of a split.
    fn activate_id(
        &mut self,
        id: elle_terminal::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The panel keeps one focus handle for both panes: which pane is "focused" is the
        // manager's active session, not a second gpui focus target. Focusing the panel is
        // still needed for the click to make keys reach the shell at all.
        window.focus(&self.focus_handle);

        // The pane losing focus keeps its grid, so it becomes the split.
        let previous = self.manager.active().map(|session| session.id());
        self.manager.activate_id(id);
        if let Some(previous) = previous
            && previous != id
            && self.split.is_some()
        {
            self.split = Some(previous);
        }
        // A selection belongs to the pane it was dragged in; carrying it across would
        // highlight unrelated text at the same coordinates in the other grid.
        self.selection = None;
        self.sync_split();
        cx.notify();
    }

    /// Repaints while any session may be producing output.
    ///
    /// One timer for the whole panel rather than one per session — and still one after a
    /// split (#97). Splitting makes a second session *visible*, not a second thing to
    /// schedule: what changes is that the generation check below sums both panes instead of
    /// reading the active one. Two timers would double the idle wakeups the perf gate
    /// measures (#93) to buy nothing, since one frame repaints the whole panel anyway.
    /// A session that is neither pane is still not polled: its output needs no frame.
    fn ensure_polling(&mut self, cx: &mut Context<Self>) {
        if self.poll.is_some() {
            return;
        }

        #[cfg(test)]
        {
            self.polls_started += 1;
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
                    let generation = this.visible_generation();
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

    // --- split ----------------------------------------------------------------------

    /// The id shown in the right pane, if that session still exists.
    ///
    /// Read through this rather than off the field: the session may have been closed since
    /// the split was made, and a stale id would render an empty pane forever.
    fn split_id(&self) -> Option<elle_terminal::SessionId> {
        let split = self.split?;
        let active = self.manager.active()?.id();
        // A split pointing at the active session is not a split — it would show the same
        // shell twice, which is *mirroring*, and mirroring is deliberately not built here
        // (#97): two views on one PTY needs one grid feeding two scrollbacks and two
        // selections, which is a different problem from laying out two grids.
        (split != active && self.manager.sessions().iter().any(|s| s.id() == split))
            .then_some(split)
    }

    pub fn is_split(&self) -> bool {
        self.split_id().is_some()
    }

    /// Drops a split whose session has gone away, so a closed pane does not linger.
    fn sync_split(&mut self) {
        if self.split_id().is_none() {
            self.split = None;
        }
    }

    /// The combined generation of everything on screen.
    ///
    /// A sum rather than the active session's counter alone: with a split, output in either
    /// pane has to trigger the repaint, and a sum changes whenever either side does. It can
    /// only collide if two sessions moved by offsetting amounts in the same 16ms tick, which
    /// would cost one late frame and be corrected by the next byte either shell writes.
    fn visible_generation(&self) -> u64 {
        let active = self.manager.active().map(|session| session.generation()).unwrap_or(0);
        let split = self
            .split_id()
            .and_then(|id| self.manager.sessions().iter().find(|s| s.id() == id))
            .map(|session| session.generation())
            .unwrap_or(0);
        active.wrapping_add(split)
    }

    /// ⌘D. Opens a second session beside this one, or closes the split if there is one.
    ///
    /// A toggle rather than a separate unsplit command, because there are only two states
    /// and the second binding would have nothing else to do.
    fn split_terminal(&mut self, _: &SplitTerminal, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_split() {
            self.split = None;
            // The panes just doubled in width, so the PTYs are a resize behind. Clearing
            // the recorded size forces the next layout to re-tell both — see `sync_size`,
            // which only acts on a change.
            self.grid_size = (0, 0);
            cx.notify();
            return;
        }

        // The session that is active *now* stays on the left, and the new one becomes
        // active on the right — matching every other terminal, where the split you just
        // made is the one you type into.
        let left = self.manager.active().map(|session| session.id());
        self.open_session(cx);

        match (left, self.manager.active().map(|session| session.id())) {
            // The left pane is the one that was already there; `open_session` made the new
            // session active, so it renders on the right.
            (Some(left), Some(new)) if left != new => self.split = Some(left),
            // Nothing was open, or the spawn failed and `open_session` already reported it.
            // Either way one pane is the honest result.
            _ => self.split = None,
        }
        self.grid_size = (0, 0);
        cx.notify();
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
    /// The cell size has to be the one the grid was *drawn* with, or a click lands on the
    /// wrong row — which is why it comes from [`Fonts::cell_size`] here, in `sync_size` and
    /// in `render_grid` rather than from three copies of the same arithmetic.
    fn point_at(
        &self,
        position: Point<Pixels>,
        geometry: GridGeometry,
        cell: (Pixels, Pixels),
    ) -> Option<elle_terminal::SelectionPoint> {
        let origin = self.grid_origin?;
        Some(elle_terminal::cell_at(
            f32::from(position.x - origin.x),
            f32::from(position.y - origin.y),
            f32::from(cell.0),
            f32::from(cell.1),
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

        let cell = Fonts::get(cx).cell_size();
        let Some(geometry) = self.geometry() else { return };
        let Some(point) = self.point_at(event.position, geometry, cell) else { return };

        // ⌘-click follows a link instead of selecting (#70): a stack trace names files as
        // `app/User.php:42`, and jumping to them is the reason a terminal lives inside an
        // editor at all. Falls through to selection when nothing link-shaped is under the
        // pointer, so a ⌘-click on plain text is a plain click rather than a dead one.
        if event.modifiers.platform && self.follow_link_at(point, geometry, cx) {
            return;
        }

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
        let cell = Fonts::get(cx).cell_size();
        let Some(geometry) = self.geometry() else { return };
        let Some(point) = self.point_at(event.position, geometry, cell) else { return };

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

    /// Follows whatever link sits under `point`. True if the click was consumed.
    ///
    /// URLs open here — the browser needs no knowledge the view lacks. Paths are *emitted*,
    /// not opened: the view sees one line of grid text and cannot honestly resolve a
    /// relative `app/User.php` or check it exists — the workspace holds the project root
    /// and the open machinery, so it gets the claim and decides (RISKS.md #4: the view
    /// never asserts "this file exists", it reports "the user clicked something shaped
    /// like this").
    fn follow_link_at(
        &mut self,
        point: elle_terminal::SelectionPoint,
        geometry: GridGeometry,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.manager.active() else { return false };
        let snapshot = session.snapshot();
        let Some(row) = geometry.row_of(point.line) else { return false };
        let Some(cells) = snapshot.lines.get(row) else { return false };
        let text: String = cells.iter().map(|cell| cell.c).collect();

        match elle_terminal::link_at(&text, point.column) {
            Some(elle_terminal::Link::Url(url)) => {
                cx.open_url(&url);
                true
            }
            Some(elle_terminal::Link::Path { path, line }) => {
                cx.emit(TerminalViewEvent::OpenPath { path: path.into(), line });
                true
            }
            None => false,
        }
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
    /// `cell` must be the size the grid is actually drawn at. Telling the PTY it has more
    /// rows than are rendered means the shell writes to lines nobody sees and its output
    /// garbles — a worse failure than misalignment, and the reason this takes the cell size
    /// from the same [`Fonts::cell_size`] the renderer uses rather than recomputing it.
    ///
    /// `width` is the width of *one pane*, not of the panel: split in two, each shell has
    /// half the columns, and telling both they have the full width is #92 again — the shell
    /// wraps at a column that is not where the pane ends. The caller divides, because it is
    /// the caller that knows how the panel was laid out.
    fn sync_size(&mut self, width: gpui::Pixels, height: gpui::Pixels, cell: (Pixels, Pixels)) {
        // Minus the one cell `render_grid` pads the left with. The shell is told what it can
        // actually draw into, not what the panel measures — the difference is #92's bug in
        // the other direction.
        let usable = f32::from(width) - f32::from(cell.0);
        let cols = (usable / f32::from(cell.0)).floor().max(1.0) as u16;
        let rows = (f32::from(height) / f32::from(cell.1)).floor().max(1.0) as u16;

        if (rows, cols) != self.grid_size {
            self.grid_size = (rows, cols);
            // Both panes are the same size, so one call still covers them — and a session
            // that is off screen is sized for the pane it will appear in.
            self.manager.resize_all(rows, cols);
        }
    }
}

impl gpui::EventEmitter<TerminalViewEvent> for TerminalView {}

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
            .on_action(cx.listener(Self::close_terminal))
            .on_action(cx.listener(Self::split_terminal))
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
            .child(self.render_strip_button("+", "terminal-new", theme, cx, |this, _window, cx| {
                this.open_session(cx)
            }))
            // "⫿" reads as two panes side by side. ⌘D does the same thing.
            .child(self.render_strip_button(
                "⫿",
                "terminal-split",
                theme,
                cx,
                |this, window, cx| this.split_terminal(&SplitTerminal, window, cx),
            ))
            // The ✕ now asks before killing a running shell, exactly as ⌘W does — the
            // mouse path was the one that could destroy work without a question.
            .child(self.render_strip_button(
                "✕",
                "terminal-close",
                theme,
                cx,
                |this, window, cx| this.close_with_confirm(window, cx),
            ))
    }

    /// One of the small square buttons on the right of the tab strip.
    ///
    /// The action takes a `Window` because closing needs one to raise the confirm prompt.
    fn render_strip_button(
        &self,
        glyph: &'static str,
        id: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
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
            .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                entity.update(cx, |this, cx| action(this, window, cx));
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

        let Some(active_id) = self.manager.active().map(|session| session.id()) else {
            return div().flex_1().into_any_element();
        };

        let fonts = Fonts::get(cx);
        let cell = fonts.cell_size();
        let panes = self.pane_ids(active_id);
        let pane_count = panes.len();
        let entity = cx.entity();

        // A loop rather than `map().collect()`: rendering a pane reborrows `cx` mutably,
        // which a closure cannot do across iterations.
        let mut rendered = Vec::with_capacity(pane_count);
        for &id in &panes {
            rendered.push(self.render_pane(id, id == active_id, theme, &fonts, cx));
        }

        div()
            .id("terminal-body")
            .flex_1()
            .flex()
            // Row, not column: the panes sit side by side. Vertical splits would be the
            // same layout with `flex_col` and a height divisor in `sync_size`, and are left
            // out because nothing asked for them (#97).
            .flex_row()
            .overflow_hidden()
            .font_family(fonts.family.clone())
            .text_size(fonts.size)
            // Must match the cell height each row is given, and gpui's default does not: it
            // lays text out at roughly 1.618 em while a terminal row is `cell.1` — 16/13 em.
            // At 13px that is 21.03px of text in a 16px cell, so every line overflows by
            // 5.03px and the drift reaches a whole row in three lines. That is what made
            // typed output overlap the prompt above it. Worse here than in the editor
            // (#106), because the terminal's cell is deliberately tighter than a text row.
            .line_height(cell.1)
            .child(
                // A canvas purely to learn the panel's pixel size, which is what decides
                // the PTY's rows and columns. gpui gives layout bounds to an element, not
                // to a render function, so measuring needs one. The grid's *origin* is
                // measured separately, by the grid itself — see `render_grid_measured`.
                //
                // Measured once for the whole body and divided, rather than once per pane:
                // both panes are `flex_1` of the same row, so they are the same width by
                // construction, and one canvas cannot disagree with itself about the row
                // count the way two racing ones could.
                gpui::canvas(
                    move |bounds, _window, cx| {
                        entity.update(cx, |this, _cx| {
                            let width = bounds.size.width / pane_count as f32;
                            this.sync_size(width, bounds.size.height, cell);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(rendered)
            .into_any_element()
    }

    /// The sessions on screen, left to right.
    ///
    /// Ordered by their position in the manager rather than by which is active, so
    /// activating the right pane does not make the two grids swap places under the cursor.
    fn pane_ids(&self, active_id: elle_terminal::SessionId) -> Vec<elle_terminal::SessionId> {
        let Some(split) = self.split_id() else { return vec![active_id] };

        let mut ids: Vec<_> = self
            .manager
            .sessions()
            .iter()
            .map(|session| session.id())
            .filter(|id| *id == active_id || *id == split)
            .collect();
        ids.sort_by_key(|id| self.manager.sessions().iter().position(|s| s.id() == *id));
        ids
    }

    /// One pane: its status banner and its grid.
    ///
    /// Only the focused pane carries the selection and the mouse handlers. A selection per
    /// pane would need a selection *and* a grid origin per pane, and the payoff — dragging
    /// in the unfocused half without focusing it first — is not one terminals offer anyway:
    /// clicking a pane focuses it, and then the drag works as it always has.
    fn render_pane(
        &self,
        id: elle_terminal::SessionId,
        is_active: bool,
        theme: &Theme,
        fonts: &Fonts,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let split = self.is_split();
        let session = self.manager.sessions().iter().find(|session| session.id() == id);
        let Some(session) = session else { return div().flex_1().into_any_element() };

        let snapshot = session.snapshot();
        let geometry = GridGeometry::of(&snapshot);
        let status = session.status();
        let entity = cx.entity();

        div()
            .id(("terminal-pane", id.0 as usize))
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden()
            // The divider between the halves, and the indicator for which one has focus.
            // A left border on the second pane rather than a separate element: one line,
            // and it cannot end up laid out in the wrong place.
            .when(split, |el| {
                el.border_l_1().border_color(if is_active { theme.accent } else { theme.border })
            })
            .when(split && is_active, |el| el.bg(theme.panel))
            // Down/move/up rather than a drag handler: a drag that leaves the panel must
            // keep extending the selection to the edge, and gpui only delivers moves
            // outside an element's bounds while a button is held.
            .when(is_active, |el| {
                el.on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    // The button can come up outside the panel, which is the common case
                    // for a drag that ran off the bottom; without this the view stays
                    // stuck in a drag.
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            })
            // Clicking the other half focuses it, which is how a split is driven from the
            // mouse. It does not also start a selection: the first click chooses the pane.
            .when(!is_active, |el| {
                el.cursor_pointer().on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                    entity.update(cx, |this, cx| this.activate_id(id, window, cx));
                })
            })
            .children(self.render_pane_label(id, is_active, theme, fonts.ui_size))
            .children(self.render_status_banner(Some(status), theme, fonts.ui_size))
            .child(self.render_grid_measured(
                &snapshot,
                geometry,
                // The unfocused pane draws no highlight: the selection belongs to the
                // focused one, and painting it on both would show a selection in a grid
                // whose text is at different coordinates.
                is_active.then_some(self.selection).flatten().as_ref(),
                theme,
                fonts,
                cx,
            ))
            .into_any_element()
    }

    /// The per-pane title, shown only while split.
    ///
    /// The active pane is marked by a "▍" and its title in the theme's text colour against
    /// the muted one — never by colour alone, so the split still reads on a monochrome
    /// display and in all five themes.
    fn render_pane_label(
        &self,
        id: elle_terminal::SessionId,
        is_active: bool,
        theme: &Theme,
        ui_size: Pixels,
    ) -> Option<impl IntoElement> {
        if !self.is_split() {
            return None;
        }
        let session = self.manager.sessions().iter().find(|session| session.id() == id)?;
        let title = format!("{}{}", if is_active { "▍" } else { " " }, session.title());

        Some(
            div()
                .flex_none()
                .px_1()
                .bg(theme.panel)
                .text_size(ui_size)
                .text_color(if is_active { theme.text } else { theme.text_muted })
                .child(SharedString::from(title)),
        )
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
        fonts: &Fonts,
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
            // `cell_size().1`, not a row height computed here: the same call `sync_size`
            // makes, so what is drawn and what the PTY is told cannot diverge.
            .child(render_grid(
                snapshot,
                geometry,
                selection,
                theme,
                fonts.cell_size().1,
                &fonts.family,
                fonts.size,
                fonts.cell_size().0,
            ))
    }

    /// A one-line banner for a session that has exited or failed.
    fn render_status_banner(
        &self,
        status: Option<SessionStatus>,
        theme: &Theme,
        ui_size: Pixels,
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
            _ => return self.error.clone().map(|error| banner(error, theme, ui_size)),
        };

        Some(banner(SharedString::from(message), theme, ui_size))
    }
}

fn banner(message: SharedString, theme: &Theme, ui_size: Pixels) -> gpui::Div {
    div()
        .flex_none()
        .px_2()
        .py_1()
        .bg(theme.panel)
        .text_color(theme.accent)
        // Explicitly the UI size, overriding the grid's editor size on the parent. A banner
        // is chrome, not terminal output, and it should not grow with ⌘+.
        .text_size(ui_size)
        .child(message)
}

/// Renders the grid, one `StyledText` per row.
///
/// Per row rather than per cell: a 24x80 grid is 1920 cells, and an element per cell is
/// ~2000 elements to lay out every frame. One text element per row with colour runs is 24,
/// and gpui shapes each row once. The cost is that a per-cell *background* needs its own
/// pass — which is also how the selection highlight is drawn: as a background colour on
/// the runs, not as a second layer of rectangles over the text.
/// `row_height` is `Fonts::cell_size().1` — the *same* value `sync_size` divided the panel
/// height by. Drawing rows at one height while telling the PTY it has a screen measured in
/// another means the shell writes rows nobody sees; passing the number in rather than
/// recomputing it is what makes that impossible to get wrong.
fn render_grid(
    snapshot: &GridSnapshot,
    geometry: GridGeometry,
    selection: Option<&Selection>,
    theme: &Theme,
    row_height: Pixels,
    family: &SharedString,
    font_size: Pixels,
    cell_width: Pixels,
) -> impl IntoElement {
    // One cell of breathing room on the left, so the first character is not flush against
    // the panel edge. Subtracted in `sync_size` before the column count is computed — a
    // padding the layout adds and the arithmetic does not know about is exactly how #92
    // handed the PTY a column that could not be drawn.
    div().flex_1().flex().flex_col().overflow_hidden().pl(cell_width).children(
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

            // No `.line_height()`: the row paints itself and takes the height as an
            // argument, so there is nothing for a style to disagree with.
            div().h(row_height).flex_none().child(styled_row(
                line,
                cursor_column,
                selected,
                theme,
                family,
                font_size,
                (cell_width, row_height),
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
    family: &SharedString,
    font_size: Pixels,
    cell: (Pixels, Pixels),
) -> crate::editor::line::Line {
    let (text, runs) = row_runs(cells, cursor_column, selected, theme, family);
    // `force_width` pins every glyph to one cell, which is what makes a terminal a grid
    // rather than a paragraph: a font whose glyphs advance slightly differently would
    // otherwise drift across a long line. Zed's terminal passes the same argument for the
    // same reason (`BatchedTextRun::paint`).
    crate::editor::line::Line::new(text, runs, font_size, cell.1).with_cell_width(cell.0)
}

/// The text and colour runs for one row.
///
/// `family` is the resolved family from [`Fonts`], not a constant (#49) — and it has been
/// verified monospace at selection time, which the terminal depends on more than the editor
/// does: this grid is addressed by column, so a proportional face does not merely look wrong,
/// it puts the cursor on the wrong character.
fn row_runs(
    cells: &[Cell],
    cursor_column: Option<usize>,
    selected: std::ops::Range<usize>,
    theme: &Theme,
    family: &SharedString,
) -> (String, Vec<TextRun>) {
    // A cursor past the last column (end of line) has no cell to invert, so the row is
    // padded with one blank. Doing it up front keeps the run loop single-path.
    let pad = cursor_column.is_some_and(|column| column >= cells.len());

    let mut text = String::with_capacity(cells.len() + 1);
    let mut runs: Vec<TextRun> = Vec::new();

    for (column, cell) in cells.iter().enumerate() {
        // The trailing half of a wide glyph gets a space, not nothing.
        //
        // Skipping it was right while the row was laid out by the text system, which gave
        // an emoji its natural two-cell advance. `force_width` (added so a grid stays a
        // grid) overrides that: every glyph now occupies exactly one cell, so a skipped
        // spacer left the row one column short and everything after the emoji slid left —
        // which is what "descolado" was in the report. A space keeps the column count and
        // the glyph overhangs it, which is what a terminal does anyway.
        if cell.wide_spacer {
            text.push(' ');
            runs.push(TextRun {
                len: 1,
                font: gpui::font(family.clone()),
                color: theme.terminal(cell.fg),
                background_color: background_for(cell, theme),
                underline: None,
                strikethrough: None,
            });
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
                family: family.clone(),
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
            font: gpui::font(family.clone()),
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
        let (text, runs) = row_runs(&cells, None, 0..0, &theme, &Fonts::default().family);

        // Five identically-styled cells must shape as one run, not five.
        assert_eq!(runs.len(), 1, "uniform text should collapse to one run");
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn a_colour_change_starts_a_new_run() {
        let theme = Theme::dark();
        let mut cells: Vec<Cell> = "ab".chars().map(cell).collect();
        cells[1].fg = CellColor::Ansi(1);

        let (text, runs) = row_runs(&cells, None, 0..0, &theme, &Fonts::default().family);
        assert_eq!(runs.len(), 2);
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn a_wide_glyph_keeps_its_second_column() {
        let theme = Theme::dark();
        let cells = vec![
            Cell { c: '漢', ..Cell::default() },
            Cell { wide_spacer: true, ..Cell::default() },
            cell('x'),
        ];

        // This asserted the opposite until the grid gained `force_width`, and the change is
        // a consequence of that rather than a correction.
        //
        // While rows went through the text system a wide glyph advanced two cells on its
        // own, so emitting anything for the spacer would have pushed the row right by one.
        // `force_width` pins *every* glyph to one cell — which is what makes a grid a grid —
        // so the second column has to come from somewhere, and a skipped spacer left the row
        // a column short. Everything after an emoji slid left, which is how this surfaced.
        let (text, runs) = row_runs(&cells, None, 0..0, &theme, &Fonts::default().family);
        assert_eq!(text, "漢 x", "the spacer holds the wide glyph's second column");
        // The multibyte case is where a run length in *chars* rather than bytes would
        // panic inside gpui.
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn a_cursor_past_the_last_column_still_renders() {
        let theme = Theme::dark();
        let cells: Vec<Cell> = "ab".chars().map(cell).collect();

        // End-of-line cursor: there is no cell to invert, so a blank is appended.
        let (text, runs) = row_runs(&cells, Some(2), 0..0, &theme, &Fonts::default().family);
        assert_eq!(text, "ab ");
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn the_cursor_cell_is_inverted() {
        let theme = Theme::dark();
        let cells: Vec<Cell> = "ab".chars().map(cell).collect();
        let (_, runs) = row_runs(&cells, Some(0), 0..0, &theme, &Fonts::default().family);

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
        let (text, runs) = row_runs(&cells, None, 0..0, &theme, &Fonts::default().family);
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
        let (text, runs) = row_runs(&[cell('\0')], None, 0..0, &theme, &Fonts::default().family);
        assert_eq!(text, " ");
        assert_runs_cover_text(&text, &runs);
    }

    #[test]
    fn selected_cells_take_the_themes_selection_colour() {
        let theme = Theme::dark();
        let cells: Vec<Cell> = "abcd".chars().map(cell).collect();
        let (text, runs) = row_runs(&cells, None, 1..3, &theme, &Fonts::default().family);

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
        let (_, runs) = row_runs(&cells, Some(0), 0..2, &theme, &Fonts::default().family);
        assert_eq!(runs[0].background_color, Some(theme.cursor));
    }

    #[test]
    fn a_selection_over_multibyte_text_keeps_the_runs_byte_accurate() {
        let theme = Theme::dark();
        // Selecting from the middle of a multibyte run is where a run length counted in
        // chars rather than bytes panics inside gpui.
        let cells: Vec<Cell> = "ação".chars().map(cell).collect();
        let (text, runs) = row_runs(&cells, None, 1..3, &theme, &Fonts::default().family);
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
