//! The find/replace bar (#80).
//!
//! Modelled on [`crate::palette::Palette`] — a self-contained widget that reports what the
//! user did as events and knows nothing about what those events mean — but deliberately
//! **not** built on it. A palette is a modal list over a query; a find bar is two text
//! fields, three toggles and four buttons that stays open while you keep editing. Sharing
//! the type would have meant a `PaletteMode` arm whose every handler starts with a mode
//! check, which is the shape a second widget wears when it is pretending to be the first.
//!
//! What *is* reused: the keystroke filter (`on_key_down` ignoring modified keys and named
//! keys, taking only `key_char`), because that is the part that took the palette three
//! tries to get right.
//!
//! It owns no matches. Those live in `Document` (`editor/state.rs`), because they are a
//! fact about the buffer that has to survive this widget losing focus and has to be
//! invalidated when the text changes — see `Document::search`.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, SharedString,
    Window, div, prelude::*, px,
};

use crate::actions::{
    Backspace, Cancel, FindNext, FindPrev, FocusReplaceField, ReplaceAll, ReplaceOne,
    ToggleCaseSensitive, ToggleRegex, ToggleWholeWord, context,
};
use crate::editor::SearchQuery;
use crate::fonts::Fonts;
use crate::theme::{Theme, Themed};

/// Which of the two text fields keystrokes go to.
///
/// A field rather than two `FocusHandle`s because gpui routes actions by *key context*,
/// and both fields want the same one — `enter` means "next match" whichever field you are
/// in. Tracking the target ourselves is one `match` in `on_key_down`; two focus handles
/// would be two `track_focus` calls, two contexts, and a duplicate binding set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Find,
    Replace,
}

/// What the bar tells the workspace.
pub enum FindEvent {
    /// The query changed — rescan and repaint. Sent on every keystroke and toggle.
    QueryChanged,
    /// Move to the next/previous match.
    Navigate {
        forward: bool,
    },
    /// Replace the current match, or all of them.
    ReplaceOne,
    ReplaceAll,
    /// Escape: close the bar and return focus to the editor.
    Dismissed,
}

pub struct FindBar {
    focus_handle: FocusHandle,
    query: SearchQuery,
    replacement: String,
    field: Field,
    /// Whether the replace row is shown. ⌘F opens find-only; ⌘⌥F opens both.
    pub replacing: bool,
    /// `(current, total)` from the document, refreshed by the workspace after every
    /// rescan. Held rather than read live because the bar has no handle on the editor —
    /// same reason `EditorView` is handed its diagnostics.
    status: Status,
}

/// What the count area shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Status {
    /// Nothing typed yet.
    #[default]
    Empty,
    /// `current` is `None` before the first ⌘G — every hit is highlighted but none is
    /// selected, so claiming "1 of 17" would be a lie about where the cursor is.
    Counted { current: Option<usize>, total: usize },
    /// The regex does not parse. Distinct from zero matches, because the fix is different.
    InvalidRegex,
    /// The file is past `find::MAX_SEARCH_BYTES` and was not scanned.
    TooLarge,
}

impl Status {
    /// The text the bar shows, and whether it should read as a problem.
    ///
    /// Split out from the render so the wording — the part a user actually reads — is
    /// assertable without a window.
    pub fn label(&self) -> (SharedString, bool) {
        match self {
            Status::Empty => ("".into(), false),
            Status::Counted { total: 0, .. } => ("No results".into(), true),
            Status::Counted { current: None, total } => (format!("{total} matches").into(), false),
            Status::Counted { current: Some(current), total } => {
                (format!("{current} of {total}").into(), false)
            }
            Status::InvalidRegex => ("Invalid pattern".into(), true),
            Status::TooLarge => ("File too large to search".into(), true),
        }
    }
}

impl EventEmitter<FindEvent> for FindBar {}

