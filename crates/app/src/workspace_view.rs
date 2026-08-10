//! The window root: activity bar, file tree, tabs, editor area, status bar, palette.

use std::path::PathBuf;
use std::sync::Arc;

use elle_core::CommandRegistry;
use elle_laravel::{HttpMethod, Resolved, Route, extract_routes};
use elle_workspace::{CancelFlag, FileTree, read_file, write_file};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, MouseButton, PathPromptOptions, SharedString,
    Task, Window, div, prelude::*, px, svg, uniform_list,
};

use crate::actions::{
    CloseTab, Dispatch, NewFile, NewTerminal, OpenFolder, Save, ToggleCommandPalette,
    ToggleHiddenFiles, ToggleQuickOpen, ToggleTerminal, ToggleTheme, context, dispatch_for,
};
use crate::editor::{Document, EditorView};
use crate::file_cache;
use crate::icons;
use crate::lsp_session::{LSP_POLL_INTERVAL, Lsp, LspState};
use crate::palette::{Palette, PaletteEvent, PaletteMode};
use crate::perf::FrameTimer;
use crate::terminal_view::TerminalView;
use crate::theme::{Metrics, Theme, Themed, set_theme};

/// An open tab.
struct Tab {
    path: Option<PathBuf>,
    editor: Entity<EditorView>,
}

/// The kinds of background work the workspace runs, one slot each.
///
/// The slot is the unit of cancellation: starting a second folder load supersedes the
/// first, and starting a save supersedes an earlier save of the same buffer. Work of a
/// *different* kind is unrelated and must be left alone.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Job {
    OpenFolder,
    OpenFile,
    Save,
    QuickOpenIndex,
    RouteIndex,
    ClosePrompt,
    /// Starting a language server, and then the loop that drains its notifications. One
    /// slot for both because they are strictly sequential — the poll loop only exists once
    /// a start succeeded, and a new start must supersede whatever the old server was doing.
    Lsp,
}

/// One palette row for a route: `GET       /users/{user}  users.show`.
///
/// The whole point of `Resolved<T>` is that this cannot quietly turn "we could not read
/// this" into something that looks like an answer. An unresolved URI renders as the
/// expression that defeated the parser, in angle brackets — `<$legacyUri>` tells the reader
/// both that it is dynamic *and* what to go look at, where an empty cell or a guessed path
/// would be a lie they might act on (RISKS.md #4).
fn route_label(route: &Route) -> String {
    fn show(value: &Resolved<String>) -> String {
        match value {
            Resolved::Known(text) => text.clone(),
            Resolved::Unknown(source) => format!("<{source}>"),
        }
    }

    // Spelled out rather than `{:?}`: Debug renders the data-carrying variants as
    // `Resource("GET")` and `Match(["PUT", "PATCH"])`, which leaks Rust syntax into the UI
    // and blows past the column width that keeps the list scannable.
    let method = match &route.method {
        HttpMethod::Match(verbs) => verbs.join("|"),
        // A resource entry maps to one verb; which one is all the reader needs.
        HttpMethod::Resource(verb) => (*verb).to_string(),
        other => format!("{other:?}").to_uppercase(),
    };
    let mut label = format!("{method:<8}{}", show(&route.uri));
    // A route that never called ->name() gets no name column at all; one that called it with
    // an expression we could not read gets the expression. Those are different facts and the
    // row should not flatten them together.
    if let Some(name) = &route.name {
        label.push_str("  ");
        label.push_str(&show(name));
    }
    label
}

/// One in-flight [`Task`] per [`Job`].
///
/// Dropping a gpui `Task` cancels it: `async_task::Task::drop` calls `set_canceled`, so a
/// future that has not been polled to completion is dropped where it stands and one that
/// was never scheduled never runs at all. That is the intended cancellation mechanism
/// (ADR-0007) — but it only means what it should if the task being dropped is the task the
/// new work supersedes.
///
/// A single shared slot made every operation cancel every other one: ⌘S then ⌘O dropped
/// the save (either the write never started, or it finished and the buffer was never
/// marked clean); ⌘O on a large project then ⌘S dropped the folder load so the tree never
/// appeared; a quick-open walk was dropped by any file open, leaving its blocking walk
/// running with nothing left to consume it. Keying by job is what makes "a new request
/// drops the old one" true of the *same* request rather than of whatever ran last.
///
/// Generic over the handle so the eviction rule — the part that can actually be wrong —
/// is testable with a payload whose drops are observable. `Task<()>` is not: gpui gives no
/// way to ask a task whether it was cancelled.
struct JobSlots<T> {
    slots: Vec<(Job, T)>,
}

/// The workspace's slots, holding real gpui tasks.
type Jobs = JobSlots<Task<()>>;

impl<T> Default for JobSlots<T> {
    fn default() -> Self {
        Self { slots: Vec::new() }
    }
}

impl<T> JobSlots<T> {
    /// Starts `task` as the current work for `job`, cancelling only that job's predecessor.
    fn start(&mut self, job: Job, task: T) {
        match self.slots.iter_mut().find(|(slot, _)| *slot == job) {
            Some(entry) => entry.1 = task,
            None => self.slots.push((job, task)),
        }
    }

    /// Drops the task for `job`, if any, cancelling it.
    fn cancel(&mut self, job: Job) {
        self.slots.retain(|(slot, _)| *slot != job);
    }
}

pub struct WorkspaceView {
    focus_handle: FocusHandle,
    registry: Arc<CommandRegistry>,
    tree: Option<FileTree>,
    tabs: Vec<Tab>,
    active_tab: usize,
    palette: Option<Entity<Palette>>,
    /// The bottom terminal panel. `Some` only while it is open — a panel that is closed is
    /// absent, not hidden, so its poll timer and its shells stop existing with it.
    terminal: Option<Entity<TerminalView>>,
    /// Transient message for the status bar (a failed save, mostly).
    status: Option<SharedString>,
    /// In-flight background work, one slot per [`Job`]. Held rather than detached so a new
    /// request of the same kind drops the old one, which is how ADR-0007's cancellation
    /// actually happens — see [`JobSlots`] for why a single shared slot was wrong.
    jobs: Jobs,
    /// Frame pacing, measured at the window root so it sees every repaint.
    frames: FrameTimer,
    /// Cancels an in-flight quick-open walk. Separate from the task slot because dropping a
    /// Task stops the await, not the blocking walk behind it.
    quick_open_cancel: Option<CancelFlag>,
    /// The language server for the open project, and its diagnostics.
    ///
    /// Owned by the workspace rather than by each editor because there is one server per
    /// *project*, not per file, and because it is the workspace that knows when a folder
    /// was opened or closed. Dropping the workspace drops this, which shuts the server
    /// down — that is what keeps a quit from leaving an orphan behind.
    lsp: Lsp,
}

