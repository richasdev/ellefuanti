//! The native theme format: what a `.json` in `assets/themes/` contains.
//!
//! Flat, one key per colour, because `Theme` is flat. A nested `{"syntax": {...},
//! "ui": {...}}` would group the keys the way this crate happens to group its fields, and
//! that grouping is not something a theme author needs to know about.
//!
//! Unlike `elle-settings`, this **is** projected onto a fixed shape. The reasoning that
//! makes settings hold a raw `Map` — an older build must not drop a newer build's keys on
//! write — does not apply, because nothing writes a theme file back. Themes are read-only
//! input, so an unknown key is a typo worth reporting rather than data worth preserving.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::color::{self, Rgb};
use crate::{Appearance, THEME_VERSION};

/// Every colour a `Theme` needs, by the name it has in a theme file.
///
/// A map rather than 20-odd named fields: the app converts this into its own `Theme`
/// struct, which is where the names become compiler-checked. Holding them as data here
/// means the *importer* and the *file reader* produce the same thing and only one of them
/// has to be tested against the real `Theme`.
///
/// `BTreeMap` so `to_json` writes keys in a stable order — a theme file that reorders
/// itself between saves is a diff nobody can read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeFile {
    /// The name this theme is selected by, in `settings.json` and on the command palette.
    pub name: String,
    /// Dark or light. Drives terminal ANSI defaults; see [`Appearance`].
    pub appearance: Appearance,
    /// Where the theme came from and under what licence, when it was imported. Free text,
    /// carried through so a theme in `assets/themes/` records its own provenance.
    pub origin: Option<String>,
    /// Colour keys: `background`, `keyword`, `ansi.0` … `ansi.15`.
    pub colors: BTreeMap<String, Rgb>,
}

/// What went wrong reading a theme file.
///
/// Every variant names the file, because the whole point of this error type is that a user
/// who edited a theme can find and fix it. A theme that fails to load keeps the current
/// theme; nothing here is ever a reason to leave the editor unreadable.
#[derive(Debug)]
pub enum ThemeError {
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    Malformed {
        path: PathBuf,
        source: serde_json::Error,
    },
    NotAnObject {
        path: PathBuf,
    },
    /// A key that must be present is missing, or a colour did not parse. Collected rather
    /// than reported one at a time: someone hand-editing a theme wants the whole list.
    Invalid {
        path: PathBuf,
        problems: Vec<String>,
    },
}

impl ThemeError {
    /// The file this error is about, for a log line that names it.
    pub fn path(&self) -> &Path {
        match self {
            Self::Unreadable { path, .. }
            | Self::Malformed { path, .. }
            | Self::NotAnObject { path }
            | Self::Invalid { path, .. } => path,
        }
    }
}

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, source } => {
                write!(f, "could not read {}: {source}", path.display())
            }
            Self::Malformed { path, source } => {
                write!(f, "{} is not valid JSON: {source}", path.display())
            }
            Self::NotAnObject { path } => {
                write!(f, "{} should contain a JSON object", path.display())
            }
            Self::Invalid { path, problems } => {
                write!(f, "{} is not a usable theme: {}", path.display(), problems.join("; "))
            }
        }
    }
}

impl std::error::Error for ThemeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            Self::Malformed { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The colour keys a theme file must define, and the order `to_json` writes them in.
///
/// Every one is required. A theme missing `keyword` could fall back to `text`, and that is
/// exactly the "fall back to something plausible" behaviour that produces a theme which is
/// 80% right and impossible to debug — the importer's job is to fill these in deliberately
/// (see `import::REQUIRED` and the fallbacks it applies), so by the time a file exists they
/// are all decided.
pub const REQUIRED_COLORS: [&str; 26] = [
    "background",
    "panel",
    "border",
    "text",
    "text_muted",
    "accent",
    "hover",
    "selected",
    "pressed",
    "cursor",
    "selection",
    "status_bar",
    "keyword",
    "type",
    "function",
    "variable",
    "property",
    "string",
    "number",
    "operator",
    "attribute",
    "comment",
    "tag",
    "blade",
    // Diagnostics. `error` and `warning` are here; `information` and `hint` are derived by
    // the importer from `accent` and `comment` rather than required, because no VS Code
    // theme reliably sets them and demanding them would make every hand-written theme
    // carry two keys its author has no opinion about.
    "error",
    "warning",
];

/// The two diagnostic colours that are optional in a file, with the key they fall back to.
///
/// Not in [`REQUIRED_COLORS`] but still always present in a loaded `ThemeFile`: the fallback
/// is applied at parse time, so the app's conversion sees a complete set and never has to
/// know that these two are special.
const DERIVED_COLORS: [(&str, &str); 2] = [("information", "accent"), ("hint", "comment")];

impl ThemeFile {
    /// Parses a theme document.
    ///
    /// Collects every problem before failing, so a file with four bad colours reports four
    /// and not the first.
    pub fn parse(path: &Path, text: &str) -> Result<Self, ThemeError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|source| ThemeError::Malformed { path: path.to_path_buf(), source })?;

        let Value::Object(document) = value else {
            return Err(ThemeError::NotAnObject { path: path.to_path_buf() });
        };

