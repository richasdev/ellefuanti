//! Turning a VS Code theme into a [`ThemeFile`].
//!
//! Two sources in the file, and they behave differently:
//!
//! - **`colors`** is a flat map of workbench keys — `editor.background`,
//!   `terminal.ansiRed`. A lookup, with a fallback chain where this editor's surfaces do
//!   not line up with VS Code's.
//! - **`tokenColors`** is a list of TextMate rules, resolved by specificity in
//!   [`crate::scope`]. That is where the real work is; see that module for why.
//!
//! # What this does not attempt
//!
//! `semanticTokenColors` is ignored. It only applies when a language server is running and
//! sends semantic tokens, and this editor highlights with tree-sitter — importing it would
//! produce colours that never appear.
//!
//! `include` is ignored. Some themes are a thin file that inherits from another by relative
//! path; following it means resolving paths out of an extension directory this crate knows
//! nothing about. A theme that inherits everything imports as a theme with no rules, which
//! fails the required-colour check by name rather than silently producing black.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Appearance;
use crate::color::{self, Rgb};
use crate::file::{REQUIRED_COLORS, ThemeError, ThemeFile};
use crate::scope::{Rule, resolve_any};

/// Each of this editor's syntax colours, and the TextMate scopes it can come from, in
/// preference order.
///
/// The lists are longer than one entry because themes disagree about which scope carries a
/// concept. One Dark Pro styles `entity.name.type`; GitHub Dark has no such rule and the
/// answer comes from the broader `entity`. Ordering is "most specific idea of this concept
/// first", so a theme that has a real opinion is asked for it before a fallback.
///
/// **A scope's presence here is not a claim that every theme has it.** Where none of the
/// candidates match, [`FALLBACKS`] decides, and the result is a colour chosen on purpose
/// rather than a default-constructed black.
const SYNTAX_SCOPES: [(&str, &[&str]); 12] = [
    ("keyword", &["keyword.control", "keyword"]),
    ("type", &["entity.name.type", "support.type", "entity.name.class", "storage.type", "entity"]),
    ("function", &["entity.name.function", "support.function"]),
    ("variable", &["variable"]),
    // `meta.object-literal.key` is what One Dark Pro styles and is how it gets the same
    // `#e06c75` as `variable`; `meta.property-name` is GitHub's.
    (
        "property",
        &[
            "variable.other.property",
            "meta.object-literal.key",
            "support.variable.property",
            "meta.property-name",
        ],
    ),
    ("string", &["string.quoted", "string"]),
    ("number", &["constant.numeric", "constant"]),
    ("operator", &["keyword.operator"]),
    // The #53 trap: `entity.other.attribute-name` must be asked for before `entity.name.tag`,
    // and within the file the specificity rule keeps the tag rule from answering for it.
    ("attribute", &["entity.other.attribute-name", "entity.name.tag"]),
    ("comment", &["comment"]),
    ("tag", &["entity.name.tag"]),
    // Blade is not a language any of these themes has an opinion about. `support.function`
    // is the nearest thing — a directive is a call — and where a theme does not style it
    // the fallback is `accent`, which keeps a directive in the theme's own palette.
    ("blade", &["support.function", "keyword.control"]),
];

/// Workbench keys for this editor's UI surfaces, in fallback order.
///
/// VS Code has ~240 colour keys and this editor has twelve surfaces, so most of these are a
/// short chain ending in a key every theme sets. Where even that is absent, [`FALLBACKS`]
/// derives from `background` or `text`.
const UI_KEYS: [(&str, &[&str]); 12] = [
    ("background", &["editor.background"]),
    ("panel", &["sideBar.background", "panel.background", "editor.background"]),
    ("border", &["panel.border", "editorGroup.border", "contrastBorder", "focusBorder"]),
    ("text", &["editor.foreground", "foreground"]),
    ("text_muted", &["editorLineNumber.foreground", "descriptionForeground"]),
    ("accent", &["focusBorder", "textLink.foreground", "editorCursor.foreground"]),
    ("hover", &["list.hoverBackground", "editorWidget.background"]),
    ("selected", &["list.activeSelectionBackground", "editorWidget.background"]),
    // No VS Code key means "the surface under a held mouse button". Always derived; see
    // `FALLBACKS`.
    ("pressed", &[]),
    ("cursor", &["editorCursor.foreground", "focusBorder"]),
    ("selection", &["editor.selectionBackground", "editor.inactiveSelectionBackground"]),
    ("status_bar", &["statusBar.background", "sideBar.background", "editor.background"]),
];

