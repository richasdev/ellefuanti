//! Reading, mutating and writing the settings document.

use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::SETTINGS_VERSION;

/// The key holding [`crate::SETTINGS_VERSION`]. Written on every save, tolerated absent.
const VERSION_KEY: &str = "version";

/// The key holding the theme name, matching `ThemeVariant`'s serialised form in the app
/// crate. A string rather than an integer so the file stays readable by the human editing
/// it, and so #58's disk themes — which have names and no enum arm — can use the same key.
const THEME_KEY: &str = "theme";

/// The theme a first run gets. Duplicated from `ThemeVariant::default()` in the app crate
/// rather than shared, because sharing it would mean this crate knowing about gpui
/// (ADR-0004). The app's `the_default_theme_name_agrees_across_the_crate_boundary` test
/// pins the two together, so a change to either without the other fails.
const DEFAULT_THEME: &str = "dark";

/// Font keys, VS Code's names so a settings file reads the way people expect.
///
/// `editor.fontFamily` is deliberately absent from the defaults below: "no family
/// configured" is a distinct state from "the user asked for Menlo", because the app walks a
/// fallback chain when nobody has chosen (#49). A default here would name one family and
/// make that chain unreachable.
const FONT_FAMILY_KEY: &str = "editor.fontFamily";
const FONT_SIZE_KEY: &str = "editor.fontSize";
const UI_FONT_SIZE_KEY: &str = "ui.fontSize";
const LINE_HEIGHT_KEY: &str = "editor.lineHeight";

/// The sizes and ratio a first run gets — the constants this replaced in `theme.rs`.
///
/// `LINE_HEIGHT` is a *multiplier*, not pixels: 20px against 13px text is a ratio someone
/// chose once and it stops meaning anything at 20px text. 20/13 is 1.538, rounded to the
/// 1.5 that every other editor defaults to — a third of a pixel at the old size, and
/// correct at every other size.
const DEFAULT_FONT_SIZE: f32 = 13.0;
const DEFAULT_UI_FONT_SIZE: f32 = 12.0;
const DEFAULT_LINE_HEIGHT: f32 = 1.5;

/// What a font size is allowed to be.
///
/// Clamped rather than rejected, because the failure is not symmetric: `"editor.fontSize":
/// 0` is a window of invisible text with no way to open settings and fix it, and a negative
/// size reaches gpui's layout as a negative `Pixels`. The bounds are generous enough that
/// nobody with a real preference meets them.
const MIN_FONT_SIZE: f32 = 6.0;
const MAX_FONT_SIZE: f32 = 96.0;

/// What a line-height multiplier is allowed to be. Below 1.0 rows overlap; above 3.0 is
/// not a preference anyone holds, and both ends are cheaper to clamp than to explain.
const MIN_LINE_HEIGHT: f32 = 1.0;
const MAX_LINE_HEIGHT: f32 = 3.0;

/// What went wrong reading a settings file, in terms the user can act on.
///
/// Never a reason not to launch. Every variant here resolves to "run on defaults", and the
/// caller's job is to say so out loud — a settings file that is silently ignored is worse
/// than one that fails loudly, because the user keeps editing it.
#[derive(Debug)]
pub enum SettingsError {
    /// The file exists but is not readable — permissions, or a directory where a file
    /// should be.
    Unreadable { path: PathBuf, source: std::io::Error },
    /// The file is not valid JSON. Carries serde_json's line and column, which is the
    /// part that makes this fixable rather than merely reported.
    Malformed { path: PathBuf, source: serde_json::Error },
    /// Valid JSON, but not an object — `[1, 2, 3]` or `"hello"`. There is no per-key
    /// recovery from this because there are no keys.
    NotAnObject { path: PathBuf, found: &'static str },
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, source } => {
                write!(f, "could not read {}: {source}", path.display())
            }
            Self::Malformed { path, source } => {
                write!(f, "{} is not valid JSON: {source}", path.display())
            }
            Self::NotAnObject { path, found } => write!(
                f,
                "{} should contain a JSON object like {{\"theme\": \"dark\"}}, but contains \
                 a {found}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            Self::Malformed { source, .. } => Some(source),
            Self::NotAnObject { .. } => None,
        }
    }
}