impl WorkspaceView {
    pub fn new(registry: Arc<CommandRegistry>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            registry,
            tree: None,
            tabs: Vec::new(),
            active_tab: 0,
            palette: None,
            terminal: None,
            status: None,
            jobs: Jobs::default(),
            frames: FrameTimer::new(),
            quick_open_cancel: None,
            lsp: Lsp::new(),
        }
    }

    fn active_editor(&self) -> Option<&Entity<EditorView>> {
        self.tabs.get(self.active_tab).map(|tab| &tab.editor)
    }

    /// Window title: the open folder, plus a dirty marker.
    fn title(&self, cx: &App) -> String {
        let folder = self.tree.as_ref().map(|t| t.root_name());
        let file = self.tabs.get(self.active_tab).map(|tab| {
            let dirty = if tab.editor.read(cx).is_dirty() { "• " } else { "" };
            format!("{dirty}{}", tab.editor.read(cx).document.title())
        });
        match (file, folder) {
            (Some(file), Some(folder)) => format!("{file} — {folder}"),
            (Some(file), None) => file,
            (None, Some(folder)) => folder,
            (None, None) => "ellefuanti".to_string(),
        }
    }

    // --- folder opening ----------------------------------------------------------

    fn open_folder(&mut self, _: &OpenFolder, _w: &mut Window, cx: &mut Context<Self>) {
        // prompt_for_paths returns a oneshot::Receiver, not a Task: three nested layers
        // (channel cancelled / io error / user cancelled).
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });

        let task = cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(root) = paths.into_iter().next() else { return };

            // FileTree::new does blocking IO, so it runs on the background pool. The UI
            // stays interactive while a large folder is read.
            let tree = cx.background_spawn(async move { FileTree::new(root) }).await;

            this.update(cx, |this, cx| {
                match tree {
                    Ok(tree) => {
                        this.tree = Some(tree);
                        this.status = None;
                        // A new project gets a new server, pointed at the new root. The
                        // old one is dropped by `set_root`, which kills its process.
                        this.start_lsp(cx);
                    }
                    Err(err) => this.status = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::OpenFolder, task);
    }

    fn toggle_hidden_files(
        &mut self,
        _: &ToggleHiddenFiles,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tree) = self.tree.as_mut() {
            let show = !tree.show_hidden();
            if let Err(err) = tree.set_show_hidden(show) {
                self.status = Some(format!("{err:#}").into());
            }
            cx.notify();
        }
    }

    /// Switches to the next compiled-in theme.
    ///
    /// `refresh_windows` rather than `cx.notify()`: notify marks *this* entity dirty, and
    /// the editor, terminal and palette are sibling entities that would keep their old
    /// colours until something else happened to redraw them. A theme change is the one
    /// case where every window really is stale, which is what `refresh_windows` means.
    ///
    /// ponytail: no persistence. Reopening the app is back to dark, because remembering the
    /// choice needs somewhere to write it and that is the settings crate, not this.
    fn toggle_theme(&mut self, _: &ToggleTheme, _w: &mut Window, cx: &mut Context<Self>) {
        let next = cx.theme_variant().next();
        set_theme(next, cx);
        self.status = Some(format!("Theme: {}", next.label()).into());
        cx.refresh_windows();
    }

    fn toggle_tree_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(tree) = self.tree.as_mut() {
            if let Err(err) = tree.toggle(index) {
                self.status = Some(format!("{err:#}").into());
            }
            cx.notify();
        }
    }

    // --- file opening ------------------------------------------------------------

    /// The active tab's editor handle, for render tests that need to inspect it.
    #[cfg(test)]
    pub fn active_editor_for_test(&self) -> Option<Entity<EditorView>> {
        self.active_editor().cloned()
    }

    /// Runs the action handlers a render test needs, which are otherwise private.
    ///
    /// Calling the real handlers rather than reimplementing them: a test that opened the
    /// terminal by assigning `self.terminal` would pass while the actual command was
    /// broken, which is the failure this whole issue is about.
    #[cfg(test)]
    pub fn toggle_terminal_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_terminal(&ToggleTerminal, window, cx);
    }

    #[cfg(test)]
    pub fn toggle_command_palette_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_command_palette(&ToggleCommandPalette, window, cx);
    }

    #[cfg(test)]
    pub fn toggle_theme_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_theme(&ToggleTheme, window, cx);
    }

    /// Publishes diagnostics as if a server had sent them, and pushes them to the editors.
    ///
    /// This is the tail of `apply_lsp_events` with the server removed, for the same reason
    /// `open_document_for_test` is the tail of `open_path`: a render test cannot spawn
    /// Intelephense, and the thing worth testing is that diagnostics reach a real layout
    /// pass — not that a subprocess starts.
    #[cfg(test)]
    pub fn publish_diagnostics_for_test(
        &mut self,
        path: &std::path::Path,
        diagnostics: &[elle_lsp::lsp_types::Diagnostic],
        cx: &mut Context<Self>,
    ) {
        let Some(uri) = crate::lsp_session::uri_for(path) else { return };
        let text = self
            .tabs
            .iter()
            .find(|tab| tab.path.as_deref() == Some(path))
            .map(|tab| tab.editor.read(cx).document.buffer.text())
            .unwrap_or_default();

        self.lsp.set_state(LspState::Running);
        self.lsp.set_diagnostics(uri, diagnostics, &text);
        self.push_diagnostics_to_editors(cx);
        cx.notify();
    }

    /// The status-bar text for the language server, for tests that assert on silence.
    #[cfg(test)]
    pub fn lsp_label_for_test(&self) -> String {
        lsp_label(&self.lsp)
    }

    /// Puts an already-built document into a tab, synchronously.
    ///
    /// `open_path` reads from disk on the background executor, which a render test cannot
    /// drive without a real file and a real await. This is the same tail of that function
    /// with the IO removed, so the view under test reaches the state a real open produces.
    #[cfg(test)]
    pub fn open_document_for_test(&mut self, document: Document, cx: &mut Context<Self>) {
        let path = document.path.clone();
        let editor = cx.new(|cx| EditorView::new(document, cx));
        self.tabs.push(Tab { path, editor });
        self.active_tab = self.tabs.len() - 1;
        cx.notify();
    }

    /// Opens a file in a tab, or activates the tab already showing it.
    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.path.as_ref() == Some(&path)) {
            self.active_tab = index;
            cx.notify();
            return;
        }

        let load_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async move { read_file(&load_path) }).await;

            this.update(cx, |this, cx| {
                match loaded {
                    Ok(file) => {
                        match Document::new(Some(path.clone()), &file.text, file.trailing_newline) {
                            Ok(document) => {
                                let text = document.buffer.text();
                                let editor = cx.new(|cx| EditorView::new(document, cx));
                                this.tabs.push(Tab { path: Some(path.clone()), editor });
                                this.active_tab = this.tabs.len() - 1;
                                this.status = None;
                                this.open_on_lsp(&path, &text);
                            }
                            Err(err) => this.status = Some(format!("{err:#}").into()),
                        }
                    }
                    Err(err) => this.status = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::OpenFile, task);
    }

    /// Opens an empty, pathless buffer in a new tab.
    ///
    /// This is the missing half of save-as: `save_as` and `prompt_for_new_path` were already
    /// here, but nothing could ever reach them because every tab was born from a file on
    /// disk and so always had a path. A tab with `path: None` is what makes `save` fall
    /// through to the prompt, so creating one is the entire wiring — no new save path.
    ///
    /// Unlike `open_path` there is no dedup: ⌘N twice means the user wants two scratch
    /// buffers, and they have no path to match on anyway.
    ///
    /// Takes no `Window` — nothing here needs one, and leaving it out is what lets a test
    /// call the real handler rather than a copy of it.
    pub fn new_file(&mut self, cx: &mut Context<Self>) {
        match Document::untitled() {
            Ok(document) => {
                let editor = cx.new(|cx| EditorView::new(document, cx));
                self.tabs.push(Tab { path: None, editor });
                self.active_tab = self.tabs.len() - 1;
                self.status = None;
            }
            // Plain text needs no grammar, so this is unreachable in practice — but it is a
            // Result, and swallowing it would leave ⌘N doing nothing with no explanation.
            Err(err) => self.status = Some(format!("{err:#}").into()),
        }
        cx.notify();
    }

    // --- saving ------------------------------------------------------------------

    fn save(&mut self, _: &Save, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let Some(path) = tab.path.clone() else {
            self.save_as(cx);
            return;
        };

        let editor = tab.editor.clone();
        let snapshot = editor.read(cx).document.snapshot_for_save();
        let (text, version) = (snapshot.text, snapshot.version);

        let synced = (path.clone(), text.clone());

        let task = cx.spawn(async move |this, cx| {
            let written = cx.background_spawn(async move { write_file(&path, &text) }).await;

            this.update(cx, |this, cx| {
                match written {
                    Ok(()) => {
                        // Resync before marking clean, so a server that answers instantly
                        // reports on the text that is now on disk.
                        this.notify_lsp_of_change(&synced.0, &synced.1);
                        // Only clear dirty state after the write actually succeeded, and
                        // only for the text that was actually written.
                        editor
                            .update(cx, |editor, _| editor.document.buffer.mark_saved_at(version));
                        this.status = None;
                    }
                    // The buffer is untouched on failure, so the user loses nothing.
                    Err(err) => this.status = Some(format!("save failed: {err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::Save, task);
    }

    /// Asks for a location, then saves a buffer that has no path yet.
    ///
    /// Suggests the project root, so a new file lands somewhere sensible instead of
    /// wherever the process happens to be running.
    fn save_as(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let editor = tab.editor.clone();

        let directory = self
            .tree
            .as_ref()
            .map(|tree| tree.root().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);

        let chosen = cx.prompt_for_new_path(&directory, Some("untitled.php"));

        let task = cx.spawn(async move |this, cx| {
            // Same three nested layers as prompt_for_paths: channel dropped, IO error,
            // user cancelled. All three mean "do nothing".
            let Ok(Ok(Some(path))) = chosen.await else { return };

            // Serialise *after* the dialog closes, not before. gpui's save panel is not
            // app-modal (`beginWithCompletionHandler:`), so the editor keeps accepting
            // keystrokes for however long the user browses for a folder — a snapshot taken
            // before the prompt would write text the user has already moved past, and the
            // version guard below would then correctly refuse to clear the dirty flag,
            // leaving a file on disk that silently lags the buffer.
            let Ok(snapshot) =
                editor.read_with(cx, |editor, _| editor.document.snapshot_for_save())
            else {
                return;
            };
            let (text, version) = (snapshot.text, snapshot.version);

            let write_path = path.clone();
            let written = cx.background_spawn(async move { write_file(&write_path, &text) }).await;

            this.update(cx, |this, cx| {
                match written {
                    Ok(()) => {
                        editor.update(cx, |editor, _| {
                            editor.document.buffer.mark_saved_at(version);
                            // The document now has a home: adopt the path so the next ⌘S
                            // writes straight through, and so a buffer saved as `.php`
                            // starts highlighting as PHP. A grammar that fails to load
                            // leaves it as plain text rather than failing the save.
                            if let Err(err) = editor.document.set_path(path.clone()) {
                                tracing::warn!(
                                    "saved, but no grammar for {}: {err:#}",
                                    path.display()
                                );
                            }
                        });
                        // Keep the tab's own record in step, or a later save would open
                        // this dialog again for a file that now has a path.
                        if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.editor == editor) {
                            tab.path = Some(path);
                        }
                        this.status = None;
                    }
                    Err(err) => this.status = Some(format!("save failed: {err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::Save, task);
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_at(self.active_tab, window, cx);
    }

    /// Closes the tab at `index`, asking first if it has unsaved changes.
    ///
    /// Losing a user's edits silently is not a simplification worth making, so this is the
    /// one place in the app that deliberately interrupts. Uses the platform dialog rather
    /// than a custom modal: it is native, focus-correct and accessible for free, and an
    /// in-app modal would be more code for a worse result.
    fn close_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else { return };

        if !tab.editor.read(cx).is_dirty() {
            self.remove_tab(index, cx);
            return;
        }

        let title = tab.editor.read(cx).document.title();
        // The entity handle identifies the tab across the await below. Titles are not
        // unique (`app/Models/User.php` and `tests/User.php` share one) and indices shift,
        // so matching on either could close the wrong file — precisely the data loss this
        // prompt exists to prevent.
        let editor = tab.editor.clone();

        // Button order matters: index 0 is the default on macOS, so Cancel sits there.
        // A stray Return must not be the keystroke that discards someone's work.
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("{title} has unsaved changes."),
            Some("Closing this tab will discard them."),
            &["Cancel", "Discard Changes"],
            cx,
        );

        let task = cx.spawn(async move |this, cx| {
            // A dropped receiver means the dialog vanished without an answer; treat that
            // as Cancel, because the safe default is to keep the buffer.
            let Ok(choice) = answer.await else { return };
            if choice != 1 {
                return;
            }
            this.update(cx, |this, cx| {
                // Re-resolve by entity handle: indices shift and titles are not unique, so
                // matching on either could close the wrong file. A tab that has gone away
                // in the meantime resolves to None and this is a no-op, which is right.
                if let Some(current) = this.tabs.iter().position(|tab| tab.editor == editor) {
                    this.remove_tab(current, cx);
                }
            })
            .ok();
        });
        self.jobs.start(Job::ClosePrompt, task);
    }

    fn remove_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let closed = self.tabs.remove(index);
        if let Some(path) = closed.path.as_deref() {
            self.close_on_lsp(path);
        }
        self.active_tab = active_after_close(self.active_tab, index, self.tabs.len());
        cx.notify();
    }

    // --- terminal ----------------------------------------------------------------

    /// Opens the panel with one session, or closes it if it is already open.
    ///
    /// Closing drops the `TerminalView`, which drops its sessions, which kills the shells.
    /// That is the intended meaning of closing the panel: §24's isolation works because a
    /// terminal owns nothing the editor needs.
    fn toggle_terminal(&mut self, _: &ToggleTerminal, window: &mut Window, cx: &mut Context<Self>) {
        match self.terminal.take() {
            Some(_) => {
                // Focus returns to the workspace, or the editor keymap stays dead.
                window.focus(&self.focus_handle);
            }
            None => {
                let terminal = cx.new(TerminalView::new);
                terminal.update(cx, |terminal, cx| {
                    terminal.set_cwd(self.tree.as_ref().map(|tree| tree.root().to_path_buf()));
                    // A panel that opens with no session would just show a placeholder;
                    // the user asked for a terminal, so start one.
                    terminal.open_session(cx);
                });
                window.focus(&terminal.read(cx).focus_handle(cx));
                self.terminal = Some(terminal);
            }
        }
        cx.notify();
    }

    /// Adds a session, opening the panel first if it was closed.
    fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        match self.terminal.clone() {
            Some(terminal) => {
                terminal.update(cx, |terminal, cx| terminal.open_session(cx));
                window.focus(&terminal.read(cx).focus_handle(cx));
                cx.notify();
            }
            // Opening the panel already creates a session, so this is the same path.
            None => self.toggle_terminal(&ToggleTerminal, window, cx),
        }
    }

    // --- palette -----------------------------------------------------------------

    fn toggle_command_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_palette(PaletteMode::Commands, window, cx);
    }

    fn toggle_quick_open(
        &mut self,
        _: &ToggleQuickOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_palette(PaletteMode::Files, window, cx);
    }

    fn toggle_palette(&mut self, mode: PaletteMode, window: &mut Window, cx: &mut Context<Self>) {
        // Same mode reopened means "dismiss"; a different mode swaps the contents.
        if self.palette.as_ref().is_some_and(|p| p.read(cx).mode() == mode) {
            self.dismiss_palette(window, cx);
            return;
        }

        // Swapping modes replaces the palette entity, so a Files walk still running has
        // lost its consumer just as surely as a dismissal would have. ⌘P then ⌘⇧P used to
        // leave that walk grinding through `vendor/` with nowhere to put the result.
        self.cancel_quick_open_walk();

        let items = match mode {
            PaletteMode::Commands => self
                .registry
                .all()
                .iter()
                .map(|command| (command.title.to_string(), command.id.0.to_string()))
                .collect(),
            // Files and routes arrive asynchronously — the palette opens empty and fills in.
            PaletteMode::Files | PaletteMode::Routes => Vec::new(),
        };

        let palette = cx.new(|cx| Palette::new(mode, items, cx));

        // The palette reports outcomes as events rather than calling back into the
        // workspace, so it stays a self-contained widget with no knowledge of what its
        // rows mean. The subscription is dropped with the palette entity.
        cx.subscribe_in(&palette, window, |this, _palette, event, window, cx| match event {
            PaletteEvent::Confirmed(id) => this.confirm_palette(id.clone(), window, cx),
            PaletteEvent::Dismissed => this.dismiss_palette(window, cx),
        })
        .detach();

        window.focus(&palette.read(cx).focus_handle(cx));
        self.palette = Some(palette.clone());

        match mode {
            PaletteMode::Files => self.load_quick_open_items(palette, cx),
            PaletteMode::Routes => self.load_route_items(palette, cx),
            PaletteMode::Commands => {}
        }

        cx.notify();
    }

    /// Fills the palette with the project's files, from the persisted index when it is
    /// current and from a live walk otherwise.
    ///
    /// The palette opens immediately with an empty list rather than waiting: on a large
    /// project the walk takes long enough that blocking on it would be the difference
    /// between an instant palette and a visible stall (§13, §22).
    ///
    /// A cache hit is 3.5-4.5x faster than the walk on real projects — see
    /// [`crate::file_cache`] for the measurements and for why a mismatch re-walks rather
    /// than repairing rows.
    fn load_quick_open_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        // Cancel a walk still running from a previous open. Dropping the Task stops us
        // awaiting it, but the blocking walk on the background thread would run to
        // completion regardless — the flag is what actually stops it (ADR-0007).
        self.cancel_quick_open_walk();
        let cancel = CancelFlag::new();
        self.quick_open_cancel = Some(cancel.clone());

        let task = cx.spawn(async move |this, cx| {
            let load_cancel = cancel.clone();
            let write_root = root.clone();
            let (files, source) =
                cx.background_spawn(async move { file_cache::load(&root, &load_cancel) }).await;

            // A cancelled load returns whatever it had; showing a partial list for a
            // palette the user already closed would be noise.
            if cancel.is_cancelled() {
                return;
            }

            let items = files
                .iter()
                .map(|file| (file.relative.clone(), file.path.display().to_string()))
                .collect();

            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();

            // Persist only what we just walked, and only *after* the palette is filled —
            // the user is never waiting on this write. A cache hit has nothing new to say.
            if source == file_cache::Source::Walk {
                cx.background_spawn(async move { file_cache::store(&write_root, &files, &cancel) })
                    .await;
            }
        });
        self.jobs.start(Job::QuickOpenIndex, task);
    }

    /// Parses `routes/*.php` on the background executor and fills the palette.
    ///
    /// Same shape as the quick-open walk above: tree-sitter over every route file is not
    /// instant on a real project, and blocking on it would stall the palette open.
    ///
    /// A project with no `routes/` is the common case — most folders anyone opens are not
    /// Laravel projects — so an empty list is a normal outcome here, not an error worth
    /// telling the user about.
    fn load_route_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        let task = cx.spawn(async move |this, cx| {
            let items = cx
                .background_spawn(async move {
                    let mut items = Vec::new();
                    let Ok(entries) = std::fs::read_dir(root.join("routes")) else {
                        return items;
                    };

                    let mut files: Vec<_> = entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
                        .collect();
                    // Stable order, so the list does not reshuffle between opens.
                    files.sort();

                    for path in files {
                        let Ok(source) = std::fs::read_to_string(&path) else { continue };
                        for route in extract_routes(&source).routes {
                            items.push((route_label(&route), path.display().to_string()));
                        }
                    }
                    items
                })
                .await;

            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.jobs.start(Job::RouteIndex, task);
    }

    // --- language server ------------------------------------------------------------

    /// Starts a language server for the open folder, if one is configured.
    ///
    /// Nothing waits on this. The handshake runs on the background executor and can take
    /// half a minute against a cold Intelephense indexing `vendor/`; the editor is usable
    /// throughout, which is the whole of §24's "slow to start" case (ADR-0007).
    ///
    /// **A failure here is silent.** Not having a language server installed is the normal
    /// state of a fresh machine, and most folders anyone opens are not PHP projects at
    /// all. A status-bar error on every folder open would be a permanent complaint about
    /// software the user never asked for. The failure goes to the log, where someone
    /// looking for it can find it.
    fn start_lsp(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        self.lsp.set_root(Some(root.clone()));

        let Some(config) = crate::lsp_session::config_for(&root) else {
            // `ELLE_LSP_COMMAND=""` — switched off on purpose, so not even a log warning.
            self.lsp.set_state(LspState::Unavailable);
            return;
        };

        self.lsp.set_state(LspState::Starting);

        let task = cx.spawn(async move |this, cx| {
            let started =
                cx.background_spawn(async move { crate::lsp_session::start(&config) }).await;

            let client = match started {
                Ok(client) => client,
                Err(err) => {
                    // The common case, and it must stay quiet. `debug` rather than `warn`:
                    // "intelephense is not installed" is not a warning about this editor.
                    tracing::debug!("no language server: {err:#}");
                    this.update(cx, |this, _| this.lsp.set_state(LspState::Unavailable)).ok();
                    return;
                }
            };

            let opened = this.update(cx, |this, cx| {
                this.lsp.adopt(client);
                // Tabs opened before the server was ready are unknown to it, so tell it
                // about them now. Without this, a file open at launch never gets
                // diagnostics until it is closed and reopened.
                this.sync_open_documents(cx);
                cx.notify();
            });

            if opened.is_err() {
                return;
            }
            Self::poll_lsp(this, cx).await;
        });
        self.jobs.start(Job::Lsp, task);
    }

    /// Drains server notifications until the server dies or the workspace goes away.
    ///
    /// # Why this polls instead of blocking on the reader
    ///
    /// `Client::wait_for_events` blocks until the server pushes something, which is the
    /// nicer shape — no timer, no idle wakeups. It cannot be used here. The client lives
    /// inside the view, so reaching it means `this.update(…)`, and *that closure runs on
    /// the main thread*: a blocking wait inside it would park the UI for the length of the
    /// timeout, turning "the server is quiet" into dropped frames. ADR-0007's rule that a
    /// blocking call must never happen on the render thread is exactly this case, and the
    /// compiler does not catch it.
    ///
    /// So the loop waits on a background timer and then takes whatever has arrived without
    /// blocking, which is the same pattern `TerminalView` uses for its PTY. The cost is a
    /// wakeup every [`LSP_POLL_INTERVAL`] while a server is running; the alternative would
    /// need the client to own a channel into gpui, which is a runtime this crate's domain
    /// layer deliberately does not have.
    async fn poll_lsp(this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp) {
        loop {
            cx.background_executor().timer(LSP_POLL_INTERVAL).await;

            // `drain_events` takes what is already queued and returns immediately, so the
            // main thread is held only for as long as applying the batch takes.
            let keep_going = this.update(cx, |this, cx| {
                let Some(events) = this.lsp.client_mut().map(|client| client.drain_events()) else {
                    return false;
                };
                this.apply_lsp_events(events, cx)
            });

            // An error means the workspace is gone — the window closed. Falling out of the
            // loop lets the task finish, and dropping the `Lsp` field kills the process.
            match keep_going {
                Ok(true) => {}
                _ => return,
            }
        }
    }

    /// Applies a batch of server notifications. Returns whether to keep polling.
    fn apply_lsp_events(
        &mut self,
        events: Vec<elle_lsp::ServerEvent>,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut changed = false;

        for event in events {
            if let elle_lsp::ServerEvent::Diagnostics { uri, diagnostics, .. } = event {
                // Resolved against our copy of the text, not the server's — the ranges are
                // painted over our buffer. `lsp_session::set_diagnostics` explains why.
                let text = self
                    .tabs
                    .iter()
                    .find(|tab| {
                        tab.path.as_deref().and_then(crate::lsp_session::uri_for).as_ref()
                            == Some(&uri)
                    })
                    .map(|tab| tab.editor.read(cx).document.buffer.text());

                // A publish for a file we do not have open is not an error — servers report
                // on the whole project. Storing it keeps the status-bar total honest.
                self.lsp.set_diagnostics(uri, &diagnostics, text.as_deref().unwrap_or(""));
                changed = true;
            }
        }

        if changed {
            self.push_diagnostics_to_editors(cx);
            cx.notify();
        }

        // A server that died between batches must not be polled forever, and the editor
        // has to carry on without it.
        if !self.lsp.is_running() {
            self.handle_lsp_death(cx);
            return false;
        }
        true
    }

    /// A server that stopped: restart it, or give up and say so.
    ///
    /// Restarting is bounded (`MAX_RESTARTS`). An unbounded restart of a server that dies
    /// on startup is a fork bomb, and an editor spawning processes in a loop is worse than
    /// an editor with no LSP — which is the state §24 says must always remain workable.
    fn handle_lsp_death(&mut self, cx: &mut Context<Self>) {
        let reason = self
            .lsp
            .client_mut()
            .and_then(|client| client.failure())
            .unwrap_or_else(|| "the language server exited".to_string());

        self.lsp.shut_down();
        self.push_diagnostics_to_editors(cx);

        if self.lsp.may_restart() {
            self.lsp.record_restart();
            tracing::warn!("{reason}; restarting the language server");
            self.start_lsp(cx);
            return;
        }

        // Now it *is* worth telling the user: this is not "you never installed one", it is
        // "the one you have keeps dying", and they cannot see that from anywhere else.
        tracing::warn!("{reason}; giving up after {} restarts", self.lsp.restarts());
        self.lsp.set_state(LspState::Failed(reason));
        cx.notify();
    }

    /// Tells the server about every open PHP tab.
    fn sync_open_documents(&mut self, cx: &mut Context<Self>) {
        let documents: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let path = tab.path.as_deref()?;
                if !crate::lsp_session::handles(path) {
                    return None;
                }
                let uri = crate::lsp_session::uri_for(path)?;
                Some((uri, tab.editor.read(cx).document.buffer.text()))
            })
            .collect();

        let Some(client) = self.lsp.client_mut() else { return };
        for (uri, text) in documents {
            // §24 all the way down: a notification that fails to send means the server has
            // gone, and the next poll notices. It is not worth interrupting the user over.
            if let Err(err) = client.did_open(uri, "php", &text) {
                tracing::debug!("could not open a document on the language server: {err:#}");
            }
        }
    }

    /// Hands each editor the diagnostics for its own file.
    ///
    /// A push rather than a pull: an editor that reached into the workspace for its
    /// diagnostics would need a handle to it, and every tab would then have an opinion
    /// about a server dying. See `EditorView::diagnostics`.
    fn push_diagnostics_to_editors(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            let items = tab
                .path
                .as_deref()
                .and_then(crate::lsp_session::uri_for)
                .and_then(|uri| self.lsp.diagnostics_for(&uri))
                .map(|file| {
                    file.items.iter().map(|d| (d.range.clone(), d.severity)).collect::<Vec<_>>()
                })
                .unwrap_or_default();

            tab.editor.update(cx, |editor, cx| {
                editor.set_diagnostics(items);
                cx.notify();
            });
        }
    }

    /// The diagnostic message under the cursor in the active tab, if any.
    fn cursor_diagnostic(&self, cx: &App) -> Option<String> {
        let tab = self.tabs.get(self.active_tab)?;
        let uri = crate::lsp_session::uri_for(tab.path.as_deref()?)?;
        let offset = tab.editor.read(cx).document.selection.head;

        self.lsp.diagnostics_for(&uri)?.at(offset).map(|d| d.message.clone())
    }

    /// Tells the server about one newly opened file.
    ///
    /// A no-op with no server running, which is the ordinary case and deliberately not
    /// worth a branch at the call site.
    fn open_on_lsp(&mut self, path: &std::path::Path, text: &str) {
        if !crate::lsp_session::handles(path) {
            return;
        }
        let Some(uri) = crate::lsp_session::uri_for(path) else { return };
        let Some(client) = self.lsp.client_mut() else { return };

        if let Err(err) = client.did_open(uri, "php", text) {
            tracing::debug!("could not open a document on the language server: {err:#}");
        }
    }

    /// Tells the server a file was closed, so it stops reporting on it.
    fn close_on_lsp(&mut self, path: &std::path::Path) {
        let Some(uri) = crate::lsp_session::uri_for(path) else { return };
        if let Some(client) = self.lsp.client_mut() {
            let _ = client.did_close(&uri);
        }
        // And drop its squiggles. An empty publish is how the rest of this code says
        // "nothing to report here", so closing a file goes through the same path.
        self.lsp.set_diagnostics(uri, &[], "");
    }

    /// Tells the server a document changed.
    ///
    /// Full-text resync rather than an incremental change: the workspace sees the buffer
    /// *after* the edit and has no `Edit` in hand, and reconstructing one to look
    /// incremental would be a second chance to diverge from the server's copy. PHP files
    /// are small enough that the bandwidth is not the constraint — see
    /// `Client::did_change_full`.
    ///
    /// ponytail: this is called on save, not on keystroke. Diagnostics therefore update
    /// when you save, which is a defensible place to start and not where it should end;
    /// per-keystroke sync wants a debounce, and a debounce wants a timer this does not have
    /// yet. Doing it on save first means the wiring is exercised without putting a
    /// notification on the typing path before there is anything to measure it with.
    fn notify_lsp_of_change(&mut self, path: &std::path::Path, text: &str) {
        if !crate::lsp_session::handles(path) {
            return;
        }
        let Some(uri) = crate::lsp_session::uri_for(path) else { return };
        let Some(client) = self.lsp.client_mut() else { return };

        if let Err(err) = client.did_change_full(&uri, text) {
            tracing::debug!("could not sync a document to the language server: {err:#}");
        }
    }

    /// Stops any quick-open walk. Called whenever the palette that would consume its
    /// results goes away — dismissed *or* replaced by a different mode.
    fn cancel_quick_open_walk(&mut self) {
        if let Some(cancel) = self.quick_open_cancel.take() {
            cancel.cancel();
        }
        // The flag stops the blocking walk; dropping the task stops us awaiting it. Both
        // are needed, and only doing the first leaves a task holding the old palette alive.
        self.jobs.cancel(Job::QuickOpenIndex);
    }

    fn dismiss_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A walk still running is now pure waste — nothing will consume its results.
        self.cancel_quick_open_walk();
        self.palette = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Runs whatever the palette confirmed.
    fn confirm_palette(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self.palette.as_ref().map(|p| p.read(cx).mode());
        self.dismiss_palette(window, cx);

        match mode {
            // Routes carry the file that declares them; opening it is the whole
            // navigation for now. ponytail: the route's line is known and not used —
            // `open_path` loads asynchronously and has nowhere to put a cursor target,
            // so jumping to the line means threading one through that load.
            Some(PaletteMode::Files | PaletteMode::Routes) => self.open_path(PathBuf::from(id), cx),
            Some(PaletteMode::Commands) => {
                // Dispatch through the same enum the keymap uses, so a palette entry and
                // a keybinding cannot drift apart.
                match dispatch_for(elle_core::CommandId(leak_id(&self.registry, &id))) {
                    Dispatch::OpenFolder => self.open_folder(&OpenFolder, window, cx),
                    Dispatch::NewFile => self.new_file(cx),
                    Dispatch::Save => self.save(&Save, window, cx),
                    Dispatch::CloseTab => self.close_tab(&CloseTab, window, cx),
                    Dispatch::QuickOpen => self.toggle_palette(PaletteMode::Files, window, cx),
                    Dispatch::Routes => self.toggle_palette(PaletteMode::Routes, window, cx),
                    Dispatch::NewTerminal => self.new_terminal(&NewTerminal, window, cx),
                    Dispatch::ToggleTerminal => self.toggle_terminal(&ToggleTerminal, window, cx),
                    Dispatch::ToggleTheme => self.toggle_theme(&ToggleTheme, window, cx),
                    Dispatch::Quit => cx.quit(),
                    Dispatch::Unhandled => {
                        self.status = Some(format!("{id} is not implemented yet").into());
                        cx.notify();
                    }
                }
            }
            None => {}
        }
    }
}

/// The status-bar text for the language server, which is usually nothing at all.
///
/// The rule this encodes is §24's, at the last step before pixels: **not having a language
/// server is not a problem the user has.** Nobody has Intelephense on a fresh machine and
/// most folders anyone opens are not PHP projects, so `Unavailable` renders as an empty
/// string — no icon, no "LSP: off", nothing to dismiss or wonder about. The editor with no
/// server looks exactly like the editor before this feature existed, which is the point.
///
/// What does get said:
/// - problems, when there are any, because that is the feature;
/// - `Starting…`, because a server indexing `vendor/` for thirty seconds otherwise looks
///   like nothing happening;
/// - a failure, but only after the restart budget is spent — "the server you installed
///   keeps dying" is something the user cannot learn anywhere else.
fn lsp_label(lsp: &Lsp) -> String {
    match lsp.state() {
        // The two silent states, and the reason this function exists.
        LspState::Idle | LspState::Unavailable => String::new(),
        LspState::Starting => "Starting…".to_string(),
        LspState::Failed(_) => "LSP unavailable".to_string(),
        LspState::Running => match lsp.totals() {
            (0, 0) => String::new(),
            (errors, 0) => format!("{errors} ✕"),
            (0, warnings) => format!("{warnings} ⚠"),
            (errors, warnings) => format!("{errors} ✕  {warnings} ⚠"),
        },
    }
}

/// Which tab is active after the one at `closed` is removed, leaving `remaining` tabs.
///
/// Closing a tab *before* the active one shifts every later tab down by one, so keeping
/// `active` where it is would silently switch the user to a different file. Clamping alone
/// was enough while ⌘W was the only way to close — it always closed the active tab, the one
/// case where clamping is right — but the per-tab ✕ closes an arbitrary index, so the shift
/// is now reachable by a single click on any tab left of the current one.
fn active_after_close(active: usize, closed: usize, remaining: usize) -> usize {
    // Follow the same file when it slides down; closing the active tab itself falls through
    // to the clamp, which lands on the neighbour that took its place (or the new last tab).
    let active = if closed < active { active - 1 } else { active };
    active.min(remaining.saturating_sub(1))
}

/// Recovers the `&'static str` id for a title the palette returned.
///
/// `CommandId` holds a `&'static str` so the registry costs no allocation; the palette
/// hands back an owned String. Looking it up in the registry recovers the static without
/// leaking memory, which is why this is a lookup rather than a `Box::leak`.
fn leak_id(registry: &CommandRegistry, id: &str) -> &'static str {
    registry
        .all()
        .iter()
        .find(|command| command.id.0 == id)
        .map(|command| command.id.0)
        .unwrap_or("")
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frames.tick();

        let theme = cx.theme().clone();
        window.set_window_title(&self.title(cx));

        div()
            .key_context(context::WORKSPACE)
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(|this, _: &NewFile, _window, cx| this.new_file(cx)))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::toggle_quick_open))
            .on_action(cx.listener(Self::toggle_hidden_files))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::toggle_theme))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_size(Metrics::UI_FONT_SIZE)
            .text_color(theme.text)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_activity_bar(&theme))
                    .child(self.render_sidebar(&theme, cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(self.render_tab_bar(&theme, cx))
                            .child(self.render_editor_area(&theme))
                            // Below the editor and inside its column, so the sidebar keeps
                            // its full height — the layout every other IDE uses.
                            .children(self.terminal.clone()),
                    ),
            )
            .child(self.render_status_bar(&theme, cx))
            .children(self.palette.clone().map(|palette| {
                // The overlay is absolutely positioned over everything, so it does not
                // reflow the layout underneath while it is open.
                div().absolute().top_0().left_0().size_full().flex().justify_center().child(palette)
            }))
    }
}