/// Diagnostic colours. VS Code themes frequently leave these to its own defaults, which are
/// not the theme's colours, so the fallbacks matter more here than anywhere else.
const DIAGNOSTIC_KEYS: [(&str, &[&str]); 4] = [
    (
        "error",
        &["editorError.foreground", "editorOverviewRuler.errorForeground", "errorForeground"],
    ),
    ("warning", &["editorWarning.foreground", "editorOverviewRuler.warningForeground"]),
    ("information", &["editorInfo.foreground", "editorOverviewRuler.infoForeground"]),
    ("hint", &["editorHint.foreground"]),
];

/// The sixteen `terminal.ansi*` keys, in slot order.
const ANSI_KEYS: [&str; 16] = [
    "terminal.ansiBlack",
    "terminal.ansiRed",
    "terminal.ansiGreen",
    "terminal.ansiYellow",
    "terminal.ansiBlue",
    "terminal.ansiMagenta",
    "terminal.ansiCyan",
    "terminal.ansiWhite",
    "terminal.ansiBrightBlack",
    "terminal.ansiBrightRed",
    "terminal.ansiBrightGreen",
    "terminal.ansiBrightYellow",
    "terminal.ansiBrightBlue",
    "terminal.ansiBrightMagenta",
    "terminal.ansiBrightCyan",
    "terminal.ansiBrightWhite",
];

/// Where a colour comes from when the theme said nothing about it.
///
/// **This is the "never fall back to black" list.** Every entry names another key in the
/// same theme, so an unstyled concept lands somewhere in the theme's own palette instead of
/// at a default-constructed zero. Resolved in order, and every chain terminates at
/// `text` or `background`, which are the two keys the importer refuses to proceed without.
///
/// `pressed` is the interesting one: it has no VS Code equivalent at all, and the compiled
/// themes each nudge it one step further from their own background — so it is derived
/// arithmetically rather than borrowed. See [`step_from_background`].
///
/// The diagnostic entries are the ones that fire most often. **Neither GitHub theme sets
/// `editorError.foreground` or `editorWarning.foreground`** — VS Code falls back to its own
/// built-in reds and yellows, which are not the theme's colours. Borrowing the theme's own
/// red (its `keyword`, or ANSI slot 1) and its own amber keeps a squiggle in-palette, which
/// is what the compiled-in ports do by hand.
const FALLBACKS: [(&str, &[&str]); 14] = [
    ("panel", &["background"]),
    ("border", &["text_muted"]),
    ("text_muted", &["comment", "text"]),
    ("accent", &["cursor", "keyword", "text"]),
    ("hover", &["panel"]),
    ("selected", &["hover"]),
    ("cursor", &["accent", "text"]),
    ("selection", &["selected"]),
    ("status_bar", &["panel"]),
    ("blade", &["accent"]),
    // Diagnostics. `error` prefers the theme's red; `warning` its amber, which in practice
    // is whatever it paints numbers or types with.
    ("error", &["ansi.1", "keyword", "text"]),
    ("warning", &["ansi.3", "number", "type", "text"]),
    ("information", &["accent", "text"]),
    ("hint", &["comment", "text_muted", "text"]),
];

/// A VS Code theme, parsed far enough to answer questions about it.
#[derive(Debug)]
pub struct VsCodeTheme {
    path: PathBuf,
    name: Option<String>,
    declared_type: Option<String>,
    colors: BTreeMap<String, String>,
    rules: Vec<Rule>,
}