/// The result of a read: settings that are always usable, plus what went wrong getting
/// them.
///
/// Not a `Result`, on purpose. Every failure mode still produces working settings, so
/// returning `Err` would push each caller into writing the same "…but carry on with
/// defaults" branch, and the one that forgot would be a launch failure caused by a typo in
/// a config file.
pub struct Load {
    pub settings: Settings,
    /// `Some` when the file could not be used at all. The whole document fell back to
    /// defaults; nothing was lost, because nothing was written.
    pub error: Option<SettingsError>,
    /// `Some(version)` when the file declared a version this build does not know. Not an
    /// error: keys we understand are still read, and everything else is preserved on
    /// write. Worth surfacing because it usually means "you downgraded".
    pub unknown_version: Option<u64>,
}

/// Every user-configurable value, as the document that holds them.
///
/// The `Map` is the storage rather than a struct with named fields, and that is the whole
/// unknown-key guarantee: there is no step at which a key we do not recognise is dropped,
/// because there is no step at which the document is projected onto a fixed shape. A
/// struct with `#[serde(flatten)] extra: Map` would work too, and would put two sources of
/// truth in one type for the sake of dot access on four keys.
///
/// The cost is that reads are a lookup and a type check instead of a field access. At the
/// rate settings are read — once at startup, once per change — that is not a cost.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    document: Map<String, Value>,
}

