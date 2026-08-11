//! The command palette and quick open, which are the same overlay over different lists.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, SharedString,
    Window, div, prelude::*, px, uniform_list,
};

use crate::actions::{Backspace, Cancel, Confirm, SelectNext, SelectPrev, context};
use crate::fonts::Fonts;
use crate::theme::{Metrics, Themed};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaletteMode {
    Commands,
    Files,
    Routes,
    /// Symbols in the active file, from the language server.
    Symbols,
    /// Where a symbol is used, from the language server.
    References,
    /// The project's artisan commands (#23). Confirming types the command into the
    /// terminal; nothing executes out of sight.
    Artisan,
    /// The project's composer scripts (#26). Confirming TYPES the run-script command
    /// into the terminal — the #146 rule, like artisan.
    ComposerScripts,
    /// Local branches (#64). Confirming switches — with the crate-level dirty-tree
    /// refusal as the guard, not a prompt.
    Branches,
    /// The quick fixes the server offered at the cursor (#19). Items are known at open
    /// (the request ran before the palette appeared); the id is the action's index.
    CodeActions,
    /// A single-input prompt for a symbol's new name (#19). The only mode with no list:
    /// Confirm emits the *query* — the typed name is the answer, not a row.
    Rename,
    /// Symbols across the whole project, from the language server (#19). The server is
    /// the matcher — every keystroke re-asks it — so this mode does NOT filter locally:
    /// the server's fuzzy match (camel humps, mid-word) would be second-guessed by a
    /// subsequence scan and real hits would vanish from the list.
    WorkspaceSymbols,
    /// The syntax language for the current buffer (#127).
    ///
    /// Not a navigation like the others — it changes the active document rather than
    /// opening something — but it is a filtered list of a few dozen fixed choices, which is
    /// exactly what this overlay is. A dedicated dropdown would be a second implementation
    /// of the same keyboard and filtering behaviour.
    Languages,
}

impl PaletteMode {
    fn placeholder(&self) -> &'static str {
        match self {
            PaletteMode::Commands => "Run a command…",
            PaletteMode::Files => "Open a file…",
            PaletteMode::Routes => "Go to a route…",
            PaletteMode::Symbols => "Go to a symbol…",
            PaletteMode::References => "Go to a usage…",
            PaletteMode::Artisan => "Type an artisan command into the terminal…",
            PaletteMode::WorkspaceSymbols => "Go to a symbol in the project…",
            PaletteMode::Rename => "New name…",
            PaletteMode::CodeActions => "Apply a fix…",
            PaletteMode::Branches => "Switch to a branch…",
            PaletteMode::ComposerScripts => "Type a composer script into the terminal…",
            PaletteMode::Languages => "Set the language…",
        }
    }
}

/// What the palette tells the workspace when the user acts.
pub enum PaletteEvent {
    /// The id (command id, or file path) that was confirmed.
    Confirmed(String),
    /// The query changed. Only live-source modes (workspace symbols) act on this; the
    /// static modes filter what they already have and ignore it.
    QueryChanged(String),
    Dismissed,
}

/// One row: what the user reads, and what gets returned on confirm.
#[derive(Clone)]
struct Item {
    label: SharedString,
    id: String,
}