impl VsCodeTheme {
    /// Parses a VS Code theme document.
    ///
    /// Tolerant on purpose: a rule with no `foreground`, a `scope` that is neither a string
    /// nor a list, an unparseable colour in a key nothing asks for — none of those stop the
    /// import, because the file is somebody else's and the parts this editor uses are a
    /// small fraction of it. What fails is a document that is not an object, or one that
    /// leaves a required colour with no value and no fallback.
    pub fn parse(path: &Path, text: &str) -> Result<Self, ThemeError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|source| ThemeError::Malformed { path: path.to_path_buf(), source })?;

        let Value::Object(document) = value else {
            return Err(ThemeError::NotAnObject { path: path.to_path_buf() });
        };

        let colors = document
            .get("colors")
            .and_then(Value::as_object)
            .map(|colors| {
                colors
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let rules = document
            .get("tokenColors")
            .and_then(Value::as_array)
            .map(|entries| entries.iter().flat_map(flatten_rule).collect())
            .unwrap_or_default();

        Ok(Self {
            path: path.to_path_buf(),
            name: document.get("name").and_then(Value::as_str).map(str::to_string),
            declared_type: document.get("type").and_then(Value::as_str).map(str::to_string),
            colors,
            rules,
        })
    }

    /// Reads a VS Code theme from disk. Blocking; see ADR-0007.
    pub fn load(path: &Path) -> Result<Self, ThemeError> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| ThemeError::Unreadable { path: path.to_path_buf(), source })?;
        Self::parse(path, &text)
    }

    /// Dark or light.
    ///
    /// `type` when the file declares one — and **many do not**. Neither of GitHub's two
    /// published themes has the key, despite one being white and one being near-black, so
    /// treating its absence as "assume dark" would import GitHub Light with a dark theme's
    /// terminal palette. #48 established that the ANSI readability fixes are
    /// background-specific and actively wrong when applied to the other kind, which makes
    /// this the one inference in the importer worth making rather than defaulting.
    ///
    /// The fallback measures `editor.background`: a theme is light when its background is
    /// bright. Crude, and right on every real file, because the question "is this
    /// background light or dark" is exactly what the luminance of the background answers.
    pub fn appearance(&self) -> Appearance {
        if let Some(declared) = self.declared_type.as_deref().and_then(Appearance::parse) {
            return declared;
        }

        match self.colors.get("editor.background").map(String::as_str).map(color::parse) {
            Some(Ok(background)) if is_light(background) => Appearance::Light,
            // No background at all is a broken theme that will fail the required-colour
            // check a moment later with a better message than this function could give.
            _ => Appearance::Dark,
        }
    }

    /// The theme's own name, or the file stem.
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            self.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
        })
    }

    /// The first of `keys` the theme sets to a parseable colour.
    fn workbench(&self, keys: &[&str]) -> Option<Rgb> {
        keys.iter().find_map(|key| color::parse(self.colors.get(*key)?).ok())
    }

    /// Converts to the native format.
    ///
    /// **The order is load-bearing.** Direct sources first — workbench keys and resolved
    /// scopes — then the ANSI table, then [`FALLBACKS`] for whatever is still missing, then
    /// `pressed`, which is always computed. A required colour still absent at the end is an
    /// error naming the key, which is the "fall back deliberately, never to black" rule
    /// stated as control flow.
    ///
    /// ANSI comes before the fallbacks because the two run in opposite directions and would
    /// otherwise be a cycle: `ansi_table` fills an unset slot from the syntax palette, while
    /// `error` and `warning` — which no GitHub theme sets — fall back to ANSI slots 1 and 3.
    /// Building the table first means each side reads only values the other has finished
    /// with.
    pub fn to_theme_file(&self, name: &str) -> Result<ThemeFile, ThemeError> {
        let mut colors: BTreeMap<String, Rgb> = BTreeMap::new();

        for (key, sources) in UI_KEYS.iter().chain(&DIAGNOSTIC_KEYS) {
            if let Some(color) = self.workbench(sources) {
                colors.insert((*key).to_string(), color);
            }
        }

        for (key, scopes) in SYNTAX_SCOPES {
            if let Some(color) = resolve_any(&self.rules, scopes).and_then(|c| color::parse(c).ok())
            {
                colors.insert(key.to_string(), color);
            }
        }

        // `operator` unset means the theme styles operators as ordinary text, which is what
        // VS Code renders — GitHub does exactly this. An explicit rule rather than a
        // `FALLBACKS` entry, because it is a statement about the source format, not a
        // last resort.
        if !colors.contains_key("operator")
            && let Some(text) = colors.get("text").copied()
        {
            colors.insert("operator".to_string(), text);
        }

        let ansi = self.ansi_table(&colors);
        for (slot, color) in ansi.into_iter().enumerate() {
            colors.insert(format!("ansi.{slot}"), color);
        }

        for (key, sources) in FALLBACKS {
            if colors.contains_key(key) {
                continue;
            }
            if let Some(color) = sources.iter().find_map(|source| colors.get(*source).copied()) {
                colors.insert(key.to_string(), color);
            }
        }

        // Always derived: no theme names it, and borrowing `hover` or `selected` makes it
        // invisible on the themes where those two are equal — One Dark Pro is exactly that
        // theme, which is why the compiled-in variants each set it by hand.
        if let Some(background) = colors.get("background").copied() {
            colors
                .insert("pressed".to_string(), step_from_background(background, self.appearance()));
        }

        let missing: Vec<String> = REQUIRED_COLORS
            .iter()
            .filter(|key| !colors.contains_key(**key))
            .map(|key| format!("no colour found for \"{key}\""))
            .collect();
        if !missing.is_empty() {
            return Err(ThemeError::Invalid { path: self.path.clone(), problems: missing });
        }

        Ok(ThemeFile {
            name: name.to_string(),
            appearance: self.appearance(),
            origin: None,
            colors,
        })
    }

    /// The sixteen ANSI slots.
    ///
    /// A theme that sets `terminal.ansi*` has already answered this and its values are used
    /// verbatim — that is the whole table for all three of the reference themes. Where a
    /// theme is silent, the slot falls back to a syntax colour of roughly the right hue, and
    /// the two slots that cannot work that way — 0 and 7, black and white — are derived from
    /// the theme's background and text according to its [`Appearance`].
    ///
    /// #48's rule applies here: lifting slot 0 off the background is a *dark* fix, and on a
    /// light theme slot 0 is genuinely black. Same question, opposite answer, which is why
    /// appearance has to be known before this runs.
    fn ansi_table(&self, colors: &BTreeMap<String, Rgb>) -> [Rgb; 16] {
        let appearance = self.appearance();
        let text = colors.get("text").copied().unwrap_or(0x000000);
        let background = colors.get("background").copied().unwrap_or(0xffffff);

        let syntax = |key: &str, default: Rgb| colors.get(key).copied().unwrap_or(default);

        // The normal row, then the bright row. Where the theme is silent the syntax palette
        // stands in, because a red is a red whether it is a keyword or `ls` output.
        let defaults: [Rgb; 16] = [
            // Slot 0 is the one #48 is about: on dark it must lift off the background or it
            // is invisible; on light, black is correct and readable.
            match appearance {
                Appearance::Dark => step_from_background(background, appearance),
                Appearance::Light => text,
            },
            syntax("error", 0xcc0000),
            syntax("string", 0x00cc00),
            syntax("warning", 0xcccc00),
            syntax("accent", 0x0066cc),
            syntax("keyword", 0xcc00cc),
            syntax("type", 0x00cccc),
            // Slot 7 is "white", which on a light background has to be a mid grey or it
            // disappears into the page.
            match appearance {
                Appearance::Dark => text,
                Appearance::Light => syntax("text_muted", 0x666666),
            },
            syntax("comment", 0x666666),
            syntax("error", 0xff6666),
            syntax("string", 0x66ff66),
            syntax("warning", 0xffff66),
            syntax("accent", 0x6699ff),
            syntax("keyword", 0xff66ff),
            syntax("type", 0x66ffff),
            match appearance {
                Appearance::Dark => 0xffffff,
                Appearance::Light => text,
            },
        ];

        let mut table = defaults;
        for (slot, key) in ANSI_KEYS.iter().enumerate() {
            if let Some(color) = self.workbench(&[key]) {
                table[slot] = color;
            }
        }
        table
    }
}