impl WorkspaceView {
    fn render_activity_bar(&self, theme: &Theme) -> impl IntoElement {
        // Later panels are shown disabled rather than hidden, so the shape of the product
        // is legible from the first commit (§6) without pretending they work.
        //
        // Paired with `icons::ICONS` positionally, and `panels_and_icons_stay_aligned`
        // below is what keeps that honest — a zip would silently drop a panel if the two
        // ever fell out of step.
        let panels = [
            ("Explorer", true),
            ("Search", false),
            ("Git", false),
            ("Laravel", false),
            ("Database", false),
            ("Docker", false),
            ("Tests", false),
        ];

        div()
            .w(Metrics::ACTIVITY_BAR_WIDTH)
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .py_2()
            .bg(theme.panel)
            .border_r_1()
            .border_color(theme.border)
            .children(panels.into_iter().zip(icons::ICONS).map(|((name, enabled), icon)| {
                div()
                    .id(name)
                    .size(px(32.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .when(enabled, |el| el.bg(theme.selected).text_color(theme.accent))
                    .when(!enabled, |el| el.text_color(theme.text_muted))
                    // 16px inside a 32px hit target: the icon is the glyph, the square is
                    // the thing you can hit, and VS Code uses the same ratio.
                    //
                    // The colour is set on this parent, not on the svg. gpui rasterises
                    // the SVG to an alpha mask and fills it with `style.text.color`, so
                    // the icon inherits `text_color` above and every theme variant
                    // recolours it for free. An icon with a hardcoded fill would be
                    // invisible in at least one of the five.
                    .child(svg().path(icon.path).size(px(16.0)))
            }))
    }

    fn render_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self
            .tree
            .as_ref()
            .map(|tree| tree.root_name().to_uppercase())
            .unwrap_or_else(|| "NO FOLDER OPEN".to_string());

        div()
            .w(Metrics::SIDEBAR_WIDTH)
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme.panel)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .h(Metrics::TAB_HEIGHT)
                    .flex()
                    .items_center()
                    .px_3()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(header)),
            )
            .child(match self.tree.as_ref() {
                Some(tree) if !tree.is_empty() => {
                    self.render_tree_rows(tree.len(), theme, cx).into_any_element()
                }
                _ => div()
                    .p_3()
                    .text_color(theme.text_muted)
                    .child("Press ⌘O to open a folder")
                    .into_any_element(),
            })
    }

    fn render_tree_rows(
        &self,
        count: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        let text = theme.text;
        let muted = theme.text_muted;
        let hover = theme.hover;

        uniform_list("file-tree", count, move |range, _window, cx| {
            entity.update(cx, |this, _cx| {
                let Some(tree) = this.tree.as_ref() else { return Vec::new() };

                range
                    .filter_map(|index| {
                        let entry = tree.entries().get(index)?;
                        let entity = entity.clone();
                        let path = entry.path.clone();
                        let is_dir = entry.is_dir();

                        Some(
                            div()
                                .id(("tree", index))
                                .flex()
                                .items_center()
                                .h(Metrics::ROW_HEIGHT)
                                .px_2()
                                // Indent by depth; the flat list makes this just arithmetic.
                                .pl(px(8.0 + entry.depth as f32 * 12.0))
                                .hover(|el| el.bg(hover))
                                .cursor_pointer()
                                .text_color(if is_dir { text } else { muted })
                                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                    entity.update(cx, |this, cx| {
                                        if is_dir {
                                            this.toggle_tree_entry(index, cx);
                                        } else {
                                            this.open_path(path.clone(), cx);
                                        }
                                    });
                                })
                                .child(SharedString::from(if is_dir {
                                    format!(
                                        "{} {}",
                                        if entry.expanded { "▾" } else { "▸" },
                                        entry.name
                                    )
                                } else {
                                    format!("  {}", entry.name)
                                }))
                                .into_any_element(),
                        )
                    })
                    .collect()
            })
        })
        .flex_1()
    }

    fn render_tab_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let active = self.active_tab;

        div()
            .h(Metrics::TAB_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                let dirty = tab.editor.read(cx).is_dirty();
                let title = tab.editor.read(cx).document.title();
                let entity = entity.clone();
                let close_entity = entity.clone();

                div()
                    .id(("tab", index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h_full()
                    .px_3()
                    .cursor_pointer()
                    .when(index == active, |el| {
                        el.bg(theme.background).border_b_2().border_color(theme.accent)
                    })
                    .when(index != active, |el| el.text_color(theme.text_muted))
                    .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                        entity.update(cx, |this, cx| {
                            this.active_tab = index;
                            cx.notify();
                        });
                    })
                    .child(SharedString::from(title))
                    .child(
                        // One slot for both the dirty marker and the close button, so the
                        // tab width never shifts as the pointer crosses it. A dirty tab
                        // still shows ✕ on hover — hiding the close affordance on exactly
                        // the tabs where closing matters most would be backwards.
                        div()
                            .id(("close", index))
                            .w(px(14.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .text_color(if dirty { theme.accent } else { theme.text_muted })
                            .child(if dirty { "•" } else { "✕" })
                            .hover(|el| el.bg(theme.hover).text_color(theme.text))
                            .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                                close_entity.update(cx, |this, cx| {
                                    this.close_tab_at(index, window, cx);
                                });
                                // Without this the tab's own handler also fires and
                                // activates the tab being closed.
                                cx.stop_propagation();
                            }),
                    )
            }))
    }

    fn render_editor_area(&self, theme: &Theme) -> impl IntoElement {
        div().flex_1().overflow_hidden().child(match self.active_editor() {
            Some(editor) => editor.clone().into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_muted)
                .child("⌘O open folder   ⌘P quick open   ⌘⇧P commands")
                .into_any_element(),
        })
    }

    fn render_status_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let (position, language) = match self.active_editor() {
            Some(editor) => {
                let editor = editor.read(cx);
                let point = editor.document.cursor_point();
                (
                    format!("Ln {}, Col {}", point.row + 1, point.column + 1),
                    editor.document.language().name().to_string(),
                )
            }
            None => (String::new(), String::new()),
        };

        // Only shown while the panel is open, so it reads as state rather than chrome.
        let terminals = self
            .terminal
            .as_ref()
            .map(|terminal| match terminal.read(cx).session_count() {
                1 => "1 terminal".to_string(),
                count => format!("{count} terminals"),
            })
            .unwrap_or_default();

        let diagnostics = lsp_label(&self.lsp);

        // A squiggle the user cannot read only says "something is wrong near here". Putting
        // the cursor on it spells it out, which is the cheapest thing that makes the
        // underlines actually useful — a hover card or a problems panel is a bigger feature
        // (#59 keeps this to diagnostics only).
        //
        // `self.status` wins the cell: a failed save is a thing the user just did, and it
        // must not be buried under a diagnostic that was already there.
        let message = self.status.clone().unwrap_or_else(|| {
            self.cursor_diagnostic(cx).map(SharedString::from).unwrap_or_default()
        });

        div()
            .h(Metrics::STATUS_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.border)
            .text_color(theme.text_muted)
            .child(
                div()
                    .flex_1()
                    // An error is the one thing in the status bar worth colouring.
                    .when(self.status.is_some(), |el| el.text_color(theme.accent))
                    .child(message),
            )
            .child(SharedString::from(diagnostics))
            .child(SharedString::from(terminals))
            .child(SharedString::from(position))
            .child(SharedString::from(language))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// `render_activity_bar` zips its panel list against `icons::ICONS`, and `zip` stops at
    /// the shorter side — so adding a panel without adding an icon would silently drop the
    /// last panel off the bar rather than fail. Assert the lengths match.
    ///
    /// The names are asserted too, because equal lengths in the wrong order is the other
    /// way to get this wrong, and it is the more confusing one: every icon renders, each
    /// against the wrong panel.
    #[test]
    fn panels_and_icons_stay_aligned() {
        let expected = ["explorer", "search", "git", "laravel", "database", "docker", "tests"];

        assert_eq!(
            icons::ICONS.len(),
            expected.len(),
            "the activity bar renders {} panels; add the matching icon to icons::ICONS",
            expected.len()
        );

        for (icon, name) in icons::ICONS.iter().zip(expected) {
            assert_eq!(
                icon.path,
                format!("icons/{name}.svg"),
                "icons::ICONS is out of order: every panel would get the wrong glyph"
            );
        }
    }

    /// Stands in for a `Task`, recording its own cancellation.
    ///
    /// A dropped gpui `Task` *is* a cancelled task (`async_task::Task::drop` calls
    /// `set_canceled`), but gpui offers no way to observe that, so the eviction rule is
    /// tested against a handle whose drop is visible.
    struct FakeTask {
        job: Job,
        cancelled: Rc<RefCell<Vec<Job>>>,
    }

    impl Drop for FakeTask {
        fn drop(&mut self) {
            self.cancelled.borrow_mut().push(self.job);
        }
    }

    struct Harness {
        slots: JobSlots<FakeTask>,
        cancelled: Rc<RefCell<Vec<Job>>>,
    }

    impl Harness {
        fn new() -> Self {
            Self { slots: JobSlots::default(), cancelled: Rc::new(RefCell::new(Vec::new())) }
        }

        fn start(&mut self, job: Job) {
            let task = FakeTask { job, cancelled: self.cancelled.clone() };
            self.slots.start(job, task);
        }

        fn cancelled(&self) -> Vec<Job> {
            self.cancelled.borrow().clone()
        }
    }

    #[test]
    fn starting_a_save_does_not_cancel_an_in_flight_folder_load() {
        // The headline bug. `pending` was one slot, so ⌘O on a large project followed by
        // ⌘S dropped the folder-load task: the walk finished on the background pool and
        // its result was thrown away, so the sidebar stayed on "NO FOLDER OPEN" forever
        // with no error shown anywhere.
        let mut h = Harness::new();
        h.start(Job::OpenFolder);
        h.start(Job::Save);

        assert!(h.cancelled().is_empty(), "unrelated work must survive: {:?}", h.cancelled());
    }

    #[test]
    fn starting_a_folder_load_does_not_cancel_an_in_flight_save() {
        // The same bug in the direction that loses data. A save in flight that gets
        // dropped either never runs its write at all, or completes the write while
        // `mark_saved` never runs — leaving a saved file the editor still reports as dirty,
        // and a close prompt for changes that are already on disk.
        let mut h = Harness::new();
        h.start(Job::Save);
        h.start(Job::OpenFolder);

        assert!(h.cancelled().is_empty(), "the save must survive: {:?}", h.cancelled());
    }

    #[test]
    fn opening_a_file_does_not_cancel_a_quick_open_walk() {
        // ⌘P starts a walk; clicking a file in the tree used to drop that task while
        // leaving its CancelFlag unset, so the blocking walk ground on with nothing left
        // to consume it.
        let mut h = Harness::new();
        h.start(Job::QuickOpenIndex);
        h.start(Job::OpenFile);

        assert!(h.cancelled().is_empty());
    }

    #[test]
    fn a_close_prompt_survives_every_other_operation() {
        // A dialog awaiting an answer is the worst thing to drop: the await is abandoned,
        // the answer is discarded, and "Discard Changes" silently does nothing.
        let mut h = Harness::new();
        h.start(Job::ClosePrompt);
        h.start(Job::Save);
        h.start(Job::OpenFolder);
        h.start(Job::OpenFile);
        h.start(Job::QuickOpenIndex);

        assert!(h.cancelled().is_empty(), "{:?}", h.cancelled());
    }

    #[test]
    fn a_second_request_of_the_same_kind_cancels_the_first() {
        // The behaviour that must be *kept*: superseding work of the same kind is exactly
        // what ADR-0007 asks for. Two folder loads in a row means the second one wins.
        let mut h = Harness::new();
        h.start(Job::OpenFolder);
        h.start(Job::OpenFolder);

        assert_eq!(h.cancelled(), vec![Job::OpenFolder]);
    }

    #[test]
    fn cancelling_a_job_drops_only_that_job() {
        let mut h = Harness::new();
        h.start(Job::QuickOpenIndex);
        h.start(Job::Save);

        h.slots.cancel(Job::QuickOpenIndex);

        assert_eq!(h.cancelled(), vec![Job::QuickOpenIndex]);
    }

    #[test]
    fn cancelling_a_job_that_is_not_running_is_a_no_op() {
        let mut h = Harness::new();
        h.slots.cancel(Job::QuickOpenIndex);
        assert!(h.cancelled().is_empty());
    }

    #[test]
    fn closing_a_tab_left_of_the_active_one_keeps_the_same_file_active() {
        // Three tabs with the third active, closing the first: the clamp happens to agree
        // here (both give 1), because the active tab was last and clamping to the new end
        // lands on the right file by luck. Kept as a regression guard, not as evidence.
        assert_eq!(active_after_close(2, 0, 2), 1);
        // This is the case that actually proves the bug — the only one where the clamp
        // cannot hide it. Five tabs, fourth active, close the second: the three later tabs
        // shift down, so the active file is now at 2. Clamping alone returns 3, which is a
        // different file. The user closed one file and silently got shown another.
        assert_eq!(active_after_close(3, 1, 4), 2);
    }

    #[test]
    fn closing_a_tab_right_of_the_active_one_leaves_it_alone() {
        // Nothing before the active tab moved, so neither should the selection.
        assert_eq!(active_after_close(0, 1, 2), 0);
        assert_eq!(active_after_close(1, 3, 3), 1);
    }

    #[test]
    fn closing_the_active_tab_selects_the_one_that_took_its_place() {
        // ⌘W's case, and the only one the old clamp got right: the neighbour that slid into
        // the slot becomes active.
        assert_eq!(active_after_close(1, 1, 3), 1);
        // Closing the last tab has no successor, so it falls back to the new last one.
        assert_eq!(active_after_close(2, 2, 2), 1);
        // Closing the only tab leaves nothing; index 0 is what an empty tab bar renders as.
        assert_eq!(active_after_close(0, 0, 0), 0);
    }

    #[test]
    fn every_job_gets_its_own_slot() {
        // Guards against a slot count regression: if two distinct jobs ever collided the
        // whole point of keying by job is gone.
        let mut h = Harness::new();
        let all = [
            Job::OpenFolder,
            Job::OpenFile,
            Job::Save,
            Job::QuickOpenIndex,
            Job::ClosePrompt,
            Job::Lsp,
        ];
        for job in all {
            h.start(job);
        }
        assert_eq!(h.slots.slots.len(), all.len());
        assert!(h.cancelled().is_empty());
    }

    #[test]
    fn starting_a_language_server_cancels_only_the_previous_one() {
        // Opening a second folder must stop the first project's poll loop — it holds a
        // client for a server pointed at a directory nobody is looking at any more. It
        // must not touch a save or a walk in flight.
        let mut h = Harness::new();
        h.start(Job::Save);
        h.start(Job::Lsp);
        h.start(Job::QuickOpenIndex);
        assert!(h.cancelled().is_empty());

        h.start(Job::Lsp);
        assert_eq!(h.cancelled(), vec![Job::Lsp]);
    }
}

