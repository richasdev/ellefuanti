//! The themes available this launch: five compiled in, plus whatever loaded from disk.
//!
//! Where `theme.rs` owns what a theme *is*, this owns which ones exist. The split matters
//! for #58's hard constraint — **a missing or corrupt `assets/themes/` must still launch
//! with Dark available** — because the built-in path in `theme.rs` never consults anything
//! here. A registry that fails to populate is an empty registry, not a broken editor.
//!
//! Two directories are read, in this order:
//!
//! - `assets/themes/`, shipped with the binary. Distribution, so anything in it records its
//!   origin and licence; see that directory's README.
//! - `~/Library/Application Support/ellefuanti/themes/`, the user's own. Theirs, and their
//!   business — the licensing question does not arise for a file they put there.
//!
//! A user theme with the same name as a shipped one wins, which is the only precedence rule
//! here and the one that makes "copy the shipped theme and tweak it" work.

use std::path::PathBuf;

use elle_theme::{ThemeError, ThemeFile};
use gpui::{App, Global};

use crate::theme::{ThemeChoice, ThemeVariant, Themed as _, set_disk_theme, set_theme};

/// Every disk theme this launch found, by name.
///
/// Sorted, because it backs a list in the command palette and a list that reorders itself
/// between launches is one nobody can build muscle memory for. `Vec` rather than a map: it
/// is read by iteration far more often than by lookup, and five to fifty entries is not a
/// size where that distinction is worth a second container.
#[derive(Default)]
pub struct DiskThemes {
    themes: Vec<ThemeFile>,
}

impl Global for DiskThemes {}

impl DiskThemes {
    /// The theme with this name, if one loaded.
    pub fn get(&self, name: &str) -> Option<&ThemeFile> {
        self.themes.iter().find(|theme| theme.name == name)
    }

    /// Every loaded theme's name, for the palette.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.themes.iter().map(|theme| theme.name.as_str())
    }
}

/// Where themes are read from, in increasing order of precedence.
///
/// `assets/themes/` is resolved relative to the executable, not the working directory: an
/// installed `.app` is launched from wherever Finder happens to be, and a relative path
/// would find themes only when run from the repository root.
fn theme_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();

    if let Ok(executable) = std::env::current_exe() {
        // `Contents/MacOS/ellefuanti` inside a bundle, `target/debug/ellefuanti` in a
        // checkout. Both put the assets one level up from the binary's directory.
        if let Some(bundled) = executable.parent().map(|dir| dir.join("../Resources/themes")) {
            directories.push(bundled);
        }
    }

    // The repository layout, for `cargo run`. Harmless when absent, which is what it is in
    // an installed build.
    directories.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/themes"));

    directories.extend(elle_theme::user_themes_dir());
    directories
}

/// Loads every theme on disk and installs the registry.
///
/// **Every failure is a log line and nothing else.** A theme that does not parse is named,
/// along with what was wrong with it, and the launch continues with one fewer theme — which
/// is the trust-boundary rule from ADR-0009 applied to a second kind of user-edited file.
/// Note what is *not* here: nothing writes, so there is no equivalent of the #76 trap where
/// a malformed file loaded as defaults could be saved back over the user's real one.
///
/// Blocking, and called at startup beside the settings read. A directory listing of a
/// handful of small files is the same order of cost as the settings `read_to_string` that
/// ADR-0007's exception already covers; if this grows a recursive walk it moves to
/// `cx.background_spawn`.
pub fn load(cx: &mut App) {
    let mut themes: Vec<ThemeFile> = Vec::new();
    let mut errors: Vec<ThemeError> = Vec::new();

    for directory in theme_directories() {
        let (loaded, failed) = elle_theme::load_dir(&directory);
        for theme in loaded {
            // Later directories win, so a user theme shadows a shipped one of the same name.
            themes.retain(|existing| existing.name != theme.name);
            themes.push(theme);
        }
        errors.extend(failed);
    }

    for error in &errors {
        tracing::error!("{error}; this theme is unavailable for the launch");
    }

    themes.sort_by(|a, b| a.name.cmp(&b.name));
    if !themes.is_empty() {
        tracing::info!(count = themes.len(), "loaded themes from disk");
    }

    cx.set_global(DiskThemes { themes });
}