impl Settings {
    /// Parses a document. Anything that is not a JSON object is a hard failure, because
    /// there is nothing to read keys out of.
    ///
    /// Note what is *not* validated here: individual key types. A `"theme": 7` parses
    /// fine and is caught at read time by [`Settings::theme`], which is what keeps one bad
    /// key from costing the file.
    pub fn parse(path: &Path, text: &str) -> Result<Self, SettingsError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|source| SettingsError::Malformed { path: path.to_path_buf(), source })?;

        match value {
            Value::Object(document) => Ok(Self { document }),
            other => Err(SettingsError::NotAnObject {
                path: path.to_path_buf(),
                found: type_name(&other),
            }),
        }
    }

    /// Reads the settings file, falling back to defaults for anything that goes wrong.
    ///
    /// A missing file is not a failure — it is the first run, and the reason every key has
    /// a default.
    ///
    /// Blocking. Call it from `cx.background_spawn` (ADR-0007), or, as `main` does, once
    /// before the window exists where a few hundred microseconds of `read_to_string` is
    /// cheaper than the machinery to defer it.
    pub fn load(path: &Path) -> Load {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Load::defaults(),
            Err(source) => {
                return Load {
                    settings: Self::default(),
                    error: Some(SettingsError::Unreadable { path: path.to_path_buf(), source }),
                    unknown_version: None,
                };
            }
        };

        match Self::parse(path, &text) {
            Ok(settings) => {
                let unknown_version = settings.declared_version().filter(|v| *v > SETTINGS_VERSION);
                Load { settings, error: None, unknown_version }
            }
            Err(error) => {
                Load { settings: Self::default(), error: Some(error), unknown_version: None }
            }
        }
    }

    /// The version the file declared, if it declared one this crate can make sense of.
    ///
    /// An absent version means a file written before versioning, or by hand — both are
    /// current-version files as far as this build is concerned. A version that is not a
    /// number is a malformed key like any other and reads as absent.
    fn declared_version(&self) -> Option<u64> {
        self.document.get(VERSION_KEY)?.as_u64()
    }

    /// The active theme's name, or the default if the key is absent or not a string.
    ///
    /// Not validated against the list of real themes: this crate does not know what themes
    /// exist (ADR-0004), and the app resolves an unrecognised name to its default. Which
    /// means a typo'd theme name keeps its spelling in the file and is therefore still
    /// there to be corrected, rather than being helpfully rewritten to "dark".
    /// Whether dirty tabs save themselves when the window loses focus.
    ///
    /// **On by default** — the owner's reference IDEs (PhpStorm, TablePlus) autosave,
    /// and the first real rename session ended with "não fica um dot… nem vai pro
    /// source control" precisely because an applied edit sat unsaved in a buffer.
    /// `"autosave": false` turns it off for whoever prefers explicit ⌘S.
    pub fn autosave(&self) -> bool {
        match self.document.get("autosave") {
            Some(Value::Bool(enabled)) => *enabled,
            Some(other) => {
                tracing::warn!(
                    key = "autosave",
                    found = type_name(other),
                    "settings: expected a bool, using the default"
                );
                true
            }
            None => true,
        }
    }

    pub fn theme(&self) -> &str {
        match self.document.get(THEME_KEY) {
            Some(Value::String(name)) => name,
            Some(other) => {
                tracing::warn!(
                    key = THEME_KEY,
                    found = type_name(other),
                    "settings: expected a string, using the default"
                );
                DEFAULT_THEME
            }
            None => DEFAULT_THEME,
        }
    }

    /// Sets the theme name. Only mutates that key; everything else in the document,
    /// recognised or not, is untouched.
    pub fn set_theme(&mut self, name: &str) {
        self.document.insert(THEME_KEY.to_string(), Value::String(name.to_string()));
    }

    /// The font family the user asked for, or `None` if they did not ask.
    ///
    /// `None` is not "Menlo". Whether the named family even exists, and whether it is
    /// monospaced, is the app's question — this crate cannot open a font (ADR-0004), and
    /// answering "is Comic Sans monospaced" from here would mean guessing. So an absent key
    /// means "walk the fallback chain" and a present one means "try this first", and the
    /// app decides what either resolves to.
    pub fn font_family(&self) -> Option<&str> {
        match self.document.get(FONT_FAMILY_KEY)? {
            Value::String(family) => Some(family),
            other => {
                tracing::warn!(
                    key = FONT_FAMILY_KEY,
                    found = type_name(other),
                    "settings: expected a string, using the default"
                );
                None
            }
        }
    }

    /// Editor text size in pixels, clamped to something legible.
    pub fn font_size(&self) -> f32 {
        self.number(FONT_SIZE_KEY, DEFAULT_FONT_SIZE).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    }

    /// Size of the chrome — tabs, sidebar, status bar. Independent of the editor's,
    /// because someone reading code at 18px does not want an 18px status bar.
    pub fn ui_font_size(&self) -> f32 {
        self.number(UI_FONT_SIZE_KEY, DEFAULT_UI_FONT_SIZE).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    }

    /// Line height as a multiple of the editor font size, clamped so rows cannot overlap.
    pub fn line_height(&self) -> f32 {
        self.number(LINE_HEIGHT_KEY, DEFAULT_LINE_HEIGHT).clamp(MIN_LINE_HEIGHT, MAX_LINE_HEIGHT)
    }

    /// Writes the editor font size. Used by the zoom actions, which are the only thing that
    /// changes a size without the user opening the file.
    pub fn set_font_size(&mut self, size: f32) {
        let size = f64::from(size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE));
        self.document.insert(FONT_SIZE_KEY.to_string(), Value::from(size));
    }

    /// Writes the editor font family. The settings panel's picker is the caller; the app
    /// has already applied the monospace check by the time a name reaches here (#85's rule:
    /// this crate cannot open a font, so it must not pretend to judge one).
    pub fn set_font_family(&mut self, family: &str) {
        self.document.insert(FONT_FAMILY_KEY.to_string(), Value::String(family.to_string()));
    }

    /// Writes the autosave flag (#25 follow-up: a toggle in the panel).
    pub fn set_autosave(&mut self, enabled: bool) {
        self.document.insert("autosave".to_string(), Value::Bool(enabled));
    }

    /// Writes the chrome font size, clamped like the read is.
    pub fn set_ui_font_size(&mut self, size: f32) {
        let size = f64::from(size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE));
        self.document.insert(UI_FONT_SIZE_KEY.to_string(), Value::from(size));
    }

    /// Writes the line-height multiplier, clamped so a write cannot make rows overlap —
    /// the same bound the read applies, for `set_font_size`'s reason: the two paths must
    /// not disagree about what a legal value is.
    pub fn set_line_height(&mut self, height: f32) {
        let height = f64::from(height.clamp(MIN_LINE_HEIGHT, MAX_LINE_HEIGHT));
        self.document.insert(LINE_HEIGHT_KEY.to_string(), Value::from(height));
    }

    /// A numeric key, or its default if absent or the wrong type.
    ///
    /// No `is_finite` guard, and that is checked rather than assumed: `serde_json` has no
    /// `NaN` or `Infinity` literals and rejects an out-of-range `1e400` as *malformed JSON*
    /// at parse time, so a `Value::Number` that reached this document is finite. The
    /// caller's `clamp` handles the rest, and a `clamp` on a finite `f32` cannot produce
    /// one that is not. A guard here was written first and removed once the parse error
    /// proved it unreachable — see the test.
    fn number(&self, key: &str, default: f32) -> f32 {
        match self.document.get(key) {
            Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::from(default)) as f32,
            Some(other) => {
                tracing::warn!(
                    key,
                    found = type_name(other),
                    "settings: expected a number, using the default"
                );
                default
            }
            None => default,
        }
    }

    /// Serialises to the exact bytes [`Settings::save`] writes.
    ///
    /// Pretty-printed with a trailing newline, because this file's primary editor is a
    /// human with a text editor, and a POSIX text file ends in a newline.
    pub fn to_json(&self) -> String {
        let mut document = self.document.clone();
        // Stamped on write rather than on load, so reading a file never has the side
        // effect of claiming it is ours. `Map` is serde_json's default `BTreeMap`, so this
        // sorts to the front and stays there — the version being the first line is worth
        // the one-line dependency on that.
        document.insert(VERSION_KEY.to_string(), Value::from(SETTINGS_VERSION));

        let mut text = serde_json::to_string_pretty(&Value::Object(document))
            // `Map<String, Value>` has no non-string keys and no NaN — the two ways this
            // can fail — so the error arm is unreachable rather than merely unlikely.
            .unwrap_or_else(|err| unreachable!("a JSON object failed to serialise: {err}"));
        text.push('\n');
        text
    }

    /// Writes the file atomically: temp file in the same directory, then rename.
    ///
    /// Same directory because `rename` is only atomic within a filesystem, and `/tmp` is
    /// its own volume on macOS. Named after the process so two ellefuanti windows saving
    /// at the same moment cannot truncate each other's temp file — the rename still races,
    /// but a race between two complete documents leaves a complete document, which is the
    /// property that matters.
    ///
    /// The parent directory is created if missing; a first run has no
    /// `Application Support/ellefuanti` yet.
    ///
    /// Blocking. Wrap it in `cx.background_spawn` (ADR-0007).
    pub fn save(&self, path: &Path) -> Result<(), anyhow::Error> {
        use anyhow::Context as _;

        let directory = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(directory)
            .with_context(|| format!("could not create {}", directory.display()))?;

        let temporary = directory.join(format!(
            ".{}.{}.tmp",
            path.file_name().unwrap_or_else(|| "settings.json".as_ref()).to_string_lossy(),
            std::process::id()
        ));

        // Explicitly not `File::create` plus `write_all`: `fs::write` truncates and writes
        // in one call, and the file being truncated here is one nobody reads.
        std::fs::write(&temporary, self.to_json())
            .with_context(|| format!("could not write {}", temporary.display()))?;

        std::fs::rename(&temporary, path).inspect_err(|_| {
            // A failed rename leaves the temp file behind, and a directory that
            // accumulates `.settings.json.4821.tmp` is its own small bug. Best-effort:
            // if the cleanup fails too, the rename error is the one worth reporting.
            let _ = std::fs::remove_file(&temporary);
        })?;

        Ok(())
    }
}