#[cfg(test)]
mod lsp_status_tests {
    use super::*;
    use crate::lsp_session::Lsp;

    /// §24 at the last step before pixels, and the single most important assertion in this
    /// file: **an editor with no language server must look exactly like an editor from
    /// before this feature existed.** No icon, no "LSP: off", nothing to dismiss.
    ///
    /// This is the path almost every user is on — nobody has Intelephense on a fresh
    /// machine, and most folders anyone opens are not PHP projects. If it ever starts
    /// saying something, it says it to everybody, forever.
    #[test]
    fn a_missing_language_server_says_nothing_at_all() {
        let mut lsp = Lsp::new();

        // Never attempted: no folder open.
        assert_eq!(lsp_label(&lsp), "");

        // Attempted and the binary was not there.
        lsp.set_state(LspState::Unavailable);
        assert_eq!(lsp_label(&lsp), "", "a missing server is not the user's problem");
    }

    #[test]
    fn a_running_server_with_nothing_to_report_also_says_nothing() {
        // A clean file is the normal state. "0 problems" is chrome.
        let mut lsp = Lsp::new();
        lsp.set_state(LspState::Running);
        assert_eq!(lsp_label(&lsp), "");
    }

    #[test]
    fn a_slow_start_is_visible_because_otherwise_nothing_is_happening() {
        // Intelephense indexing a large vendor/ tree takes tens of seconds, during which
        // an empty status bar is indistinguishable from a broken one.
        let mut lsp = Lsp::new();
        lsp.set_state(LspState::Starting);
        assert_eq!(lsp_label(&lsp), "Starting…");
    }