impl FindBar {
    pub fn new(replacing: bool, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            query: SearchQuery::default(),
            replacement: String::new(),
            field: Field::Find,
            replacing,
            status: Status::default(),
        }
    }

    pub fn query(&self) -> &SearchQuery {
        &self.query
    }

    /// Types `pattern` into the find field, as `on_key_down` would.
    ///
    /// A test cannot synthesise a `KeyDownEvent` with a `key_char` through gpui's test
    /// harness, so this is the seam. It sets the same field the keystroke path sets and
    /// emits the same event, so what it exercises downstream is the real thing.
    #[cfg(test)]
    pub fn type_query_for_test(&mut self, pattern: &str, cx: &mut Context<Self>) {
        self.query.pattern = pattern.to_string();
        cx.emit(FindEvent::QueryChanged);
        cx.notify();
    }

    #[cfg(test)]
    pub fn set_replacement_for_test(&mut self, replacement: &str) {
        self.replacement = replacement.to_string();
    }

    #[cfg(test)]
    pub fn status_for_test(&self) -> Status {
        self.status
    }

    pub fn replacement(&self) -> &str {
        &self.replacement
    }

    pub fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }

    /// Seeds the find field, typically from the editor's selection.
    ///
    /// ⌘F with text selected searching for that text is the behaviour of every editor,
    /// and the one that makes ⌘F feel instant rather than like a form to fill in. A
    /// multi-line selection is not seeded: the user selected a block, they did not ask to
    /// search for a block.
    pub fn seed(&mut self, text: &str) {
        if !text.is_empty() && !text.contains('\n') {
            self.query.pattern = text.to_string();
        }
    }

    /// Reopening with ⌘F or ⌘⌥F: refocus and select, do not reset.
    ///
    /// The constraint from the issue — "⌘F while open should refocus, not reopen" —
    /// applies to ⌘⌥F too, which additionally reveals the replace row. Nothing here
    /// clears the query, so ⌘F, type, click away, ⌘F resumes where it was.
    pub fn reopen(&mut self, replacing: bool, cx: &mut Context<Self>) {
        if replacing {
            self.replacing = true;
        }
        self.field = Field::Find;
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
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
        let Some(text) = keystroke.key_char.as_deref() else { return };
        if text.is_empty() || text.chars().all(|c| c.is_control()) {
            return;
        }

        match self.field {
            Field::Find => {
                self.query.pattern.push_str(text);
                cx.emit(FindEvent::QueryChanged);
            }
            // The replacement is not searched for, so typing in it changes nothing about
            // the matches and must not trigger a rescan.
            Field::Replace => self.replacement.push_str(text),
        }
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        match self.field {
            Field::Find => {
                self.query.pattern.pop();
                cx.emit(FindEvent::QueryChanged);
            }
            Field::Replace => {
                self.replacement.pop();
            }
        }
        cx.notify();
    }

    fn focus_replace_field(
        &mut self,
        _: &FocusReplaceField,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // ⇥ with the replace row hidden has nowhere to go, so it stays put rather than
        // focusing a field that is not on screen.
        if self.replacing {
            self.field = match self.field {
                Field::Find => Field::Replace,
                Field::Replace => Field::Find,
            };
            cx.notify();
        }
    }

    fn find_next(&mut self, _: &FindNext, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FindEvent::Navigate { forward: true });
    }

    fn find_prev(&mut self, _: &FindPrev, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FindEvent::Navigate { forward: false });
    }

    fn replace_one(&mut self, _: &ReplaceOne, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FindEvent::ReplaceOne);
    }

    fn replace_all(&mut self, _: &ReplaceAll, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FindEvent::ReplaceAll);
    }

    fn cancel(&mut self, _: &Cancel, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FindEvent::Dismissed);
    }

    /// Flips one search option, from either the keyboard or a click.
    ///
    /// Taking the option as a value rather than having three near-identical methods,
    /// because the three differ only in which `bool` they invert and a click handler
    /// needs to reach them without a `Window` in hand.
    pub fn toggle_option(&mut self, option: Option_, cx: &mut Context<Self>) {
        let flag = match option {
            Option_::CaseSensitive => &mut self.query.case_sensitive,
            Option_::WholeWord => &mut self.query.whole_word,
            Option_::Regex => &mut self.query.regex,
        };
        *flag = !*flag;
        cx.emit(FindEvent::QueryChanged);
        cx.notify();
    }

    fn toggle_case_sensitive(
        &mut self,
        _: &ToggleCaseSensitive,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_option(Option_::CaseSensitive, cx);
    }

    fn toggle_whole_word(&mut self, _: &ToggleWholeWord, _w: &mut Window, cx: &mut Context<Self>) {
        self.toggle_option(Option_::WholeWord, cx);
    }

    fn toggle_regex(&mut self, _: &ToggleRegex, _w: &mut Window, cx: &mut Context<Self>) {
        self.toggle_option(Option_::Regex, cx);
    }
}

/// One of the three search options, so a click handler can name which to flip.
///
/// The trailing underscore avoids shadowing `Option`, which this module uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Option_ {
    CaseSensitive,
    WholeWord,
    Regex,
}

impl Focusable for FindBar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FindBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fonts = Fonts::get(cx);

        div()
            .key_context(context::FIND)
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::focus_replace_field))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_prev))
            .on_action(cx.listener(Self::replace_one))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::toggle_case_sensitive))
            .on_action(cx.listener(Self::toggle_whole_word))
            .on_action(cx.listener(Self::toggle_regex))
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .text_size(fonts.ui_size)
            .text_color(theme.text)
            .child(self.render_find_row(&theme, cx))
            .when(self.replacing, |el| el.child(self.render_replace_row(&theme, cx)))
    }
}