        let mut problems = Vec::new();

        // A version this build does not know is a warning, not a failure — same reasoning
        // as `elle-settings`. Keys we understand are still read.
        if let Some(version) = document.get("version").and_then(Value::as_u64)
            && version > THEME_VERSION
        {
            tracing::warn!(
                file = %path.display(),
                file_version = version,
                known_version = THEME_VERSION,
                "theme was written by a newer build; reading what this build understands"
            );
        }

        let name = match document.get("name") {
            Some(Value::String(name)) if !name.trim().is_empty() => name.clone(),
            Some(_) => {
                problems.push("\"name\" must be a non-empty string".to_string());
                String::new()
            }
            None => {
                problems.push("\"name\" is required".to_string());
                String::new()
            }
        };

        let appearance = match document.get("appearance") {
            Some(Value::String(text)) => Appearance::parse(text).unwrap_or_else(|| {
                problems
                    .push(format!("\"appearance\" must be \"dark\" or \"light\", not {text:?}"));
                Appearance::Dark
            }),
            Some(_) => {
                problems.push("\"appearance\" must be \"dark\" or \"light\"".to_string());
                Appearance::Dark
            }
            None => {
                problems.push("\"appearance\" is required".to_string());
                Appearance::Dark
            }
        };

        let origin = document.get("origin").and_then(Value::as_str).map(str::to_string);

        let mut colors = BTreeMap::new();
        let source = match document.get("colors") {
            Some(Value::Object(colors)) => colors.clone(),
            Some(_) => {
                problems.push("\"colors\" must be an object".to_string());
                Map::new()
            }
            None => {
                problems.push("\"colors\" is required".to_string());
                Map::new()
            }
        };

        let ansi_keys: Vec<String> = (0..16).map(|slot| format!("ansi.{slot}")).collect();
        let required = REQUIRED_COLORS.iter().map(|k| k.to_string()).chain(ansi_keys);

        for key in required {
            match source.get(&key) {
                Some(Value::String(text)) => match color::parse(text) {
                    Ok(color) => {
                        colors.insert(key, color);
                    }
                    Err(error) => problems.push(format!("\"{key}\": {error}")),
                },
                Some(_) => problems.push(format!("\"{key}\" must be a colour string")),
                None => problems.push(format!("\"{key}\" is missing")),
            }
        }

        // Optional, with a deliberate fallback rather than black.
        for (key, fallback) in DERIVED_COLORS {
            let parsed = match source.get(key) {
                Some(Value::String(text)) => match color::parse(text) {
                    Ok(color) => Some(color),
                    Err(error) => {
                        problems.push(format!("\"{key}\": {error}"));
                        None
                    }
                },
                Some(_) => {
                    problems.push(format!("\"{key}\" must be a colour string"));
                    None
                }
                None => None,
            };
            if let Some(color) = parsed.or_else(|| colors.get(fallback).copied()) {
                colors.insert(key.to_string(), color);
            }
        }

