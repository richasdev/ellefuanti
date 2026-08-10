//! Themes on disk: a native format, and an importer for VS Code's.
//!
//! Plain Rust, no gpui (ADR-0004). This crate answers "what colours does this file
//! describe" and hands back a [`ThemeFile`] of `u32` RGB values; turning those into the
//! app's `Theme` of `Hsla` fields is the app crate's job, because `Theme` is a gpui type.
//!
//! Blocking IO, like every other infrastructure crate here — the caller wraps it in
//! `cx.background_spawn` (ADR-0007).
//!
//! # Why the built-ins do not go through this
//!
//! `Theme::dark()` and the four others stay compiled in. The issue's constraint is that a
//! missing or corrupt `assets/themes/` must still launch with Dark available, and the
//! cheapest way to guarantee that is for the default theme never to touch a parser. Disk
//! themes are strictly additional: a file that fails to load is a log line and the current
//! theme, never a window with no colours in it.
//!
//! # The trust boundary
//!
//! A theme file is something a user edited. Every failure path here names the file and the
//! problem and leaves the caller holding a working theme — see [`ThemeError`]. The related
//! trap #76 hit is not repeated: nothing in this crate writes to a file it has read, so
//! there is no path on which a malformed theme is overwritten with defaults.

mod color;
mod file;
mod import;
mod scope;

pub use color::{ColorError, Rgb, format as format_color, parse as parse_color};
pub use file::{REQUIRED_COLORS, ThemeError, ThemeFile};
pub use import::{VsCodeTheme, import};
pub use scope::{Rule, resolve, resolve_any};

use std::path::{Path, PathBuf};

/// Bumped when a *readable* older theme file would be understood wrongly by this build — a
/// key whose meaning changed, not a key that was added.
///
/// From the first commit, same reasoning as `elle-settings`'s `SETTINGS_VERSION` and
/// `elle-index`'s `SCHEMA_VERSION` (ADR-0008): retrofitting a version means guessing what
/// version an unlabelled file is.
///
/// The recovery is `elle-settings`'s rather than `elle-index`'s. A theme file is not a
/// cache and may not be deleted, so a version this build does not recognise is a warning
/// and a best-effort read, not a discard.
pub const THEME_VERSION: u64 = 1;

/// Whether a theme is meant for a dark or a light background.
///
/// Not cosmetic metadata. #48 established that the terminal's ANSI readability fixes are
/// background-specific: lifting slot 0 off the background so it is visible is correct on
/// dark and actively wrong on light, where slot 0 is genuinely black. The importer needs
/// this before it can build a palette, which is why it infers one when a file does not
/// declare it — see [`VsCodeTheme::appearance`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    /// Parses VS Code's `type` and this format's `appearance`, which are the same two words.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            // `hc` and `hcLight` are VS Code's high-contrast themes. Not mapped: they are a
            // different accessibility promise, and treating one as an ordinary dark theme
            // would silently discard the contrast guarantee the user chose it for.
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// `~/Library/Application Support/ellefuanti/themes`, where a user drops their own themes.
///
/// Beside `settings.json` rather than inside a cache directory, for the reason
/// `elle-settings` states: this is something a human made and there is no source to rebuild
/// it from.
///
/// `None` when `HOME` is unset, which means no user themes for the launch — the compiled-in
/// ones are unaffected, which is the point of them not going through this crate.
pub fn user_themes_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/ellefuanti/themes"))
}

/// Every theme in a directory, and every reason one did not load.
///
/// Both halves are returned because both matter: the caller shows what loaded and logs what
/// did not. A directory that does not exist is neither — it is the normal state of a machine
/// where nobody has added a theme, and it yields no themes and no errors.
///
/// Blocking. Call it from `cx.background_spawn` (ADR-0007).
pub fn load_dir(dir: &Path) -> (Vec<ThemeFile>, Vec<ThemeError>) {
    let mut themes = Vec::new();
    let mut errors = Vec::new();

    let Ok(entries) = std::fs::read_dir(dir) else {
        return (themes, errors);
    };

    // Sorted, so the palette lists themes in the same order on every launch rather than in
    // whatever order the filesystem hands them back.
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .collect();
    paths.sort();

    for path in paths {
        match ThemeFile::load(&path) {
            Ok(theme) => themes.push(theme),
            Err(error) => errors.push(error),
        }
    }

    (themes, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_parses_both_spellings_and_rejects_high_contrast() {
        assert_eq!(Appearance::parse("dark"), Some(Appearance::Dark));
        assert_eq!(Appearance::parse("Light"), Some(Appearance::Light));
        assert_eq!(Appearance::parse(" dark "), Some(Appearance::Dark));
        assert_eq!(Appearance::parse("hc"), None, "high contrast is a different promise");
        assert_eq!(Appearance::parse(""), None);
    }

    #[test]
    fn the_user_theme_directory_sits_beside_the_settings_file() {
        let Some(dir) = user_themes_dir() else { return };
        assert!(dir.ends_with("Library/Application Support/ellefuanti/themes"), "{dir:?}");
    }

    #[test]
    fn a_directory_that_does_not_exist_is_not_an_error() {
        let (themes, errors) = load_dir(Path::new("/nowhere/there/are/no/themes"));
        assert!(themes.is_empty());
        assert!(errors.is_empty(), "an absent directory is the normal state, not a failure");
    }
}