pub struct Palette {
    mode: PaletteMode,
    focus_handle: FocusHandle,
    query: String,
    items: Vec<Item>,
    filtered: Vec<Item>,
    selected: usize,
    /// Whether the candidate list is final. Quick open opens empty while a background walk
    /// runs, and "No matches" would be a lie during that window.
    loaded: bool,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Palette {
    /// `items` is `(label, id)`.
    pub fn new(mode: PaletteMode, items: Vec<(String, String)>, cx: &mut Context<Self>) -> Self {
        let items: Vec<Item> =
            items.into_iter().map(|(label, id)| Item { label: label.into(), id }).collect();

        Self {
            mode,
            focus_handle: cx.focus_handle(),
            query: String::new(),
            filtered: items.clone(),
            // Commands are known up front; files arrive later via set_items.
            // Commands and languages are both known up front; files and the rest arrive
            // later via `set_items`, and "No matches" would be a lie until they do.
            loaded: matches!(
                mode,
                PaletteMode::Commands | PaletteMode::Languages | PaletteMode::Rename
            ),
            items,
            selected: 0,
        }
    }

    pub fn mode(&self) -> PaletteMode {
        self.mode
    }

    /// Replaces the candidate list, re-applying whatever the user has already typed.
    ///
    /// Quick open opens empty and fills in when the background walk lands, so by the time
    /// this is called the user may already have typed a query. Re-filtering rather than
    /// resetting is what keeps those keystrokes from being silently discarded.
    pub fn set_items(&mut self, items: Vec<(String, String)>, cx: &mut Context<Self>) {
        self.items =
            items.into_iter().map(|(label, id)| Item { label: label.into(), id }).collect();
        self.loaded = true;
        self.refilter();
        cx.notify();
    }

    /// Re-filters against the query.
    ///
    /// ponytail: a linear subsequence scan over every candidate, on every keystroke. Fine
    /// for a few dozen commands; quick open now indexes a whole project, so this runs over
    /// tens of thousands of paths on the UI thread.
    ///
    /// Measured rather than guessed at, over 50k Laravel-shaped paths: **0.7 ms** for a
    /// one-character query, **2.9 ms** worst case for a long one. Inside the 8.3 ms frame
    /// budget, but a third of it before gpui does any layout — so this is the first thing
    /// to blame if typing in quick open feels heavy on a very large project.
    ///
    /// The upgrade is a real fuzzy matcher (nucleo) with scoring and incremental narrowing
    /// as the query grows. That is a dependency plus a ranking model, not a one-liner, so it
    /// waits for a project big enough to justify it.
    fn refilter(&mut self) {
        // Workspace symbols arrive already matched by the server; see the mode's doc.
        self.filtered = if self.mode == PaletteMode::WorkspaceSymbols {
            self.items.clone()
        } else {
            filter_items(&self.items, &self.query)
        };
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// The rows currently shown, after filtering.
    #[cfg(test)]
    pub fn labels_for_test(&self) -> Vec<String> {
        self.filtered.iter().map(|item| item.label.to_string()).collect()
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform
            || keystroke.modifiers.control
            || keystroke.modifiers.function
        {
            return;
        }
        // Navigation, confirm and dismiss are actions; only text reaches the query.
        if matches!(
            keystroke.key.as_str(),
            "enter" | "escape" | "up" | "down" | "backspace" | "tab" | "left" | "right"
        ) {
            return;
        }
        let Some(text) = keystroke.key_char.as_deref() else { return };
        if text.is_empty() || text.chars().all(|c| c.is_control()) {
            return;
        }

        self.typed(text, cx);
    }

    /// The shared tail of a keystroke reaching the query — also the test door, because a
    /// `KeyDownEvent` cannot be conjured headlessly.
    fn typed(&mut self, text: &str, cx: &mut Context<Self>) {
        self.query.push_str(text);
        self.refilter();
        cx.emit(PaletteEvent::QueryChanged(self.query.clone()));
        cx.notify();
    }

    #[cfg(test)]
    pub fn type_for_test(&mut self, text: &str, cx: &mut Context<Self>) {
        self.typed(text, cx);
    }

    fn backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        self.query.pop();
        self.refilter();
        cx.emit(PaletteEvent::QueryChanged(self.query.clone()));
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _w: &mut Window, cx: &mut Context<Self>) {
        if !self.filtered.is_empty() {
            // Wraps, because a palette that stops at the bottom is a palette you have to
            // look at while navigating.
            self.selected = (self.selected + 1) % self.filtered.len();
            cx.notify();
        }
    }

    fn select_prev(&mut self, _: &SelectPrev, _w: &mut Window, cx: &mut Context<Self>) {
        if !self.filtered.is_empty() {
            self.selected =
                if self.selected == 0 { self.filtered.len() - 1 } else { self.selected - 1 };
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &Confirm, _w: &mut Window, cx: &mut Context<Self>) {
        self.confirm_inner(cx);
    }

    fn confirm_inner(&mut self, cx: &mut Context<Self>) {
        // Rename has no rows; the typed name is the answer. An empty name is a rename
        // to nothing, which is not a thing — Escape is how you decline.
        if self.mode == PaletteMode::Rename {
            if !self.query.trim().is_empty() {
                cx.emit(PaletteEvent::Confirmed(self.query.trim().to_string()));
            }
            return;
        }
        if let Some(item) = self.filtered.get(self.selected) {
            cx.emit(PaletteEvent::Confirmed(item.id.clone()));
        }
    }

    /// Pre-fills the input — the rename prompt opens holding the current name, because
    /// most renames edit a word rather than retype it.
    pub fn preset_query(&mut self, text: &str, cx: &mut Context<Self>) {
        self.query = text.to_string();
        self.refilter();
        cx.notify();
    }

    #[cfg(test)]
    pub fn confirm_for_test(&mut self, cx: &mut Context<Self>) {
        self.confirm_inner(cx);
    }

    fn cancel(&mut self, _: &Cancel, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PaletteEvent::Dismissed);
    }
}

impl Focusable for Palette {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Palette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let entity = cx.entity();
        let count = self.filtered.len();
        let selected = self.selected;
        let query_shown = if self.query.is_empty() {
            SharedString::from(self.mode.placeholder())
        } else {
            SharedString::from(self.query.clone())
        };
        let query_is_placeholder = self.query.is_empty();