impl FindBar {
    fn render_find_row(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, is_problem) = self.status.label();

        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(26.0))
            .child(text_field(&self.query.pattern, "Find", self.field == Field::Find, theme))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.render_toggle(
                        "Aa",
                        Option_::CaseSensitive,
                        self.query.case_sensitive,
                        theme,
                        cx,
                    ))
                    .child(self.render_toggle(
                        "ab",
                        Option_::WholeWord,
                        self.query.whole_word,
                        theme,
                        cx,
                    ))
                    .child(self.render_toggle(".*", Option_::Regex, self.query.regex, theme, cx)),
            )
            .child(
                div()
                    .min_w(px(90.0))
                    .text_color(if is_problem { theme.error } else { theme.text_muted })
                    .child(status),
            )
            .child(button("↑", theme, {
                let entity = cx.entity();
                move |cx| entity.update(cx, |_, cx| cx.emit(FindEvent::Navigate { forward: false }))
            }))
            .child(button("↓", theme, {
                let entity = cx.entity();
                move |cx| entity.update(cx, |_, cx| cx.emit(FindEvent::Navigate { forward: true }))
            }))
            .child(button("✕", theme, {
                let entity = cx.entity();
                move |cx| entity.update(cx, |_, cx| cx.emit(FindEvent::Dismissed))
            }))
    }

    /// A toggle for one search option.
    ///
    /// The active state is a background *and* a text colour change, not a colour alone —
    /// the same non-colour second channel #71 required of the disabled activity-bar
    /// icons. A user who cannot separate `accent` from `text` still sees the filled box.
    ///
    /// The labels are VS Code's (`Aa`, `ab`, `.*`). ponytail: no tooltips. gpui has the
    /// API, but it needs a `Tooltip` view per control and the bar has six; the labels are
    /// what a user recognises anyway.
    fn render_toggle(
        &self,
        label: &'static str,
        option: Option_,
        active: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .id(label)
            .size(px(20.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .when(active, |el| el.bg(theme.selected).text_color(theme.accent))
            .when(!active, |el| el.text_color(theme.text_muted))
            .hover(|el| el.bg(theme.hover))
            .active(|el| el.bg(theme.pressed))
            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                entity.update(cx, |bar, cx| bar.toggle_option(option, cx));
            })
            .child(SharedString::from(label))
    }

    fn render_replace_row(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(26.0))
            .child(text_field(&self.replacement, "Replace", self.field == Field::Replace, theme))
            .child(button("Replace", theme, {
                let entity = cx.entity();
                move |cx| entity.update(cx, |_, cx| cx.emit(FindEvent::ReplaceOne))
            }))
            .child(button("All", theme, {
                let entity = cx.entity();
                move |cx| entity.update(cx, |_, cx| cx.emit(FindEvent::ReplaceAll))
            }))
    }
}

/// One text field: the typed text, or a muted placeholder.
///
/// No caret and no text selection. gpui has no text input element and the editor's own
/// caret is a background highlight on a character (see `editor/view.rs`); a real input
/// needs the same custom `Element` that unlocks IME. The focused field is marked by its
/// border instead, which is legible in all five themes and does not fake a control that
/// does not work.
fn text_field(text: &str, placeholder: &str, focused: bool, theme: &Theme) -> impl IntoElement {
    let empty = text.is_empty();
    div()
        .flex_1()
        .min_w(px(120.0))
        .h(px(22.0))
        .flex()
        .items_center()
        .px_2()
        .rounded_sm()
        .bg(theme.background)
        .border_1()
        .border_color(if focused { theme.accent } else { theme.border })
        .when(empty, |el| el.text_color(theme.text_muted))
        .child(SharedString::from(if empty { placeholder.to_string() } else { text.to_string() }))
}

fn button(
    label: &'static str,
    theme: &Theme,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(label)
        .h(px(20.0))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .text_color(theme.text_muted)
        .hover(|el| el.bg(theme.hover).text_color(theme.text))
        .active(|el| el.bg(theme.pressed))
        .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| on_click(cx))
        .child(SharedString::from(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_says_nothing_at_all() {
        // Not "0 of 0": there is no query, so there is nothing to report.
        let (label, problem) = Status::Empty.label();
        assert_eq!(label, "");
        assert!(!problem);
    }

    #[test]
    fn no_matches_reads_as_a_problem_and_zero_reads_as_no_results() {
        let (label, problem) = Status::Counted { current: None, total: 0 }.label();
        assert_eq!(label, "No results");
        assert!(problem, "an empty result must be visually distinct from a count");
    }

    #[test]
    fn a_count_without_a_current_match_does_not_claim_one() {
        // Before the first ⌘G every hit is highlighted but the cursor has not moved, so
        // "1 of 17" would point at a match the user is not on.
        let (label, problem) = Status::Counted { current: None, total: 17 }.label();
        assert_eq!(label, "17 matches");
        assert!(!problem);
    }

    #[test]
    fn a_current_match_reads_as_n_of_m() {
        let (label, _) = Status::Counted { current: Some(3), total: 17 }.label();
        assert_eq!(label, "3 of 17");
    }

    #[test]
    fn an_invalid_pattern_says_so_rather_than_reporting_zero_matches() {
        // The two states have different fixes — fix the regex vs search for something
        // else — so they must not render the same.
        let (invalid, _) = Status::InvalidRegex.label();
        let (none, _) = Status::Counted { current: None, total: 0 }.label();
        assert_ne!(invalid, none);
        assert_eq!(invalid, "Invalid pattern");
        assert!(Status::InvalidRegex.label().1);
    }

    #[test]
    fn a_file_too_large_to_search_says_so() {
        let (label, problem) = Status::TooLarge.label();
        assert_eq!(label, "File too large to search");
        assert!(problem);
    }
}
