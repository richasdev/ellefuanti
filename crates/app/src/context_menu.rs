//! The file tree's right-click menu, and the two prompts its actions need.
//!
//! # What this is for
//!
//! #126, reported as *"não dá pra apertar click direito … pra alterar ou apagar"*. Before
//! this there was no way to rename, delete or create a file from the tree at all — not a
//! bug, a feature that was never built, and the last one keeping the tree a read-only view
//! of a folder rather than a way to work in it.
//!
//! # Why a menu primitive lives here and not in gpui
//!
//! gpui has a menu API, but it drives the **system menu bar** (#78) — a `Menu` handed to
//! the platform at startup. It cannot be opened at a mouse position, cannot be built from
//! the row that was clicked, and cannot be dismissed by clicking elsewhere in the window.
//! An in-window context menu is a different thing that happens to share a name, so this is
//! ~70 lines of `div`s rather than an attempt to bend the platform one into shape.
//!
//! # The three overlays, and why they are one module
//!
//! A menu, a name prompt and a confirmation are the same shape: a small floating panel that
//! takes the keyboard, emits one event and goes away. Splitting them across three files
//! would triple the focus-and-dismiss wiring, which is the only part with any subtlety in
//! it. They share `Overlay` and differ in what they render.
//!
//! # Destructive actions
//!
//! Delete is the one action in this editor a user cannot undo — there is no trash and no
//! journal (see `elle_workspace::delete`). So it is the only entry that opens a
//! confirmation, the confirmation names the file rather than saying "this item", and it
//! says *permanently*, because on a directory it is a recursive delete and the word is the
//! only warning the user gets.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, Pixels, Point,
    SharedString, Window, div, prelude::*, px,
};

use crate::actions::{Backspace, Cancel, Confirm, SelectNext, SelectPrev, context};
use crate::theme::{Metrics, Themed};

/// What the user picked from the menu.
///
/// A path is carried on every variant rather than kept in the workspace between the click
/// and the confirm. The tree can be refreshed by anything — a save, a git status poll — and
/// an index remembered across an await can point at a different row by the time it is used.
/// The path is what the user actually clicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    NewFile,
    NewDirectory,
    Rename,
    Delete,
    RevealInFinder,
    CopyPath,
}

impl MenuAction {
    fn label(&self) -> &'static str {
        match self {
            Self::NewFile => "New File",
            Self::NewDirectory => "New Folder",
            Self::Rename => "Rename…",
            Self::Delete => "Delete",
            Self::RevealInFinder => "Reveal in Finder",
            Self::CopyPath => "Copy Path",
        }
    }

    /// Whether this action destroys something, which is what earns a confirmation and the
    /// one bit of colour in the menu.
    pub fn is_destructive(&self) -> bool {
        matches!(self, Self::Delete)
    }
}

/// The entries offered for a row, which depend on what it is.
///
/// A directory gets the creation actions because "new file" means "new file *in here*", and
/// a file does not: creating a sibling from a file row is a guess about intent, and the
/// directory the user wants is always one row up and visible.
/// The entries offered for the project root, reached by right-clicking the tree's empty
/// space — the only way to create something at the top level, since the root has no row.
///
/// No rename and no delete: the root is the open project itself. `fs::delete` refuses it
/// anyway, but an entry that always errors is worse than no entry — it reads as broken
/// rather than as impossible.
pub fn actions_for_root() -> Vec<MenuAction> {
    vec![
        MenuAction::NewFile,
        MenuAction::NewDirectory,
        MenuAction::RevealInFinder,
        MenuAction::CopyPath,
    ]
}

pub fn actions_for(is_dir: bool) -> Vec<MenuAction> {
    let mut actions = Vec::new();
    if is_dir {
        actions.push(MenuAction::NewFile);
        actions.push(MenuAction::NewDirectory);
    }
    actions.push(MenuAction::Rename);
    actions.push(MenuAction::Delete);
    actions.push(MenuAction::RevealInFinder);
    actions.push(MenuAction::CopyPath);
    actions
}

