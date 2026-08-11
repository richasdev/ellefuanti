//! The settings panel: ⌘, as a form instead of a JSON file (#100).
//!
//! # Scope, held deliberately
//!
//! Five fields, because five settings exist. The issue is explicit that a panel whose
//! fields outnumber the settings is how a config surface becomes a burden before it is
//! useful — so this renders exactly what `elle-settings` exposes, plus the one button
//! that keeps the JSON reachable. Hand-editing stays the power path; the panel is the
//! first-⌘, path.
//!
//! # One mutation door
//!
//! Every control writes through `settings::update_settings`, which re-resolves fonts
//! from the mutated document — so the panel cannot apply a family the startup path would
//! refuse (#85's monospace check lives inside `fonts::resolve`), cannot write a value
//! the file read would clamp differently, and cannot write at all on the launch where
//! the file was malformed (#60's rule). The panel owns no application logic; it is
//! buttons over the same machinery the zoom keys and the theme toggle already use.
//!
//! # The family picker measures on demand
//!
//! Cycling the font family steps through the system's families, skipping any that fail
//! the monospace check *at selection time* — the issue's hard requirement, because gpui
//! substitutes proportional faces silently and every column calculation breaks (#85).
//! The measurement happens per step rather than upfront: filtering every installed
//! family on open would cost hundreds of glyph measurements to build a list most opens
//! never scroll.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, MouseButton, SharedString, Window, div,
    prelude::*, px,
};

use crate::actions::{Cancel, context};
use crate::fonts::Fonts;
use crate::theme::{Metrics, Themed};

/// What the panel reports to the workspace.
pub enum SettingsPanelEvent {
    Dismissed,
    /// The "edit the file" button: the workspace opens settings.json in a tab.
    OpenJson,
}

pub struct SettingsPanel {
    focus_handle: FocusHandle,
    /// The theme the picker last applied, tracked here rather than read back.
    ///
    /// Found by this panel's own lap test: reading the position from the settings works
    /// only when a `LiveSettings` exists to persist into. On the malformed-file launch
    /// there is none, the read answers the default every time, and the picker sticks —
    /// alternating between the default and its neighbour forever. The panel is the only
    /// writer while it is open, so its own cursor is the truthful position; a theme
    /// changed *around* it (the palette's toggle) drifts this until the next click, which
    /// resyncs from the applied name's neighbour and is the cheap end of that trade.
    chosen_theme: Option<String>,
}

impl EventEmitter<SettingsPanelEvent> for SettingsPanel {}

impl SettingsPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self { focus_handle: cx.focus_handle(), chosen_theme: None }
    }

    fn cancel(&mut self, _: &Cancel, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SettingsPanelEvent::Dismissed);
    }

    /// Applies a ±step to one of the numeric settings.
    fn step(&mut self, field: NumericField, delta: f32, cx: &mut Context<Self>) {
        crate::settings::update_settings(cx, |settings| match field {
            NumericField::EditorFontSize => {
                settings.set_font_size(settings.font_size() + delta);
            }
            NumericField::UiFontSize => {
                settings.set_ui_font_size(settings.ui_font_size() + delta);
            }
            NumericField::LineHeight => {
                settings.set_line_height(settings.line_height() + delta);
            }
        });
        cx.notify();
    }

    /// Moves the theme picker one step through the registry, wrapping.
    fn cycle_theme(&mut self, forward: bool, cx: &mut Context<Self>) {
        let names = crate::themes::selectable_names(cx);
        if names.is_empty() {
            return;
        }
        let current = self
            .chosen_theme
            .clone()
            .unwrap_or_else(|| crate::settings::current(cx).theme().to_string());
        let index = names.iter().position(|name| *name == current).unwrap_or(0);
        let next = step_index(index, names.len(), forward);

        // `apply_named` is the whole live path — set_theme's persistence hook fires from
        // inside it, so the panel writes nothing itself here.
        crate::themes::apply_named(&names[next], cx);
        self.chosen_theme = Some(names[next].clone());
        cx.notify();
    }

    /// Moves the font family one step through the installed families, skipping any that
    /// fail the monospace check.
    ///
    /// Bounded by one full lap: on a machine where nothing else measures as monospace,
    /// this stops where it started instead of spinning forever.
    fn cycle_family(&mut self, forward: bool, cx: &mut Context<Self>) {
        let families = cx.text_system().all_font_names();
        if families.is_empty() {
            return;
        }
        let current = Fonts::get(cx).family.to_string();
        let start = families.iter().position(|name| *name == current).unwrap_or(0);

        let mut index = start;
        for _ in 0..families.len() {
            index = step_index(index, families.len(), forward);
            if index == start {
                return;
            }
            if crate::fonts::family_is_usable(&families[index], cx) {
                let chosen = families[index].clone();
                crate::settings::update_settings(cx, |settings| {
                    settings.set_font_family(&chosen);
                });
                cx.notify();
                return;
            }
        }
    }
}

#[cfg(test)]
impl SettingsPanel {
    /// A stepper click, through the same handler the button uses.
    pub fn step_for_test(&mut self, field: &str, delta: f32, cx: &mut Context<Self>) {
        let field = match field {
            "editor" => NumericField::EditorFontSize,
            "ui" => NumericField::UiFontSize,
            _ => NumericField::LineHeight,
        };
        self.step(field, delta, cx);
    }