        div()
            .key_context(context::PALETTE)
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .mt(px(80.0))
            .w(px(520.0))
            .max_h(px(420.0))
            .flex()
            .flex_col()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .rounded_lg()
            .shadow_lg()
            .text_size(Fonts::get(cx).ui_size)
            .text_color(theme.text)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(38.0))
                    .px_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .when(query_is_placeholder, |el| el.text_color(theme.text_muted))
                    .child(query_shown),
            )
            .child(if count == 0 {
                div()
                    .p_3()
                    .text_color(theme.text_muted)
                    .child(if self.loaded { "No matches" } else { "Searching…" })
                    .into_any_element()
            } else {
                uniform_list("palette-items", count, move |range, _window, cx| {
                    entity.update(cx, |palette, _cx| {
                        range
                            .filter_map(|index| {
                                let item = palette.filtered.get(index)?.clone();
                                let entity = entity.clone();
                                Some(
                                    div()
                                        .id(("palette-row", index))
                                        .flex()
                                        .items_center()
                                        .h(Metrics::ROW_HEIGHT)
                                        .px_3()
                                        .cursor_pointer()
                                        .when(index == selected, |el| el.bg(theme.selected))
                                        .hover(|el| el.bg(theme.hover))
                                        .active(|el| el.bg(theme.pressed))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            move |_ev, _window, cx| {
                                                entity.update(cx, |palette, cx| {
                                                    palette.selected = index;
                                                    cx.emit(PaletteEvent::Confirmed(
                                                        palette.filtered[index].id.clone(),
                                                    ));
                                                });
                                            },
                                        )
                                        .child(item.label.clone())
                                        .into_any_element(),
                                )
                            })
                            .collect()
                    })
                })
                .flex_1()
                .into_any_element()
            })
    }
}

/// Candidates matching `query`, in their original order.
///
/// Free function rather than a method so it is testable without constructing a gpui app —
/// the filtering is the part that can actually be wrong.
fn filter_items(items: &[Item], query: &str) -> Vec<Item> {
    if query.is_empty() {
        return items.to_vec();
    }
    items
        .iter()
        // Matching the id as well as the label lets a user type a path fragment that the
        // displayed label does not show.
        .filter(|item| subsequence(&item.label, query) || subsequence(&item.id, query))
        .cloned()
        .collect()
}

/// Case-insensitive subsequence match, the same rule `CommandRegistry::search` uses.
fn subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars().map(|c| c.to_ascii_lowercase());
    needle
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|want| chars.any(|c| c == want.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_matches_scattered_characters() {
        assert!(subsequence("editor.save", "esave"));
        assert!(subsequence("Save File", "sf"));
        assert!(subsequence("Save File", "SAVE"));
        assert!(!subsequence("Save File", "xyz"));
        assert!(subsequence("anything", ""));
    }

    #[test]
    fn subsequence_requires_order() {
        assert!(subsequence("abc", "ac"));
        assert!(!subsequence("abc", "ca"));
    }

    #[test]
    fn subsequence_matches_nested_paths_the_way_quick_open_needs() {
        // Quick open's real query shape: initials scattered across path segments.
        assert!(subsequence("app/Models/User.php", "user"));
        assert!(subsequence("app/Models/User.php", "amu"));
        assert!(subsequence("app/Http/Controllers/UserController.php", "uctrl"));
        assert!(subsequence("resources/views/welcome.blade.php", "welcome"));
        // A path that simply lacks a letter must not match — the case that made an
        // earlier benchmark of this function silently report zero hits for every query.
        assert!(!subsequence("app/Models/Post.php", "z"));
    }

    fn items(pairs: &[(&str, &str)]) -> Vec<Item> {
        pairs
            .iter()
            // `to_string` first: SharedString borrows for 'static, and a &str from a test
            // slice does not live that long.
            .map(|(label, id)| Item { label: label.to_string().into(), id: id.to_string() })
            .collect()
    }

    #[test]
    fn an_empty_query_keeps_every_candidate_in_order() {
        let all = items(&[("b.php", "/b.php"), ("a.php", "/a.php")]);
        let filtered = filter_items(&all, "");
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].label, "b.php", "order must be preserved, not sorted");
    }

    #[test]
    fn filtering_matches_the_label_or_the_hidden_id() {
        // Quick open labels are project-relative paths but the id is absolute; a user
        // typing a directory that only appears in the id should still find the file.
        let all = items(&[
            ("User.php", "/home/me/proj/app/Models/User.php"),
            ("Post.php", "/home/me/proj/app/Models/Post.php"),
        ]);

        assert_eq!(filter_items(&all, "user").len(), 1);
        assert_eq!(filter_items(&all, "models").len(), 2, "id-only match must still hit");
        assert!(filter_items(&all, "zzz").is_empty());
    }

    #[test]
    fn filtering_a_large_candidate_list_stays_correct() {
        // The scale quick open now operates at, guarding against an off-by-one or an
        // early return that only shows up past a few hundred entries.
        let pairs: Vec<(String, String)> = (0..5_000)
            .map(|i| (format!("File{i}.php"), format!("/proj/app/File{i}.php")))
            .collect();
        let all: Vec<Item> = pairs
            .iter()
            .map(|(label, id)| Item { label: label.clone().into(), id: id.clone() })
            .collect();

        assert_eq!(all.len(), 5_000);
        assert_eq!(filter_items(&all, "").len(), 5_000);
        // "file4242" matches exactly one; the subsequence rule also admits scattered hits.
        assert!(filter_items(&all, "file4242").iter().any(|i| i.label == "File4242.php"));
        assert!(filter_items(&all, "qqqq").is_empty());
    }
}