impl Load {
    fn defaults() -> Self {
        Self { settings: Settings::default(), error: None, unknown_version: None }
    }
}

/// JSON's name for a value's type, for error messages a user can act on.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("/nowhere/settings.json")
    }

    #[test]
    fn no_file_at_all_gives_working_defaults() {
        let load = Settings::load(Path::new("/nowhere/there/is/no/settings.json"));

        assert_eq!(load.settings.theme(), DEFAULT_THEME);
        assert!(load.error.is_none(), "a missing file is a first run, not a failure");
        assert!(load.unknown_version.is_none());
    }

    #[test]
    fn an_empty_object_is_the_same_as_no_file() {
        let settings = Settings::parse(&path(), "{}").unwrap();
        assert_eq!(settings.theme(), DEFAULT_THEME);
    }

    #[test]
    fn the_panel_setters_round_trip_and_clamp() {
        // The four fields the settings panel writes (#100). Clamps must match the reads —
        // a panel that can write 2.0 while the read clamps to 3.0 would show a value the
        // next launch silently changes.
        let mut settings = Settings::parse(&path(), r#"{"custom.key": "survives"}"#).unwrap();

        settings.set_font_family("Fira Code");
        settings.set_ui_font_size(500.0);
        settings.set_line_height(0.1);

        assert_eq!(settings.font_family(), Some("Fira Code"));
        assert_eq!(settings.ui_font_size(), MAX_FONT_SIZE, "writes clamp like reads");
        assert_eq!(settings.line_height(), MIN_LINE_HEIGHT);
        // The reason the document is a Map: a panel write must not cost the user a key
        // this build does not understand (#60's rule, extended to the new setters).
        assert_eq!(settings.document.get("custom.key"), Some(&Value::String("survives".into())));
    }

    #[test]
    fn malformed_json_names_the_file_and_the_position() {
        let error = Settings::parse(&path(), "{\"theme\": \"dark\",}").expect_err("trailing comma");

        let message = error.to_string();
        assert!(message.contains("/nowhere/settings.json"), "{message}");
        assert!(message.contains("line 1"), "the position is what makes this fixable: {message}");
    }

    #[test]
    fn a_json_array_is_rejected_by_name() {
        let error = Settings::parse(&path(), "[1, 2, 3]").expect_err("not an object");
        assert!(error.to_string().contains("array"), "{error}");
    }

    /// The constraint that separates this from `elle-index`: one wrong key costs one key.
    #[test]
    fn one_key_of_the_wrong_type_does_not_discard_the_others() {
        let settings = Settings::parse(&path(), r#"{"theme": 7, "editor.fontSize": 15}"#).unwrap();

        assert_eq!(settings.theme(), DEFAULT_THEME, "the bad key falls back");
        assert!(
            settings.to_json().contains("editor.fontSize"),
            "and its neighbours survive: {}",
            settings.to_json()
        );
    }

    #[test]
    fn a_theme_name_this_build_does_not_know_is_kept_verbatim() {
        // Resolving names is the app's job. Rewriting an unrecognised one to "dark" here
        // would destroy the typo the user needs to see in order to fix it.
        let settings = Settings::parse(&path(), r#"{"theme": "solarized-lite"}"#).unwrap();
        assert_eq!(settings.theme(), "solarized-lite");
    }

    #[test]
    fn setting_a_key_leaves_every_other_key_alone() {
        let mut settings =
            Settings::parse(&path(), r#"{"theme": "light", "somethingElse": {"a": [1]}}"#).unwrap();
        settings.set_theme("github-dark");

        let text = settings.to_json();
        assert!(text.contains("github-dark"));
        assert!(text.contains("somethingElse"), "{text}");
        assert!(text.contains("\"a\""), "nested values too: {text}");
    }

    #[test]
    fn a_version_is_written_even_when_the_file_had_none() {
        let settings = Settings::default();
        let text = settings.to_json();
        assert!(text.contains(&format!("\"version\": {SETTINGS_VERSION}")), "{text}");
    }

    #[test]
    fn a_version_from_the_future_is_reported_but_still_read() {
        let text = format!(r#"{{"version": {}, "theme": "light"}}"#, SETTINGS_VERSION + 1);
        let settings = Settings::parse(&path(), &text).unwrap();
        let load = Load {
            unknown_version: settings.declared_version().filter(|v| *v > SETTINGS_VERSION),
            settings,
            error: None,
        };

        assert_eq!(load.unknown_version, Some(SETTINGS_VERSION + 1));
        assert_eq!(load.settings.theme(), "light", "a newer file is still read, not discarded");
    }

    #[test]
    fn a_version_that_is_not_a_number_reads_as_absent() {
        let settings = Settings::parse(&path(), r#"{"version": "one"}"#).unwrap();
        assert_eq!(settings.declared_version(), None);
    }

    #[test]
    fn the_written_file_ends_in_a_newline() {
        assert!(Settings::default().to_json().ends_with("}\n"));
    }

    // --- fonts (#49) --------------------------------------------------------------------

    #[test]
    fn no_font_keys_at_all_gives_the_old_compiled_in_values() {
        // These were `Metrics::FONT_SIZE` and friends until #49. A first run must look
        // exactly like the build before the setting existed.
        let settings = Settings::default();
        assert_eq!(settings.font_family(), None, "absent means 'walk the fallback chain'");
        assert_eq!(settings.font_size(), 13.0);
        assert_eq!(settings.ui_font_size(), 12.0);
        assert_eq!(settings.line_height(), 1.5);
    }

    #[test]
    fn the_font_keys_read_back_what_was_written() {
        let text = r#"{
            "editor.fontFamily": "Berkeley Mono",
            "editor.fontSize": 16,
            "ui.fontSize": 14,
            "editor.lineHeight": 1.8
        }"#;
        let settings = Settings::parse(&path(), text).unwrap();

        assert_eq!(settings.font_family(), Some("Berkeley Mono"));
        assert_eq!(settings.font_size(), 16.0);
        assert_eq!(settings.ui_font_size(), 14.0);
        assert_eq!(settings.line_height(), 1.8);
    }

    /// A family this build cannot use keeps its spelling, exactly as a theme name does.
    ///
    /// Whether "Comic Sans" is monospaced is not decidable here (ADR-0004) — the app
    /// refuses it against real font metrics. What this pins is that refusing it must not
    /// mean *rewriting* it, or the user loses the typo they need to see.
    #[test]
    fn a_font_family_this_crate_cannot_judge_is_kept_verbatim() {
        let mut settings =
            Settings::parse(&path(), r#"{"editor.fontFamily": "Comic Sans"}"#).unwrap();
        assert_eq!(settings.font_family(), Some("Comic Sans"));

        settings.set_theme("light");
        assert!(settings.to_json().contains("Comic Sans"), "{}", settings.to_json());
    }

    /// Sizes are clamped rather than rejected, and the reason is asymmetric: a 0px editor
    /// is a window of invisible text with no way to open settings and fix it.
    #[test]
    fn a_size_that_would_make_the_editor_unusable_is_clamped() {
        for (json, expected) in [
            (r#"{"editor.fontSize": 0}"#, MIN_FONT_SIZE),
            (r#"{"editor.fontSize": -13}"#, MIN_FONT_SIZE),
            (r#"{"editor.fontSize": 100000}"#, MAX_FONT_SIZE),
        ] {
            let settings = Settings::parse(&path(), json).unwrap();
            assert_eq!(settings.font_size(), expected, "{json}");
            assert!(settings.font_size().is_finite());
        }
    }

    /// Why [`Settings::number`] has no `is_finite` guard.
    ///
    /// It had one, defending against `1e400` parsing to `f64::INFINITY` and surviving
    /// `clamp` as infinity. It cannot: serde_json rejects an out-of-range float as
    /// malformed JSON before a `Value::Number` ever exists, so the whole file falls back to
    /// defaults — the branch was unreachable. This pins the parser behaviour the removal
    /// depends on, so a serde_json that starts accepting `1e400` as infinity fails here
    /// rather than in someone's layout.
    #[test]
    fn an_out_of_range_float_is_a_parse_error_not_an_infinity() {
        let error = Settings::parse(&path(), r#"{"editor.fontSize": 1e400}"#)
            .expect_err("serde_json must reject this rather than yield infinity");
        assert!(matches!(error, SettingsError::Malformed { .. }), "{error}");
    }

    #[test]
    fn a_line_height_below_one_would_overlap_rows_and_is_clamped() {
        let settings = Settings::parse(&path(), r#"{"editor.lineHeight": 0.2}"#).unwrap();
        assert_eq!(settings.line_height(), MIN_LINE_HEIGHT);
    }

    /// The old absolute pixel value, typed into the new multiplier key.
    ///
    /// Someone migrating by hand will write `20`. That is not 20 px of line — it is a 20x
    /// multiplier, i.e. a 260 px row — so the clamp is what stands between them and one
    /// visible line of code. Not silently reinterpreted as pixels: guessing which of two
    /// units a number meant is how a setting becomes unpredictable.
    #[test]
    fn the_old_absolute_line_height_clamps_instead_of_producing_a_260px_row() {
        let settings = Settings::parse(&path(), r#"{"editor.lineHeight": 20}"#).unwrap();
        assert_eq!(settings.line_height(), MAX_LINE_HEIGHT);
    }

    #[test]
    fn a_font_key_of_the_wrong_type_falls_back_without_touching_its_neighbours() {
        let text = r#"{"editor.fontFamily": 12, "editor.fontSize": "big", "theme": "light"}"#;
        let settings = Settings::parse(&path(), text).unwrap();

        assert_eq!(settings.font_family(), None);
        assert_eq!(settings.font_size(), DEFAULT_FONT_SIZE);
        assert_eq!(settings.theme(), "light", "one bad key costs one key");
    }

    #[test]
    fn setting_the_font_size_leaves_every_other_key_alone() {
        let mut settings =
            Settings::parse(&path(), r#"{"theme": "light", "editor.fontFamily": "Menlo"}"#)
                .unwrap();
        settings.set_font_size(18.0);

        assert_eq!(settings.font_size(), 18.0);
        assert_eq!(settings.theme(), "light");
        assert_eq!(settings.font_family(), Some("Menlo"));
    }

    /// The zoom actions call this in a loop; the clamp has to hold at the setter too, or a
    /// held ⌘- walks the size to zero and writes it to the file.
    #[test]
    fn the_setter_clamps_as_well_as_the_getter() {
        let mut settings = Settings::default();
        settings.set_font_size(-5.0);
        assert_eq!(settings.font_size(), MIN_FONT_SIZE);

        settings.set_font_size(1_000.0);
        assert_eq!(settings.font_size(), MAX_FONT_SIZE);
    }
    #[test]
    fn autosave_defaults_on_and_set_persists() {
        // The panel toggle writes through set_autosave; the read is the same key.
        let mut settings = Settings::default();
        assert!(settings.autosave(), "default on");
        settings.set_autosave(false);
        assert!(!settings.autosave(), "the write is read back");
        settings.set_autosave(true);
        assert!(settings.autosave());
    }
}