    /// A theme-picker click.
    pub fn cycle_theme_for_test(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.cycle_theme(forward, cx);
    }
}

/// Which stepper a click belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NumericField {
    EditorFontSize,
    UiFontSize,
    LineHeight,
}

/// The next index in a wrapping cycle.
fn step_index(index: usize, len: usize, forward: bool) -> usize {
    if forward { (index + 1) % len } else { (index + len - 1) % len }
}

impl Focusable for SettingsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let settings = crate::settings::current(cx);
        let fonts = Fonts::get(cx);

        // Displayed values come from the live state, not from panel-local copies: there is
        // one mutation door and therefore nothing here to drift out of sync.
        let theme_name = self.chosen_theme.clone().unwrap_or_else(|| settings.theme().to_string());
        // The *resolved* family, deliberately: it is what the editor is actually drawing
        // with, and after a settings file names something unusable the two differ — showing
        // the file's wish here would claim a font that is not on screen (RISKS.md #4).
        let family = fonts.family.to_string();
        let font_size = format!("{}px", f32::from(fonts.size));
        let ui_size = format!("{}px", settings.ui_font_size());
        let line_height = format!("{:.2}", settings.line_height());

        let entity = cx.entity();
        let rows = [
            row_picker("Theme", theme_name, RowKind::Theme, &theme, &entity),
            row_picker("Editor font", family, RowKind::Family, &theme, &entity),
            row_stepper(
                "Editor font size",
                font_size,
                NumericField::EditorFontSize,
                1.0,
                &theme,
                &entity,
            ),
            row_stepper("UI font size", ui_size, NumericField::UiFontSize, 1.0, &theme, &entity),
            row_stepper(
                "Line height",
                line_height,
                NumericField::LineHeight,
                0.05,
                &theme,
                &entity,
            ),
        ];

        div()
            .key_context(context::PALETTE)
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down_out(cx.listener(|_this, _ev: &gpui::MouseDownEvent, _window, cx| {
                cx.emit(SettingsPanelEvent::Dismissed);
            }))
            .w(px(440.0))
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.0))
            .shadow_lg()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .text_color(theme.text)
            .text_size(fonts.ui_size)
            .child(div().text_color(theme.text_muted).child("Settings"))
            .children(rows)
            .child(
                // The escape hatch the issue requires: the panel must not become a wall
                // between the user and their file.
                div()
                    .id("settings-open-json")
                    .mt_1()
                    .px_2()
                    .py_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(theme.border)
                    .hover(|el| el.bg(theme.hover))
                    .on_mouse_down(MouseButton::Left, {
                        let entity = cx.entity();
                        move |_ev, _window, cx| {
                            entity.update(cx, |_this, cx| cx.emit(SettingsPanelEvent::OpenJson));
                        }
                    })
                    .child("Edit settings.json"),
            )
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child("Esc to close — changes apply immediately"),
            )
    }
}

/// Which cycler a ‹ › pair drives.
#[derive(Clone, Copy)]
enum RowKind {
    Theme,
    Family,
}

/// A row with ‹ value › controls.
fn row_picker(
    label: &'static str,
    value: String,
    kind: RowKind,
    theme: &crate::theme::Theme,
    entity: &gpui::Entity<SettingsPanel>,
) -> gpui::AnyElement {
    let arrow = |glyph: &'static str, forward: bool| {
        let entity = entity.clone();
        div()
            .id((label, forward as usize))
            .px_2()
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme.hover))
            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                entity.update(cx, |panel, cx| match kind {
                    RowKind::Theme => panel.cycle_theme(forward, cx),
                    RowKind::Family => panel.cycle_family(forward, cx),
                });
            })
            .child(glyph)
    };

    row(label, value, arrow("‹", false), arrow("›", true), theme)
}

/// A row with − value + controls.
fn row_stepper(
    label: &'static str,
    value: String,
    field: NumericField,
    step: f32,
    theme: &crate::theme::Theme,
    entity: &gpui::Entity<SettingsPanel>,
) -> gpui::AnyElement {
    let button = |glyph: &'static str, delta: f32| {
        let entity = entity.clone();
        div()
            .id((label, (delta > 0.0) as usize))
            .px_2()
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme.hover))
            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                entity.update(cx, |panel, cx| panel.step(field, delta, cx));
            })
            .child(glyph)
    };

    row(label, value, button("−", -step), button("+", step), theme)
}

fn row(
    label: &'static str,
    value: String,
    left: impl IntoElement,
    right: impl IntoElement,
    theme: &crate::theme::Theme,
) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .h(Metrics::ROW_HEIGHT)
        .gap_2()
        .child(div().w(px(140.0)).flex_none().text_color(theme.text_muted).child(label))
        .child(left)
        // Truncated for the same reason completion rows are: a long family name must not
        // wrap the row (that class of bug has shipped once already).
        .child(div().flex_1().truncate().child(SharedString::from(value)))
        .child(right)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cycle_wraps_in_both_directions() {
        // The pure half of both pickers. Wrapping is what makes ‹ from the first theme
        // reach the last instead of sticking — a stuck picker reads as broken.
        assert_eq!(step_index(0, 3, true), 1);
        assert_eq!(step_index(2, 3, true), 0, "forward wraps");
        assert_eq!(step_index(0, 3, false), 2, "backward wraps");
        assert_eq!(step_index(1, 1, true), 0, "a single entry cycles to itself");
    }
}