        if problems.is_empty() {
            Ok(Self { name, appearance, origin, colors })
        } else {
            Err(ThemeError::Invalid { path: path.to_path_buf(), problems })
        }
    }

    /// Reads a theme file from disk.
    ///
    /// Blocking. Call it from `cx.background_spawn` (ADR-0007).
    pub fn load(path: &Path) -> Result<Self, ThemeError> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| ThemeError::Unreadable { path: path.to_path_buf(), source })?;
        Self::parse(path, &text)
    }

    /// Serialises to the native format, pretty-printed and newline-terminated.
    pub fn to_json(&self) -> String {
        let mut document = Map::new();
        document.insert("version".to_string(), Value::from(THEME_VERSION));
        document.insert("name".to_string(), Value::from(self.name.clone()));
        document.insert("appearance".to_string(), Value::from(self.appearance.name()));
        if let Some(origin) = &self.origin {
            document.insert("origin".to_string(), Value::from(origin.clone()));
        }

        let colors: Map<String, Value> = self
            .colors
            .iter()
            .map(|(key, value)| (key.clone(), Value::from(color::format(*value))))
            .collect();
        document.insert("colors".to_string(), Value::Object(colors));

        let mut text = serde_json::to_string_pretty(&Value::Object(document))
            .unwrap_or_else(|err| unreachable!("a JSON object failed to serialise: {err}"));
        text.push('\n');
        text
    }

    /// The colour under `key`, if the file defined one.
    ///
    /// Returns `Option` rather than a fallback, because the caller — the app's conversion
    /// into `Theme` — is the only place that knows what a sensible substitute is, and a
    /// silent black here is the exact failure the issue names.
    pub fn color(&self, key: &str) -> Option<Rgb> {
        self.colors.get(key).copied()
    }

    /// The sixteen ANSI slots, in order.
    ///
    /// Every slot is required by [`ThemeFile::parse`], so a parsed file always has all
    /// sixteen — the `Option` is for a `ThemeFile` built in memory by the importer, which
    /// fills them from the same source.
    pub fn ansi(&self) -> Option<[Rgb; 16]> {
        let mut slots = [0; 16];
        for (slot, out) in slots.iter_mut().enumerate() {
            *out = self.color(&format!("ansi.{slot}"))?;
        }
        Some(slots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("/nowhere/theme.json")
    }

    /// A minimal complete theme, for tests that need a valid file to perturb.
    pub(crate) fn complete_document() -> String {
        let mut colors = Map::new();
        for key in REQUIRED_COLORS {
            colors.insert(key.to_string(), Value::from("#101010"));
        }
        for slot in 0..16 {
            colors.insert(format!("ansi.{slot}"), Value::from("#202020"));
        }
        let mut document = Map::new();
        document.insert("version".to_string(), Value::from(THEME_VERSION));
        document.insert("name".to_string(), Value::from("Test"));
        document.insert("appearance".to_string(), Value::from("dark"));
        document.insert("colors".to_string(), Value::Object(colors));
        serde_json::to_string(&Value::Object(document)).unwrap()
    }

    #[test]
    fn a_complete_theme_parses() {
        let theme = ThemeFile::parse(&path(), &complete_document()).unwrap();

        assert_eq!(theme.name, "Test");
        assert_eq!(theme.appearance, Appearance::Dark);
        assert_eq!(theme.color("keyword"), Some(0x101010));
        assert_eq!(theme.ansi(), Some([0x202020; 16]));
    }

    #[test]
    fn every_problem_is_reported_at_once_rather_than_the_first() {
        let error = ThemeFile::parse(&path(), r#"{"name": "X", "appearance": "dark"}"#)
            .expect_err("no colours at all");

        let ThemeError::Invalid { problems, .. } = &error else { panic!("{error}") };
        assert!(problems.len() > 20, "one problem per missing key, not one for the file");
        assert!(error.to_string().contains("/nowhere/theme.json"), "{error}");
    }

    #[test]
    fn a_bad_colour_names_the_key_and_the_text() {
        let document =
            complete_document().replace(r##""keyword":"#101010""##, r#""keyword":"nope""#);
        let error = ThemeFile::parse(&path(), &document).expect_err("bad colour");

        let message = error.to_string();
        assert!(message.contains("keyword"), "{message}");
        assert!(message.contains("nope"), "{message}");
    }

    #[test]
    fn the_two_derived_diagnostics_fall_back_deliberately_and_not_to_black() {
        let theme = ThemeFile::parse(&path(), &complete_document()).unwrap();

        // Neither is in the fixture; both must still be present and equal to their source.
        assert_eq!(theme.color("information"), theme.color("accent"));
        assert_eq!(theme.color("hint"), theme.color("comment"));
        assert_ne!(theme.color("information"), Some(0x000000), "never black");
    }

    #[test]
    fn a_declared_information_colour_beats_the_fallback() {
        let document =
            complete_document().replace(r#""colors":{"#, r##""colors":{"information":"#abcdef","##);
        let theme = ThemeFile::parse(&path(), &document).unwrap();

        assert_eq!(theme.color("information"), Some(0xabcdef));
    }

    #[test]
    fn a_missing_name_or_appearance_is_reported_by_name() {
        let error = ThemeFile::parse(&path(), r#"{"colors": {}}"#).expect_err("no name");
        let message = error.to_string();
        assert!(message.contains("name"), "{message}");
        assert!(message.contains("appearance"), "{message}");
    }

    #[test]
    fn an_appearance_that_is_not_dark_or_light_says_so() {
        let document =
            complete_document().replace(r#""appearance":"dark""#, r#""appearance":"grey""#);
        let error = ThemeFile::parse(&path(), &document).expect_err("bad appearance");
        assert!(error.to_string().contains("grey"), "{error}");
    }

    #[test]
    fn malformed_json_names_the_file_and_the_position() {
        let error = ThemeFile::parse(&path(), "{\"name\": \"X\",}").expect_err("trailing comma");
        let message = error.to_string();
        assert!(message.contains("/nowhere/theme.json"), "{message}");
        assert!(message.contains("line 1"), "{message}");
    }

    #[test]
    fn a_json_array_is_not_a_theme() {
        let error = ThemeFile::parse(&path(), "[]").expect_err("not an object");
        assert!(matches!(error, ThemeError::NotAnObject { .. }), "{error}");
    }

    #[test]
    fn a_written_theme_reads_back_identically() {
        let theme = ThemeFile::parse(&path(), &complete_document()).unwrap();
        let text = theme.to_json();

        assert_eq!(ThemeFile::parse(&path(), &text).unwrap(), theme);
        assert!(text.ends_with("}\n"), "a POSIX text file ends in a newline");
        assert!(text.contains(&format!("\"version\": {THEME_VERSION}")), "{text}");
    }

    #[test]
    fn the_origin_is_carried_through_a_round_trip() {
        let mut theme = ThemeFile::parse(&path(), &complete_document()).unwrap();
        theme.origin = Some("github.github-vscode-theme v6.3.5, MIT".to_string());

        let read = ThemeFile::parse(&path(), &theme.to_json()).unwrap();
        assert_eq!(read.origin.as_deref(), Some("github.github-vscode-theme v6.3.5, MIT"));
    }
}
