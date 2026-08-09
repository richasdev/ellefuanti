//! The window root: activity bar, file tree, tabs, editor area, status bar, palette.

use std::path::PathBuf;
use std::sync::Arc;

use elle_core::CommandRegistry;
use elle_workspace::{FileTree, read_file, write_file};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, MouseButton, PathPromptOptions, SharedString,
    Task, Window, div, prelude::*, px, uniform_list,
};

use crate::actions::{
    CloseTab, Dispatch, OpenFolder, Save, ToggleCommandPalette, ToggleHiddenFiles, ToggleQuickOpen,
    context, dispatch_for,
};
use crate::editor::{Document, EditorView};
use crate::palette::{Palette, PaletteEvent, PaletteMode};
use crate::perf::FrameTimer;
use crate::theme::{Metrics, Theme};

/// An open tab.
struct Tab {
    path: Option<PathBuf>,
    editor: Entity<EditorView>,
}

pub struct WorkspaceView {
    focus_handle: FocusHandle,
    registry: Arc<CommandRegistry>,
    tree: Option<FileTree>,
    tabs: Vec<Tab>,
    active_tab: usize,
    palette: Option<Entity<Palette>>,
    /// Transient message for the status bar (a failed save, mostly).
    status: Option<SharedString>,
    /// In-flight background work. Held rather than detached so a new request drops the
    /// old one, which is how ADR-0007's cancellation actually happens.
    pending: Option<Task<()>>,
    /// Frame pacing, measured at the window root so it sees every repaint.
    frames: FrameTimer,
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
            status: None,
            pending: None,
            frames: FrameTimer::new(),
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

        self.pending = Some(cx.spawn(async move |this, cx| {
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
        }));
    }

    fn toggle_hidden_files(&mut self, _: &ToggleHiddenFiles, _w: &mut Window, cx: &mut Context<Self>) {
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

    /// Opens a file in a tab, or activates the tab already showing it.
    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.path.as_ref() == Some(&path)) {
            self.active_tab = index;
            cx.notify();
            return;
        }

        let load_path = path.clone();
        self.pending = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { read_file(&load_path) })
                .await;

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
        }));
    }

    // --- saving ------------------------------------------------------------------

    fn save(&mut self, _: &Save, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let Some(path) = tab.path.clone() else {
            // ponytail: no save-as dialog yet. `prompt_for_new_path` exists; wire it when
            // creating new files is a real flow (nothing creates an untitled buffer today).
            self.status = Some("this buffer has no path".into());
            cx.notify();
            return;
        };

        let editor = tab.editor.clone();
        let text = editor.read(cx).document.text_for_save();

        self.pending = Some(cx.spawn(async move |this, cx| {
            let written = cx
                .background_spawn(async move { write_file(&path, &text) })
                .await;

            this.update(cx, |this, cx| {
                match written {
                    Ok(()) => {
                        // Only clear dirty state after the write actually succeeded.
                        editor.update(cx, |editor, _| editor.document.buffer.mark_saved());
                        this.status = None;
                    }
                    // The buffer is untouched on failure, so the user loses nothing.
                    Err(err) => this.status = Some(format!("save failed: {err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn close_tab(&mut self, _: &CloseTab, _w: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        // ponytail: closes a dirty tab without prompting. Add the confirm dialog when
        // there is a modal system to host it (Milestone 1 task 13 brings the overlay).
        self.tabs.remove(self.active_tab);
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
        cx.notify();
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

    fn toggle_quick_open(&mut self, _: &ToggleQuickOpen, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_palette(PaletteMode::Files, window, cx);
    }

    fn toggle_palette(&mut self, mode: PaletteMode, window: &mut Window, cx: &mut Context<Self>) {
        // Same mode reopened means "dismiss"; a different mode swaps the contents.
        if self.palette.as_ref().is_some_and(|p| p.read(cx).mode() == mode) {
            self.dismiss_palette(window, cx);
            return;
        }

        let items = match mode {
            PaletteMode::Commands => self
                .registry
                .all()
                .iter()
                .map(|command| (command.title.to_string(), command.id.0.to_string()))
                .collect(),
            PaletteMode::Files => self.tree_file_items(),
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
        self.palette = Some(palette);
        cx.notify();
    }

    /// Files currently *visible* in the tree, as quick-open candidates.
    ///
    /// ponytail: only expanded rows, not a recursive walk of the project. A real quick
    /// open needs a background, cancellable directory walk feeding an index — that is
    /// Milestone 1 task 13, and doing it here would block the UI on a large project.
    fn tree_file_items(&self) -> Vec<(String, String)> {
        self.tree
            .as_ref()
            .map(|tree| {
                tree.entries()
                    .iter()
                    .filter(|entry| !entry.is_dir())
                    .map(|entry| (entry.name.clone(), entry.path.display().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn dismiss_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                    Dispatch::Save => self.save(&Save, window, cx),
                    Dispatch::CloseTab => self.close_tab(&CloseTab, window, cx),
                    Dispatch::QuickOpen => self.toggle_palette(PaletteMode::Files, window, cx),
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
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::toggle_quick_open))
            .on_action(cx.listener(Self::toggle_hidden_files))
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
                            .child(self.render_editor_area(&theme)),
                    ),
            )
            .child(self.render_status_bar(&theme, cx))
            .children(self.palette.clone().map(|palette| {
                // The overlay is absolutely positioned over everything, so it does not
                // reflow the layout underneath while it is open.
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .justify_center()
                    .child(palette)
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
            .w(px(44.0))
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
                                    format!("{} {}", if entry.expanded { "▾" } else { "▸" }, entry.name)
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
                        div()
                            .text_color(if dirty { theme.accent } else { theme.text_muted })
                            .child(if dirty { "•" } else { "" }),
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
            .child(SharedString::from(position))
            .child(SharedString::from(language))
    }
}