/// What an overlay reports back to the workspace.
pub enum OverlayEvent {
    /// A menu entry was chosen.
    Picked(MenuAction),
    /// A name was typed and confirmed — for a rename, a new file or a new directory.
    Named(String),
    /// A destructive action was confirmed.
    Accepted,
    Dismissed,
}

/// Which of the three things this overlay currently is.
enum Mode {
    Menu {
        entries: Vec<MenuAction>,
    },
    /// A one-line text field. `verb` is what the confirm button would say if there were
    /// one, and doubles as the title.
    Prompt {
        verb: SharedString,
        subject: SharedString,
        text: String,
    },
    /// A yes/no about something that cannot be undone.
    Confirm {
        question: SharedString,
        detail: SharedString,
    },
}

/// A small floating panel that owns the keyboard until it is answered.
pub struct Overlay {
    mode: Mode,
    focus_handle: FocusHandle,
    selected: usize,
    /// Where to draw, in window coordinates. The click position for a menu; ignored by the
    /// prompt and the confirmation, which centre themselves.
    origin: Point<Pixels>,
}

impl EventEmitter<OverlayEvent> for Overlay {}

impl Overlay {
    /// A context menu at the mouse position.
    pub fn menu(entries: Vec<MenuAction>, origin: Point<Pixels>, cx: &mut Context<Self>) -> Self {
        Self { mode: Mode::Menu { entries }, focus_handle: cx.focus_handle(), selected: 0, origin }
    }

    /// A name prompt, pre-filled with `initial` — the current name for a rename, empty for
    /// a creation.
    ///
    /// Pre-filling matters for rename: retyping `RegistersUserController.php` to change one
    /// letter is what makes a rename box feel broken.
    pub fn prompt(
        verb: impl Into<SharedString>,
        subject: impl Into<SharedString>,
        initial: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            mode: Mode::Prompt { verb: verb.into(), subject: subject.into(), text: initial.into() },
            focus_handle: cx.focus_handle(),
            selected: 0,
            origin: Point::default(),
        }
    }

    /// A confirmation for something irreversible.
    pub fn confirm(
        question: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            mode: Mode::Confirm { question: question.into(), detail: detail.into() },
            focus_handle: cx.focus_handle(),
            // Starts on "Cancel". A confirmation that opens with the destructive option
            // selected turns a reflexive Enter into a deletion, which is the accident the
            // dialog exists to prevent.
            selected: 1,
            origin: Point::default(),
        }
    }

    /// The menu's entries, or `None` if this overlay is a prompt or a confirmation.
    #[cfg(test)]
    pub fn entries_for_test(&self) -> Option<Vec<MenuAction>> {
        match &self.mode {
            Mode::Menu { entries } => Some(entries.clone()),
            _ => None,
        }
    }

    /// Whether this overlay centres itself rather than drawing at a click position.
    ///
    /// A menu belongs at the mouse — that is what makes it a context menu. A prompt and a
    /// confirmation are about the whole window and belong where the user is already
    /// looking, which is the same reasoning the palette follows.
    pub fn is_centred(&self) -> bool {
        !matches!(self.mode, Mode::Menu { .. })
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let Mode::Prompt { text, .. } = &mut self.mode else { return };

        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform
            || keystroke.modifiers.control
            || keystroke.modifiers.function
        {
            return;
        }
        if matches!(
            keystroke.key.as_str(),
            "enter" | "escape" | "up" | "down" | "backspace" | "tab" | "left" | "right"
        ) {
            return;
        }
        let Some(typed) = keystroke.key_char.as_deref() else { return };
        if typed.is_empty() || typed.chars().all(|c| c.is_control()) {
            return;
        }

        text.push_str(typed);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        if let Mode::Prompt { text, .. } = &mut self.mode {
            text.pop();
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, _w: &mut Window, cx: &mut Context<Self>) {
        let count = self.option_count();
        if count > 0 {
            self.selected = (self.selected + 1) % count;
            cx.notify();
        }
    }

    fn select_prev(&mut self, _: &SelectPrev, _w: &mut Window, cx: &mut Context<Self>) {
        let count = self.option_count();
        if count > 0 {
            self.selected = if self.selected == 0 { count - 1 } else { self.selected - 1 };
            cx.notify();
        }
    }

    /// How many things arrow keys move between.
    fn option_count(&self) -> usize {
        match &self.mode {
            Mode::Menu { entries } => entries.len(),
            // Delete and Cancel.
            Mode::Confirm { .. } => 2,
            // A text field has nothing to move between.
            Mode::Prompt { .. } => 0,
        }
    }

    fn confirm_action(&mut self, _: &Confirm, _w: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Menu { entries } => {
                if let Some(action) = entries.get(self.selected) {
                    cx.emit(OverlayEvent::Picked(action.clone()));
                }
            }
            Mode::Prompt { text, .. } => {
                // An empty name is refused here rather than sent on to fail in the file
                // layer, so the message names the field the user is looking at.
                if text.trim().is_empty() {
                    cx.emit(OverlayEvent::Dismissed);
                } else {
                    cx.emit(OverlayEvent::Named(text.clone()));
                }
            }
            Mode::Confirm { .. } => {
                if self.selected == 0 {
                    cx.emit(OverlayEvent::Accepted);
                } else {
                    cx.emit(OverlayEvent::Dismissed);
                }
            }
        }
    }

    fn cancel(&mut self, _: &Cancel, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(OverlayEvent::Dismissed);
    }
}