/// Applies the theme a settings file named, whether it is compiled in or on disk.
///
/// The resolution order is the one `ThemeChoice::from_name` states: a built-in wins its own
/// name, so no file can shadow Dark. A name that is neither falls back to the default and
/// says which name it did not recognise — and, as in #76, does **not** rewrite the file, so
/// the typo the user needs to see is still there to be corrected.
/// Every selectable theme name, for the settings panel's picker (#100).
///
/// Built-ins first, then disk themes (#58), which is precedence order — the same list the
/// palette offers, so the two ways of choosing a theme cannot disagree about what exists.
pub fn selectable_names(cx: &App) -> Vec<String> {
    let mut names: Vec<String> =
        crate::theme::ThemeVariant::ALL.iter().map(|variant| variant.name().to_string()).collect();
    if let Some(disk) = cx.try_global::<DiskThemes>() {
        for name in disk.names() {
            if !names.iter().any(|existing| existing == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

pub fn apply_named(name: &str, cx: &mut App) {
    match ThemeChoice::from_name(name) {
        ThemeChoice::BuiltIn(variant) => set_theme(variant, cx),
        ThemeChoice::Disk(name) => {
            // `try_global` for the same reason as in `cycle`: no registry is an empty
            // registry, which resolves to the default rather than to a panic.
            let found = cx.try_global::<DiskThemes>().and_then(|themes| themes.get(&name)).cloned();
            match found {
                Some(theme) => set_disk_theme(&theme, ThemeVariant::default(), cx),
                None => {
                    let fallback = ThemeVariant::default();
                    tracing::warn!(
                        theme = name,
                        using = fallback.name(),
                        "no such theme, compiled in or on disk"
                    );
                    set_theme(fallback, cx);
                }
            }
        }
    }
}

/// Switches to the next theme and returns its label, for `theme.toggle`.
///
/// The order is the five built-ins in `ThemeVariant::next()`'s order, then every disk theme
/// alphabetically, then back to the start. Disk themes come last so the cycle a user already
/// knows is unchanged up to the point where their own themes begin.
///
/// Falls back to the built-in cycle when nothing loaded from disk, which is the common case
/// and the one that must not get slower or stranger for the feature existing.
pub fn cycle(cx: &mut App) -> String {
    // `try_global`, not `global`: a render test never calls `load`, and the built-in cycle
    // must not depend on the registry existing. Same reasoning as `LiveSettings` being
    // absent meaning "do not persist" — the disk layer is additional, never required.
    let disk: Vec<String> = cx
        .try_global::<DiskThemes>()
        .map(|themes| themes.names().map(str::to_string).collect())
        .unwrap_or_default();

    if disk.is_empty() {
        let next = cx.theme_variant().next();
        set_theme(next, cx);
        return next.label().to_string();
    }

    let last_builtin = ThemeVariant::default().label().to_string();
    match cx.theme_choice() {
        // Off the end of the built-ins is the first disk theme, rather than round to Dark.
        ThemeChoice::BuiltIn(variant) if variant.next() == ThemeVariant::default() => {
            let name = disk[0].clone();
            apply_named(&name, cx);
            name
        }
        ThemeChoice::BuiltIn(variant) => {
            let next = variant.next();
            set_theme(next, cx);
            next.label().to_string()
        }
        ThemeChoice::Disk(current) => {
            // The next disk theme, or back to the first built-in once they are exhausted.
            // `position` returning `None` means the active theme has vanished from the
            // registry, which cannot happen without a reload but resolves sanely anyway.
            let next =
                disk.iter().position(|name| *name == current).and_then(|index| disk.get(index + 1));
            match next {
                Some(name) => {
                    let name = name.clone();
                    apply_named(&name, cx);
                    name
                }
                None => {
                    set_theme(ThemeVariant::default(), cx);
                    last_builtin
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Themed;

    /// A complete, valid theme document, for tests that need one on disk.
    ///
    /// Built by string concatenation rather than with `serde_json`, which is not a
    /// dependency of this crate and would be one added for four lines of test fixture.
    fn document(name: &str, background: &str) -> String {
        let mut colors = String::new();
        for key in elle_theme::REQUIRED_COLORS {
            let value = match key {
                "background" => background,
                "text" => "#eeeeee",
                _ => "#808080",
            };
            colors.push_str(&format!("\"{key}\": \"{value}\","));
        }
        for slot in 0..16 {
            colors.push_str(&format!("\"ansi.{slot}\": \"#404040\","));
        }
        colors.pop(); // the trailing comma, which JSON does not allow

        format!(
            "{{\"version\": 1, \"name\": \"{name}\", \"appearance\": \"dark\", \
             \"colors\": {{{colors}}}}}"
        )
    }

    #[gpui::test]
    async fn a_theme_on_disk_can_be_selected_and_actually_changes_the_colours(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("midnight.json"), document("midnight", "#010203")).unwrap();

        let (themes, errors) = elle_theme::load_dir(dir.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(themes.len(), 1);

        cx.update(|cx| {
            cx.set_global(DiskThemes { themes });
            set_theme(ThemeVariant::default(), cx);
            apply_named("midnight", cx);

            assert_eq!(cx.theme().background, gpui::rgb(0x010203).into());
            assert_eq!(cx.theme_choice(), ThemeChoice::Disk("midnight".to_string()));
        });
    }

    /// #58's hard constraint, as a test.
    #[gpui::test]
    async fn a_directory_full_of_broken_themes_still_launches_on_dark(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.json"), "{ not json at all").unwrap();
        std::fs::write(dir.path().join("empty.json"), "{}").unwrap();
        std::fs::write(dir.path().join("wrong.json"), "[1, 2, 3]").unwrap();

        let (themes, errors) = elle_theme::load_dir(dir.path());
        assert!(themes.is_empty(), "none of those are themes");
        assert_eq!(errors.len(), 3, "and each is reported separately");
        for error in &errors {
            let message = error.to_string();
            assert!(message.contains(".json"), "every error names its file: {message}");
        }

        cx.update(|cx| {
            cx.set_global(DiskThemes { themes });
            apply_named("dark", cx);
            assert_eq!(cx.theme().background, crate::theme::Theme::dark().background);
        });
    }

    /// The name a user typed that no longer exists — a deleted theme file, or a typo.
    #[gpui::test]
    async fn a_theme_name_that_matches_nothing_falls_back_without_a_panic(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.set_global(DiskThemes::default());
            apply_named("a-theme-nobody-has", cx);

            assert_eq!(cx.theme_variant(), ThemeVariant::default());
            assert_eq!(cx.theme().background, crate::theme::Theme::dark().background);
        });
    }

    /// The registry never being installed — a render test, or a launch where `load` failed
    /// before it could `set_global`. The built-in cycle has to keep working.
    #[gpui::test]
    async fn theme_switching_works_with_no_registry_at_all(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            set_theme(ThemeVariant::default(), cx);

            // Once round the built-in cycle, with nothing on disk and no global installed.
            for _ in 0..5 {
                cycle(cx);
            }
            assert_eq!(cx.theme_variant(), ThemeVariant::default(), "back where it started");
        });
    }

    /// Disk themes join the cycle after the built-ins, and it comes back round.
    #[gpui::test]
    async fn the_cycle_reaches_disk_themes_and_returns(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("aurora.json"), document("aurora", "#010203")).unwrap();

        let (themes, _) = elle_theme::load_dir(dir.path());

        cx.update(|cx| {
            cx.set_global(DiskThemes { themes });
            set_theme(ThemeVariant::default(), cx);

            // Five built-ins, then the one disk theme, then back to Dark.
            let mut seen = Vec::new();
            for _ in 0..6 {
                seen.push(cycle(cx));
            }

            assert!(seen.contains(&"aurora".to_string()), "the disk theme is reachable: {seen:?}");
            assert_eq!(cx.theme_choice(), ThemeChoice::BuiltIn(ThemeVariant::Dark), "{seen:?}");
        });
    }

    /// A file called `dark.json` must not be able to replace the built-in Dark, which is the
    /// theme the "always available" guarantee is about.
    #[gpui::test]
    async fn a_disk_theme_cannot_shadow_a_built_in_name(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dark.json"), document("dark", "#ff00ff")).unwrap();

        let (themes, _) = elle_theme::load_dir(dir.path());

        cx.update(|cx| {
            cx.set_global(DiskThemes { themes });
            apply_named("dark", cx);

            assert_eq!(
                cx.theme().background,
                crate::theme::Theme::dark().background,
                "the compiled-in Dark must win its own name"
            );
            assert_eq!(cx.theme_choice(), ThemeChoice::BuiltIn(ThemeVariant::Dark));
        });
    }
}