    #[test]
    fn a_server_that_kept_dying_is_reported_once_the_budget_is_spent() {
        // The one failure worth saying out loud: not "you never installed one", but "the
        // one you have will not stay up", which the user cannot learn anywhere else.
        let mut lsp = Lsp::new();
        lsp.set_state(LspState::Failed("it exited".into()));
        assert_eq!(lsp_label(&lsp), "LSP unavailable");
    }

    #[test]
    fn problem_counts_read_as_counts() {
        use elle_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

        fn at(line: u32, severity: DiagnosticSeverity) -> Diagnostic {
            Diagnostic {
                range: Range {
                    start: Position { line, character: 0 },
                    end: Position { line, character: 1 },
                },
                severity: Some(severity),
                message: "x".into(),
                ..Default::default()
            }
        }

        let mut lsp = Lsp::new();
        lsp.set_state(LspState::Running);
        let uri: elle_lsp::lsp_types::Uri = "file:///a.php".parse().unwrap();
        let text = "a\nb\nc\n";

        lsp.set_diagnostics(uri.clone(), &[at(0, DiagnosticSeverity::ERROR)], text);
        assert_eq!(lsp_label(&lsp), "1 ✕");

        lsp.set_diagnostics(
            uri.clone(),
            &[at(0, DiagnosticSeverity::ERROR), at(1, DiagnosticSeverity::WARNING)],
            text,
        );
        assert_eq!(lsp_label(&lsp), "1 ✕  1 ⚠");

        lsp.set_diagnostics(uri.clone(), &[at(0, DiagnosticSeverity::WARNING)], text);
        assert_eq!(lsp_label(&lsp), "1 ⚠");

        // And back to silence when the file is fixed.
        lsp.set_diagnostics(uri, &[], text);
        assert_eq!(lsp_label(&lsp), "");
    }
}