/// Flattens one `tokenColors` entry into its scopes.
///
/// A `scope` may be a string, a comma-separated string, or a list — all three appear in the
/// three reference files. An entry with no `foreground` sets only `fontStyle`, which this
/// editor does not model, and contributes no rules rather than a rule with no colour.
fn flatten_rule(entry: &Value) -> Vec<Rule> {
    let Some(foreground) = entry.get("settings").and_then(|s| s.get("foreground")) else {
        return Vec::new();
    };
    let Some(foreground) = foreground.as_str() else { return Vec::new() };

    let selectors: Vec<String> = match entry.get("scope") {
        Some(Value::String(scope)) => scope.split(',').map(|s| s.trim().to_string()).collect(),
        Some(Value::Array(scopes)) => {
            scopes.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).collect()
        }
        _ => Vec::new(),
    };

    selectors
        .into_iter()
        .filter(|selector| !selector.is_empty())
        .map(|selector| Rule { selector, foreground: foreground.to_string() })
        .collect()
}

/// Whether a colour is light enough to sit text on top of.
///
/// Rec. 601 luma, which is the cheap standard answer and needs no colour-space conversion.
/// The threshold is the midpoint; nothing in a real theme sits near it — the three reference
/// backgrounds score 0x14, 0x2d and 0xff.
fn is_light(color: Rgb) -> bool {
    let (r, g, b) = ((color >> 16) & 0xff, (color >> 8) & 0xff, color & 0xff);
    (r * 299 + g * 587 + b * 114) / 1000 > 128
}

