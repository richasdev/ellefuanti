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
    /// The AI section's "Set API key…" (#99). The workspace opens the prompt and does
    /// the Keychain write, because the panel must never hold the key — a key that only
    /// ever exists in the prompt's text and the `security` argv is a key that cannot be
    /// logged or persisted by accident here.
    SetApiKey,
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
    /// Codex availability (#99), resolved once off the main thread and cached. `None`
    /// while the probe runs. It lives here rather than being read per-render because the
    /// check spawns `codex --version`, and a render must never spawn a process.
    codex_status: Option<Result<(), String>>,
}

impl EventEmitter<SettingsPanelEvent> for SettingsPanel {}

impl SettingsPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Probed unconditionally: the cycler can reach Codex from any starting provider,
        // and an answer that only arrives after the user lands there reads as a stall.
        #[cfg(not(test))]
        cx.spawn(async move |this, cx| {
            let status = cx.background_spawn(async { crate::ai_codex::availability() }).await;
            this.update(cx, |this, cx| {
                this.codex_status = Some(status);
                cx.notify();
            })
            .ok();
        })
        .detach();
        Self { focus_handle: cx.focus_handle(), chosen_theme: None, codex_status: None }
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
    /// Flips one of the boolean settings and persists it, through the one door.
    fn toggle(&mut self, kind: ToggleKind, cx: &mut Context<Self>) {
        let settings = crate::settings::current(cx);
        crate::settings::update_settings(cx, |mutable| match kind {
            ToggleKind::Autosave => mutable.set_autosave(!settings.autosave()),
            // The AI switches (#99): off by default, and this panel is how they come on.
            ToggleKind::AiChat => mutable.set_ai_chat_enabled(!settings.ai_chat_enabled()),
            ToggleKind::AiAutocomplete => {
                mutable.set_ai_autocomplete_enabled(!settings.ai_autocomplete_enabled())
            }
        });
        cx.notify();
    }

    /// Steps one of the AI provider cyclers (#99). Backward is `next` repeated to the
    /// wrap — cheaper than teaching `Provider` a `previous` it needs nowhere else.
    ///
    /// Two cyclers because chat and autocomplete can differ: the owner runs ghost text on
    /// an API key and chat on the Codex subscription, and one shared value cannot say that.
    fn cycle_provider(&mut self, chat: bool, forward: bool, cx: &mut Context<Self>) {
        let settings = crate::settings::current(cx);
        let current = crate::ai::Provider::from_setting(if chat {
            settings.ai_chat_provider()
        } else {
            settings.ai_provider()
        });
        let mut next = current.next();
        if !forward {
            // Backward: keep stepping until the *next* step would come home.
            while next.next() != current {
                next = next.next();
            }
        }
        crate::settings::update_settings(cx, |settings| {
            if chat {
                settings.set_ai_chat_provider(next.setting_name());
            } else {
                settings.set_ai_provider(next.setting_name());
            }
        });
        cx.notify();
    }

    // Production callers moved to the chip grid's `choose_theme`; the lap behaviour this
    // implements (skip-nothing wrap over the registry) is still pinned by a render test,
    // which is the only reachable door — hence the cfg gate rather than deletion.
    #[cfg_attr(not(test), allow(dead_code))]
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

    /// The picker grid's direct route: click a theme, get that theme.
    fn choose_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        crate::themes::apply_named(name, cx);
        self.chosen_theme = Some(name.to_string());
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

