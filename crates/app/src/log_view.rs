//! The Laravel log panel (#25): structured entries, newest first, click to the throw site.
//!
//! A viewer, not a tail: the file is read when the panel opens and again on window
//! refocus while it is up — the same no-timer discipline as the git panel (#64). The
//! parse is `elle_laravel::parse_laravel_log`'s, and the click target is the first real
//! stack frame, which is the throw site — both decided in the domain crate, tested
//! there; this file is rows and a jump.

use gpui::{
    App, Context, MouseButton, SharedString, Window, div, prelude::*, px, uniform_list,
};

use crate::fonts::Fonts;
use crate::theme::{Metrics, Themed};

type JumpHandler = Box<dyn Fn(&std::path::Path, u32, &mut Window, &mut App)>;

pub struct LogView {
    /// Newest first — the panel answers "what just went wrong".
    entries: Vec<elle_laravel::LogEntry>,
    /// The log file the entries came from, for the header.
    source: Option<SharedString>,
    on_jump: Option<JumpHandler>,
}

impl LogView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self { entries: Vec::new(), source: None, on_jump: None }
    }

    pub fn on_jump(&mut self, jump: impl Fn(&std::path::Path, u32, &mut Window, &mut App) + 'static) {
        self.on_jump = Some(Box::new(jump));
    }

    /// Replaces the entries, newest first, and remembers where they came from.
    pub fn set_entries(
        &mut self,
        mut entries: Vec<elle_laravel::LogEntry>,
        source: Option<String>,
        cx: &mut Context<Self>,
    ) {
        entries.reverse();
        self.entries = entries;
        self.source = source.map(SharedString::from);
        cx.notify();
    }

    #[cfg(test)]
    pub fn entries_for_test(&self) -> &[elle_laravel::LogEntry] {
        &self.entries
    }

    /// A row's click, through the real handler.
    #[cfg(test)]
    pub fn jump_for_test(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.jump(index, window, cx);
    }

    fn jump(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index) else { return };
        let Some((path, line)) = entry.target.clone() else { return };
        if let Some(jump) = &self.on_jump {
            jump(&path, line, window, cx);
        }
    }
}

impl Render for LogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fonts = Fonts::get(cx);
        let entity = cx.entity();
        let count = self.entries.len();

        let header = match (&self.source, count) {
            (Some(source), _) => SharedString::from(format!("LOG · {source}")),
            (None, _) => SharedString::from("LOG"),
        };

        div()
            .h(px(180.0))
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.panel)
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .h(Metrics::TAB_HEIGHT)
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .text_color(theme.text_muted)
                    .child(header),
            )
            .child(if count == 0 {
                div()
                    .p_3()
                    .text_color(theme.text_muted)
                    .child("No log entries")
                    .into_any_element()
            } else {
                let row_height = fonts.line_height();
                uniform_list("log-rows", count, move |range, _window, cx| {
                    entity.update(cx, |this, cx| {
                        let theme = cx.theme().clone();
                        range
                            .filter_map(|index| this.entries.get(index).map(|e| (index, e.clone())))
                            .map(|(index, entry)| {
                                let entity = cx.entity();
                                let has_target = entry.target.is_some();
                                let label = format!(
                                    "{}  {}  {}{}",
                                    entry.level,
                                    entry.timestamp,
                                    entry.message,
                                    // Text, not colour: the marker that a row can jump.
                                    if has_target { "  ↩" } else { "" }
                                );
                                div()
                                    .id(("log-row", index))
                                    .h(row_height)
                                    .px_3()
                                    .text_color(if entry.level == "ERROR" {
                                        theme.error
                                    } else {
                                        theme.text
                                    })
                                    .hover(|el| el.bg(theme.hover))
                                    .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                                        entity.update(cx, |this, cx| this.jump(index, window, cx));
                                    })
                                    .child(SharedString::from(label))
                                    .into_any_element()
                            })
                            .collect()
                    })
                })
                .h_full()
                .text_size(fonts.size)
                .into_any_element()
            })
    }
}