/// One step away from the background, in the direction that is away from it.
///
/// A dark theme's surfaces get lighter as they come forward and a light theme's get darker.
/// The step is a fixed 0x18 per channel, saturating, which is enough to read as a change and
/// small enough not to leave the theme's own register.
fn step_from_background(background: Rgb, appearance: Appearance) -> Rgb {
    const STEP: u32 = 0x18;

    let channel = |shift: u32| {
        let value = (background >> shift) & 0xff;
        match appearance {
            Appearance::Dark => (value + STEP).min(0xff),
            Appearance::Light => value.saturating_sub(STEP),
        }
    };

    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// Imports a VS Code theme file into the native format.
///
/// `name` is what the theme is selected by, and is the caller's to choose — a file called
/// `dark-default.json` is "GitHub Dark", which nothing in the file says.
pub fn import(path: &Path, name: &str) -> Result<ThemeFile, ThemeError> {
    VsCodeTheme::load(path)?.to_theme_file(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(json: &str) -> VsCodeTheme {
        VsCodeTheme::parse(Path::new("/nowhere/theme.json"), json).unwrap()
    }

    #[test]
    fn a_scope_given_as_a_comma_separated_string_becomes_several_rules() {
        let theme = theme(
            r##"{"tokenColors": [{"scope": "comment, string", "settings": {"foreground": "#111111"}}]}"##,
        );

        assert_eq!(theme.rules.len(), 2);
        assert_eq!(crate::scope::resolve(&theme.rules, "string"), Some("#111111"));
    }

    #[test]
    fn a_rule_with_no_foreground_contributes_nothing() {
        let theme = theme(
            r#"{"tokenColors": [{"scope": "comment", "settings": {"fontStyle": "italic"}}]}"#,
        );
        assert!(theme.rules.is_empty(), "a fontStyle-only rule has no colour to offer");
    }

    /// The inference that GitHub's themes force. Neither declares `type`.
    #[test]
    fn a_theme_with_no_type_is_classified_by_its_background() {
        assert_eq!(
            theme(r##"{"colors": {"editor.background": "#ffffff"}}"##).appearance(),
            Appearance::Light
        );
        assert_eq!(
            theme(r##"{"colors": {"editor.background": "#0d1117"}}"##).appearance(),
            Appearance::Dark
        );
    }

    #[test]
    fn a_declared_type_is_believed_over_the_background() {
        // A theme that says "light" while setting a dark background is wrong, but it is the
        // theme's own declaration and second-guessing it is not this importer's job.
        let theme = theme(r##"{"type": "light", "colors": {"editor.background": "#000000"}}"##);
        assert_eq!(theme.appearance(), Appearance::Light);
    }

    #[test]
    fn slot_zero_lifts_off_a_dark_background_and_is_black_on_a_light_one() {
        let dark = theme(
            r##"{"colors": {"editor.background": "#000000", "editor.foreground": "#ffffff"}}"##,
        );
        let light = theme(
            r##"{"colors": {"editor.background": "#ffffff", "editor.foreground": "#000000"}}"##,
        );

        let mut dark_colors = BTreeMap::new();
        dark_colors.insert("background".to_string(), 0x000000);
        dark_colors.insert("text".to_string(), 0xffffff);
        let dark_table = dark.ansi_table(&dark_colors);

        let mut light_colors = BTreeMap::new();
        light_colors.insert("background".to_string(), 0xffffff);
        light_colors.insert("text".to_string(), 0x000000);
        let light_table = light.ansi_table(&light_colors);

        assert_ne!(dark_table[0], 0x000000, "#48: an unlifted slot 0 is invisible on dark");
        assert_eq!(light_table[0], 0x000000, "on light, black is the readable colour");
    }

    #[test]
    fn a_theme_that_sets_the_ansi_keys_has_them_used_verbatim() {
        let theme = theme(
            r##"{"colors": {"editor.background": "#000000", "terminal.ansiRed": "#abcdef"}}"##,
        );
        let table = theme.ansi_table(&BTreeMap::new());
        assert_eq!(table[1], 0xabcdef);
    }

    #[test]
    fn a_press_moves_away_from_the_background_in_the_right_direction() {
        assert!(step_from_background(0x000000, Appearance::Dark) > 0x000000);
        assert_eq!(step_from_background(0xffffff, Appearance::Light), 0xe7e7e7);
        // Saturating, not wrapping: a white dark-theme background must not become black.
        assert_eq!(step_from_background(0xffffff, Appearance::Dark), 0xffffff);
        assert_eq!(step_from_background(0x000000, Appearance::Light), 0x000000);
    }

    #[test]
    fn a_theme_with_nothing_in_it_fails_by_naming_the_keys_it_could_not_fill() {
        let error = theme(r#"{"name": "Empty"}"#)
            .to_theme_file("empty")
            .expect_err("no colours means no theme");

        let message = error.to_string();
        assert!(message.contains("background"), "{message}");
        assert!(message.contains("/nowhere/theme.json"), "must name the file: {message}");
    }

    #[test]
    fn a_theme_that_is_not_json_names_the_position() {
        let error =
            VsCodeTheme::parse(Path::new("/nowhere/t.json"), "{not json").expect_err("malformed");
        assert!(error.to_string().contains("line 1"), "{error}");
    }

    #[test]
    fn the_display_name_falls_back_to_the_file_stem() {
        let theme = VsCodeTheme::parse(Path::new("/x/OneDark-Pro.json"), "{}").unwrap();
        assert_eq!(theme.display_name(), "OneDark-Pro");
    }

    #[test]
    fn luminance_classifies_the_three_real_backgrounds() {
        assert!(!is_light(0x0d1117), "GitHub Dark");
        assert!(!is_light(0x282c34), "One Dark Pro");
        assert!(is_light(0xffffff), "GitHub Light");
    }
}
