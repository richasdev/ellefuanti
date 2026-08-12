//! A one-line text tooltip, because gpui 0.2.2 ships the `.tooltip()` hook but no view
//! to hand it — Zed's `ui::Tooltip` lives in a crate we do not depend on.
//!
//! The owner's report: hovering an activity-bar icon says nothing, so the icons are a
//! guessing game. `.tooltip(Tooltip::text("Explorer"))` on any element fixes that with
//! the framework's own hover timing and positioning.

use gpui::{
    App, AppContext, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, px,
};

use crate::theme::Themed;

/// A tooltip that shows a single line of text.
pub struct Tooltip {
    text: SharedString,
}

impl Tooltip {
    /// A builder for `.tooltip(...)`: `el.tooltip(Tooltip::text("Explorer"))`.
    pub fn text(
        text: impl Into<SharedString>,
    ) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
        let text = text.into();
        move |_window, cx| cx.new(|_cx| Tooltip { text: text.clone() }).into()
    }

    /// The same one-line card as a plain view — `on_drag` wants an `Entity<impl Render>`
    /// to float under the pointer, and this label is exactly that shape already.
    pub fn new(text: impl Into<SharedString>) -> Self {
        Tooltip { text: text.into() }
    }
}

/// The label an activity-bar icon shows: its name, or the name plus the honest
/// "(coming soon)" for a panel that is not wired yet. Split out so the choice is
/// testable without a window (the tooltip view itself is unverifiable headlessly, #112).
pub fn activity_label(name: &str, enabled: bool) -> String {
    if enabled { name.to_string() } else { format!("{name} (coming soon)") }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // The panel background over the editor, a border to lift it off whatever it
        // covers — the same recipe the palette and hover card use, so the three read as
        // one product rather than three tooltips.
        div()
            .px_2()
            .py_1()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_md()
            .text_color(theme.text)
            .text_size(px(12.0))
            .child(self.text.clone())
    }
}

use gpui::Context;

#[cfg(test)]
mod tests {
    use super::activity_label;

    #[test]
    fn a_disabled_panel_says_coming_soon() {
        assert_eq!(activity_label("Explorer", true), "Explorer");
        assert_eq!(activity_label("Xdebug", false), "Xdebug (coming soon)");
    }
}
