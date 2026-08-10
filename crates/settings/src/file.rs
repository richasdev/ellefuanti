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
}