/// Which boolean setting a toggle row flips.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ToggleKind {
    Autosave,
    AiChat,
    AiAutocomplete,
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

        // Sections (owner request): the flat five-row form grew enough controls to need
        // grouping. Editor first because it is what people come to change most.
        let editor_rows = [
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
            row_toggle("Auto save", ToggleKind::Autosave, settings.autosave(), &theme, &entity),
        ];

        // The AI section (#99). The provider and the two switches are writable; the
        // models are shown read-only with the JSON as the edit path — a free-text model
        // field is a text input this panel deliberately does not fake (see the find
        // bar's `text_field` note). The key never appears here at all: the button asks
        // the workspace to prompt, and the Keychain is the only destination.
        let chat_provider = crate::ai::Provider::from_setting(settings.ai_chat_provider());
        let ai_rows = [
            row_picker(
                "Chat provider",
                chat_provider.setting_name().to_string(),
                RowKind::AiChatProvider,
                &theme,
                &entity,
            ),
            row_picker(
                "Autocomplete provider",
                crate::ai::Provider::from_setting(settings.ai_provider())
                    .setting_name()
                    .to_string(),
                RowKind::AiProvider,
                &theme,
                &entity,
            ),
            row_toggle("AI chat", ToggleKind::AiChat, settings.ai_chat_enabled(), &theme, &entity),
            row_toggle(
                "AI autocomplete",
                ToggleKind::AiAutocomplete,
                settings.ai_autocomplete_enabled(),
                &theme,
                &entity,
            ),
            row_readonly("Chat model", settings.ai_chat_model().to_string(), &theme),
            row_readonly("Autocomplete model", settings.ai_completion_model().to_string(), &theme),
        ];

        // Codex's state, shown only when it is selected: whether the CLI is there and
        // logged in, or the command that would fix it. Cached by the panel rather than
        // probed here — a render must not spawn a process.
        let codex_row = (chat_provider == crate::ai::Provider::Codex).then(|| {
            let status = match &self.codex_status {
                None => "checking…".to_string(),
                Some(Ok(())) => "installed, logged in".to_string(),
                Some(Err(reason)) => reason.clone(),
            };
            row_readonly("Codex", status, &theme)
        });

        // "Set API key…" is meaningless for a provider that holds its own login, and a
        // button that cannot do anything is worse than no button (#99).
        let takes_key = chat_provider.takes_api_key()
            || crate::ai::Provider::from_setting(settings.ai_provider()).takes_api_key();

        // The theme picker: every selectable theme as a chip with four swatch dots —
        // background, accent, string, keyword — so palettes are told apart on sight
        // instead of by cycling through them one repaint at a time.
        let current_theme = theme_name;
        let theme_chips: Vec<gpui::AnyElement> = crate::themes::selectable_names(cx)
            .into_iter()
            .map(|name| {
                let swatches = crate::themes::preview(&name, cx);
                let selected = name == current_theme;
                let entity = entity.clone();
                let chip_name = name.clone();
                div()
                    .id(gpui::ElementId::Name(format!("theme-{name}").into()))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if selected { theme.accent } else { theme.border })
                    .when(selected, |el| el.bg(theme.selected))
                    .hover(|el| el.bg(theme.hover))
                    .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                        entity.update(cx, |panel, cx| panel.choose_theme(&chip_name, cx));
                    })
                    .children(swatches.map(|colors| {
                        div().flex().gap_1().children(
                            colors
                                .into_iter()
                                .map(|color| div().size(px(10.0)).rounded(px(5.0)).bg(color)),
                        )
                    }))
                    .child(SharedString::from(name))
                    .into_any_element()
            })
            .collect();

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
            .child(section_header("Editor", &theme))
            .children(editor_rows)
            .child(section_header("Appearance", &theme))
            .child(div().flex().flex_wrap().gap_1().children(theme_chips))
            .child(section_header("AI", &theme))
            .children(ai_rows)
            .children(codex_row)
            .when(takes_key, |el| {
                el.child(
                    div()
                        .id("settings-set-api-key")
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
                                entity
                                    .update(cx, |_this, cx| cx.emit(SettingsPanelEvent::SetApiKey));
                            }
                        })
                        .child("Set API key…"),
                )
            })
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

/// A muted group label with room above it — what turns the flat form into sections.
fn section_header(label: &'static str, theme: &crate::theme::Theme) -> gpui::AnyElement {
    div().mt_1().text_color(theme.text_muted).child(label).into_any_element()
}

/// Which cycler a ‹ › pair drives.
#[derive(Clone, Copy)]
enum RowKind {
    Family,
    /// Autocomplete's provider (`ai.provider`): anthropic → ant → custom → codex.
    AiProvider,
    /// Chat's own provider (`ai.chat_provider`), which may differ (#99).
    AiChatProvider,
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
                    RowKind::Family => panel.cycle_family(forward, cx),
                    RowKind::AiProvider => panel.cycle_provider(false, forward, cx),
                    RowKind::AiChatProvider => panel.cycle_provider(true, forward, cx),
                });
            })
            .child(glyph)
    };

    row(label, value, arrow("‹", false), arrow("›", true), theme)
}

/// A row with a single on/off control — a labelled button whose text IS the state, so
/// the toggle needs no colour to be read (the house rule: nothing by colour alone).
fn row_toggle(
    label: &'static str,
    kind: ToggleKind,
    on: bool,
    theme: &crate::theme::Theme,
    entity: &gpui::Entity<SettingsPanel>,
) -> gpui::AnyElement {
    let entity = entity.clone();
    let button = div()
        .id(label)
        .px_2()
        .rounded(px(4.0))
        .cursor_pointer()
        .when(on, |el| el.bg(theme.selected))
        .hover(|el| el.bg(theme.hover))
        .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
            entity.update(cx, |panel, cx| panel.toggle(kind, cx));
        })
        .child(if on { "On" } else { "Off" });
    row(label, String::new(), button, gpui::div().into_any_element(), theme)
}

/// A row that only shows a value — the models (#99), whose edit path is the JSON file.
fn row_readonly(
    label: &'static str,
    value: String,
    theme: &crate::theme::Theme,
) -> gpui::AnyElement {
    row(label, value, gpui::div().into_any_element(), gpui::div().into_any_element(), theme)
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