#[cfg(test)]
mod route_label_tests {
    use super::*;

    /// The row must never present an unreadable value as if it were read. This is RISKS.md
    /// #4 at the last step before pixels: everything upstream can be honest and a label that
    /// prints `Unknown` as an empty string throws it all away.
    #[test]
    fn an_unresolved_uri_shows_the_expression_that_defeated_us() {
        let route = Route {
            method: elle_laravel::HttpMethod::Get,
            uri: Resolved::Unknown("$legacyUri".into()),
            name: Some(Resolved::Unknown("$legacyName".into())),
            action: Resolved::Unknown("[$c, 'h']".into()),
            middleware: vec![],
            line: 36,
        };
        let label = route_label(&route);
        assert!(label.contains("<$legacyUri>"), "got {label:?}");
        assert!(label.contains("<$legacyName>"), "got {label:?}");
        assert!(!label.contains("  <$legacyUri>  \n"), "no empty placeholder");
    }

    /// `None` (never called ->name()) and `Some(Unknown)` (called it with an expression) are
    /// different facts. Flattening them into one blank column loses the distinction the
    /// extractor went to trouble to preserve.
    #[test]
    fn a_route_with_no_name_gets_no_name_column() {
        let route = Route {
            method: elle_laravel::HttpMethod::Post,
            uri: Resolved::Known("/users".into()),
            name: None,
            action: Resolved::Known(elle_laravel::RouteAction::Closure),
            middleware: vec![],
            line: 7,
        };
        let label = route_label(&route);
        assert!(label.contains("/users"));
        assert!(!label.contains('<'), "nothing unresolved here: {label:?}");
        assert_eq!(label.trim_end(), label, "no trailing pad for an absent name");
    }
}
