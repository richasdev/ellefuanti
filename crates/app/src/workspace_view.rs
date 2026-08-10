//! The window root: activity bar, file tree, tabs, editor area, status bar, palette.

use std::path::PathBuf;
use std::sync::Arc;

use elle_core::CommandRegistry;
use elle_workspace::{CancelFlag, FileTree, index_files, read_file, write_file};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, MouseButton, PathPromptOptions, SharedString,
    Task, Window, div, prelude::*, px, uniform_list,
};

use crate::actions::{
    CloseTab, Dispatch, NewFile, NewTerminal, OpenFolder, Save, ToggleCommandPalette,
    ToggleHiddenFiles, ToggleQuickOpen, ToggleTerminal, context, dispatch_for,
};
use crate::editor::{Document, EditorView};
use crate::palette::{Palette, PaletteEvent, PaletteMode};
use crate::perf::FrameTimer;
use crate::terminal_view::TerminalView;
use crate::theme::{Metrics, Theme};

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
    ClosePrompt,
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
                                let editor = cx.new(|cx| EditorView::new(document, cx));
                                this.tabs.push(Tab { path: Some(path), editor });
                                this.active_tab = this.tabs.len() - 1;
                                this.status = None;
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

        let task = cx.spawn(async move |this, cx| {
            let written = cx.background_spawn(async move { write_file(&path, &text) }).await;

            this.update(cx, |this, cx| {
                match written {
                    Ok(()) => {
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
        self.tabs.remove(index);
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
            // Files arrive asynchronously — the palette opens empty and fills in.
            PaletteMode::Files => Vec::new(),
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

        if mode == PaletteMode::Files {
            self.load_quick_open_items(palette, cx);
        }

        cx.notify();
    }

    /// Walks the project on the background executor and fills the palette when it lands.
    ///
    /// The palette opens immediately with an empty list rather than waiting: on a large
    /// project the walk takes long enough that blocking on it would be the difference
    /// between an instant palette and a visible stall (§13, §22).
    fn load_quick_open_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        // Cancel a walk still running from a previous open. Dropping the Task stops us
        // awaiting it, but the blocking walk on the background thread would run to
        // completion regardless — the flag is what actually stops it (ADR-0007).
        self.cancel_quick_open_walk();
        let cancel = CancelFlag::new();
        self.quick_open_cancel = Some(cancel.clone());

        let task = cx.spawn(async move |this, cx| {
            let walk_cancel = cancel.clone();
            let files = cx.background_spawn(async move { index_files(&root, &walk_cancel) }).await;

            // A cancelled walk returns whatever it had; showing a partial list for a
            // palette the user already closed would be noise.
            if cancel.is_cancelled() {
                return;
            }

            let items = files
                .into_iter()
                .map(|file| (file.relative, file.path.display().to_string()))
                .collect();

            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.jobs.start(Job::QuickOpenIndex, task);
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
            Some(PaletteMode::Files) => self.open_path(PathBuf::from(id), cx),
            Some(PaletteMode::Commands) => {
                // Dispatch through the same enum the keymap uses, so a palette entry and
                // a keybinding cannot drift apart.
                match dispatch_for(elle_core::CommandId(leak_id(&self.registry, &id))) {
                    Dispatch::OpenFolder => self.open_folder(&OpenFolder, window, cx),
                    Dispatch::NewFile => self.new_file(cx),
                    Dispatch::Save => self.save(&Save, window, cx),
                    Dispatch::CloseTab => self.close_tab(&CloseTab, window, cx),
                    Dispatch::QuickOpen => self.toggle_palette(PaletteMode::Files, window, cx),
                    Dispatch::NewTerminal => self.new_terminal(&NewTerminal, window, cx),
                    Dispatch::ToggleTerminal => self.toggle_terminal(&ToggleTerminal, window, cx),
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

        let theme = Theme::dark();
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
            .children(panels.into_iter().map(|(name, enabled)| {
                div()
                    .id(name)
                    .size(px(32.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .when(enabled, |el| el.bg(theme.selected).text_color(theme.accent))
                    .when(!enabled, |el| el.text_color(theme.text_muted))
                    // First letter as a placeholder glyph: real icons are an assets task,
                    // and a letter is honest about being a placeholder.
                    .child(SharedString::from(name[..1].to_string()))
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
                    .child(self.status.clone().unwrap_or_default()),
            )
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
        let all =
            [Job::OpenFolder, Job::OpenFile, Job::Save, Job::QuickOpenIndex, Job::ClosePrompt];
        for job in all {
            h.start(job);
        }
        assert_eq!(h.slots.slots.len(), all.len());
        assert!(h.cancelled().is_empty());
    }
}
