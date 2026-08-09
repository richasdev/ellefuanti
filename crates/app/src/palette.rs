//! The command palette and quick open, which are the same overlay over different lists.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, SharedString,
    Window, div, prelude::*, px, uniform_list,
};

use crate::actions::{Backspace, Cancel, Confirm, SelectNext, SelectPrev, context};
use crate::theme::{Metrics, Theme};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaletteMode {
    Commands,
    Files,
}

impl PaletteMode {
    fn placeholder(&self) -> &'static str {
        match self {
            PaletteMode::Commands => "Run a command…",
            PaletteMode::Files => "Open a file…",
        }
    }
}

/// What the palette tells the workspace when the user acts.
pub enum PaletteEvent {
    /// The id (command id, or file path) that was confirmed.
    Confirmed(String),
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
            items,
            selected: 0,
        }
    }

    pub fn mode(&self) -> PaletteMode {
        self.mode
    }

    /// Re-filters against the query.
    ///
    /// Reuses `CommandRegistry`'s subsequence matcher via a scratch registry so the
    /// palette ranks commands and files identically — one ranking implementation, not two.
    /// ponytail: building a scratch registry per keystroke is an allocation over a list of
    /// dozens; if quick open ever indexes a whole project, lift the matcher out of
    /// `CommandRegistry` into a free function instead of duplicating it here.
    fn refilter(&mut self) {
        if self.query.is_empty() {
            self.filtered = self.items.clone();
        } else {
            self.filtered = self
                .items
                .iter()
                .filter(|item| subsequence(&item.label, &self.query) || subsequence(&item.id, &self.query))
                .cloned()
                .collect();
        }
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.function {
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

        self.query.push_str(text);
        self.refilter();
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        self.query.pop();
        self.refilter();
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
        if let Some(item) = self.filtered.get(self.selected) {
            cx.emit(PaletteEvent::Confirmed(item.id.clone()));
        }
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
        let theme = Theme::dark();
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
            .text_size(Metrics::UI_FONT_SIZE)
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
                div().p_3().text_color(theme.text_muted).child("No matches").into_any_element()
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
                                        .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                            entity.update(cx, |palette, cx| {
                                                palette.selected = index;
                                                cx.emit(PaletteEvent::Confirmed(
                                                    palette.filtered[index].id.clone(),
                                                ));
                                            });
                                        })
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
}