impl Focusable for Overlay {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Overlay {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let entity = cx.entity();

        let panel = div()
            .key_context(context::PALETTE)
            .track_focus(&self.focus_handle(cx))
            // Click anywhere else to dismiss, which is what every context menu on the
            // platform does and the only way to close one without reaching for Escape.
            //
            // `on_mouse_down_out` rather than a full-window box behind the panel with its
            // own handler: that box is the panel's *parent*, and gpui delivers mouse-down
            // from the inside out, so a click on an entry would run the entry's handler and
            // then the parent's dismiss — which clears `pending` before the action it just
            // emitted has been handled. The menu would close having done nothing, which is
            // indistinguishable from the feature not working. This fires only for clicks
            // that miss the panel, so there is no ordering to get right.
            .on_mouse_down_out(cx.listener(|_this, _ev: &gpui::MouseDownEvent, _window, cx| {
                cx.emit(OverlayEvent::Dismissed);
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::confirm_action))
            .on_action(cx.listener(Self::cancel))
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .rounded(px(6.0))
            .py_1()
            .text_color(theme.text);

        match &self.mode {
            Mode::Menu { entries } => {
                let selected = self.selected;
                panel.absolute().left(self.origin.x).top(self.origin.y).w(px(200.0)).children(
                    entries.iter().enumerate().map(|(index, action)| {
                        let entity = entity.clone();
                        let action_for_click = action.clone();
                        div()
                            .id(("menu-entry", index))
                            .flex()
                            .items_center()
                            .h(Metrics::ROW_HEIGHT)
                            .px_3()
                            .cursor_pointer()
                            .when(index == selected, |el| el.bg(theme.selected))
                            .hover(|el| el.bg(theme.hover))
                            // The one coloured entry, and the colour is not the only
                            // signal: it is also the only one that opens a confirmation.
                            .when(action.is_destructive(), |el| el.text_color(theme.error))
                            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                entity.update(cx, |_this, cx| {
                                    cx.emit(OverlayEvent::Picked(action_for_click.clone()));
                                });
                            })
                            .child(SharedString::from(action.label()))
                    }),
                )
            }
            Mode::Prompt { verb, subject, text } => {
                let shown = if text.is_empty() {
                    SharedString::from("…")
                } else {
                    SharedString::from(text.clone())
                };
                panel.w(px(420.0)).px_3().py_2().child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_color(theme.text_muted)
                                .child(SharedString::from(format!("{verb} {subject}"))),
                        )
                        .child(
                            div()
                                .w_full()
                                .px_2()
                                .py_1()
                                .bg(theme.status_bar)
                                .border_1()
                                .border_color(theme.accent)
                                .rounded(px(4.0))
                                .when(text.is_empty(), |el| el.text_color(theme.text_muted))
                                .child(shown),
                        )
                        .child(
                            div()
                                .text_color(theme.text_muted)
                                .child(SharedString::from("Enter to confirm, Esc to cancel")),
                        ),
                )
            }
            Mode::Confirm { question, detail } => {
                let selected = self.selected;
                panel.w(px(420.0)).px_3().py_2().child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().child(question.clone()))
                        .child(div().text_color(theme.text_muted).child(detail.clone()))
                        .child(div().flex().gap_2().children(
                            [(0usize, "Delete"), (1usize, "Cancel")].map(|(index, label)| {
                                let entity = entity.clone();
                                div()
                                    .id(("confirm-option", index))
                                    .px_3()
                                    .py_1()
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .border_1()
                                    // The selected button carries a fill as well as the
                                    // accent border, so which button Enter would press does
                                    // not rest on the border colour alone — the house rule
                                    // that nothing is signalled by colour alone, and cheap
                                    // insurance on a theme where `accent` sits close to
                                    // `border`. `theme.selected` is the same fill the
                                    // palette and completion list use for their selection.
                                    .when(index == selected, |el| el.bg(theme.selected))
                                    .border_color(if index == selected {
                                        theme.accent
                                    } else {
                                        theme.border
                                    })
                                    .when(index == 0, |el| el.text_color(theme.error))
                                    .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                        entity.update(cx, |_this, cx| {
                                            cx.emit(if index == 0 {
                                                OverlayEvent::Accepted
                                            } else {
                                                OverlayEvent::Dismissed
                                            });
                                        });
                                    })
                                    .child(SharedString::from(label))
                            }),
                        )),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_offers_creation_and_a_file_does_not() {
        // "New file" from a file row means "a sibling", which is a guess about intent —
        // and the directory the user wants is always one row up and already visible.
        let on_dir = actions_for(true);
        assert!(on_dir.contains(&MenuAction::NewFile));
        assert!(on_dir.contains(&MenuAction::NewDirectory));

        let on_file = actions_for(false);
        assert!(!on_file.contains(&MenuAction::NewFile));
        assert!(!on_file.contains(&MenuAction::NewDirectory));
    }

    #[test]
    fn both_kinds_can_be_renamed_and_deleted() {
        // The two the report actually asked for: "alterar ou apagar".
        for is_dir in [true, false] {
            let actions = actions_for(is_dir);
            assert!(actions.contains(&MenuAction::Rename), "rename must be offered");
            assert!(actions.contains(&MenuAction::Delete), "delete must be offered");
        }
    }

    #[test]
    fn the_root_offers_creation_but_never_destruction() {
        // The root is the open project. Renaming or deleting it from inside itself is
        // refused by the fs layer, and an entry that always errors reads as broken.
        let actions = actions_for_root();
        assert!(actions.contains(&MenuAction::NewFile), "the root is where new files start");
        assert!(!actions.contains(&MenuAction::Delete));
        assert!(!actions.contains(&MenuAction::Rename));
    }

    #[test]
    fn delete_is_the_only_destructive_entry() {
        // What earns a confirmation. If another action ever becomes irreversible it has to
        // be added here deliberately, rather than inheriting silence by default.
        let destructive: Vec<_> =
            actions_for(true).into_iter().filter(MenuAction::is_destructive).collect();
        assert_eq!(destructive, vec![MenuAction::Delete]);
    }
}
