//! Where `elle-settings` meets gpui: the startup read, and the theme it decides.
//!
//! `elle-settings` is plain Rust and knows nothing about themes beyond the fact that one
//! has a name (ADR-0004). This is the translation layer ADR-0004 says the presentation
//! crate has to hold — the twenty lines that turn a string into a `ThemeVariant` and a
//! parse error into a log line.
//!
//! Only the theme is wired up. That is the whole point of #60: a settings *layer* with one
//! real consumer proving it works, rather than a schema of keys nobody reads yet. Fonts
//! (#49), scrollback (#70) and window geometry each add a pair of functions here when
//! their issue lands.

use std::path::PathBuf;

use elle_settings::{Load, Settings};
use gpui::{App, AppContext as _, Global};

use crate::theme::{ThemeVariant, set_theme};

/// The settings document, live, plus where it came from.
///
/// A global rather than a field on `WorkspaceView`, because the thing that writes it is
/// [`set_theme`], which takes an `App` and no view. Holding the whole parsed document — not
/// just the keys this build reads — is what makes the unknown-key guarantee survive a
/// *write*: a save serialises this value, so a key nobody here has heard of is still in it.
///
/// Absent means "do not persist", which is the state render tests and `--help`-shaped
/// invocations are in. Nothing has to opt out of writing to the user's real settings file;
/// they have to opt in by calling [`load_and_apply`].
struct LiveSettings {
    settings: Settings,
    path: PathBuf,
}

impl Global for LiveSettings {}

/// Reads the settings file and applies everything in it.
///
/// Installs [`LiveSettings`] afterwards, so the `set_theme` this makes does not turn round
/// and write the file back — reading settings must not be a write. From here on, every
/// theme change persists.
///
/// Blocking, and called before the window exists. ADR-0007 says blocking work belongs on
/// `cx.background_spawn`, and this is the exception it is worth naming: a `read_to_string`
/// of a file measured in hundreds of bytes costs less than the frame it would take to hand
/// the result back, and the alternative is opening a window in the wrong theme and
/// repainting it. Anything here that grows a directory walk moves to the background.
pub fn load_and_apply(cx: &mut App) {
    let Some(path) = elle_settings::settings_path() else {
        tracing::warn!("HOME is unset; running on default settings and persisting nothing");
        set_theme(ThemeVariant::default(), cx);
        return;
    };

    let Load { settings, error, unknown_version } = Settings::load(&path);

    // Named file, named problem, still launches — the trust-boundary rule. `error` is at
    // error level because the user's file is being ignored and they will keep editing it
    // otherwise; the version note is a warning because nothing was ignored.
    if let Some(version) = unknown_version {
        tracing::warn!(
            file_version = version,
            known_version = elle_settings::SETTINGS_VERSION,
            "settings.json was written by a newer build; unrecognised keys are read as \
             absent but will be preserved when this build saves"
        );
    }

    set_theme(theme_from(&settings), cx);

    // A file we could not read does **not** become writable. `settings` here is defaults,
    // and installing it would mean the next theme toggle overwrote a file full of the
    // user's configuration with a two-key document — losing exactly what this issue exists
    // to protect, and doing it as a side effect of a keystroke. So: read-only for this
    // launch, say so, and let them fix the file.
    if let Some(error) = error {
        tracing::error!(
            "{error}; running on defaults and NOT saving over it — fix the file, or delete \
             it to start fresh"
        );
        return;
    }

    cx.set_global(LiveSettings { settings, path });
}

/// Records a theme change, if there is a settings file to record it in.
///
/// Called from [`set_theme`] rather than from the toggle handler, so any future way of
/// changing the theme persists without being told to. A no-op when [`LiveSettings`] is
/// absent — which is every render test, and the startup read itself.
///
/// The write is a `background_spawn` (ADR-0007): `set_theme` runs from a key handler, and
/// two filesystem syscalls in a key handler is exactly the frame-time bug that ADR bans.
/// The in-memory document is updated synchronously, so a second toggle before the write
/// lands still saves the right value — and the loser of that race is a complete document
/// either way, which is the property [`Settings::save`] is arranged around.
///
/// A failed write logs and nothing else. Losing a theme preference is not worth an error
/// dialog, and there is nothing the user can do about a full disk from inside the editor.
pub fn persist_theme(variant: ThemeVariant, cx: &mut App) {
    let Some(live) = cx.try_global::<LiveSettings>() else { return };
    if live.settings.theme() == variant.name() {
        return;
    }

    let live = cx.global_mut::<LiveSettings>();
    live.settings.set_theme(variant.name());
    let (settings, path) = (live.settings.clone(), live.path.clone());

    cx.background_spawn(async move {
        if let Err(err) = settings.save(&path) {
            tracing::error!("could not save settings: {err:#}");
        }
    })
    .detach();
}

/// The variant the settings named, or the default.
///
/// An unrecognised name logs and falls back rather than failing, and — importantly — does
/// not rewrite the file. `"solarized"` in someone's settings is either a typo they can see
/// and fix or a theme #58 will load from disk; either way, silently replacing it with
/// `"dark"` on the next save destroys the evidence.
fn theme_from(settings: &Settings) -> ThemeVariant {
    let name = settings.theme();
    ThemeVariant::from_name(name).unwrap_or_else(|| {
        let fallback = ThemeVariant::default();
        tracing::warn!(theme = name, using = fallback.name(), "unknown theme name in settings");
        fallback
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two halves of the round trip that live in different crates.
    ///
    /// `elle-settings` hardcodes `"dark"` as its default because it cannot see this enum.
    /// If `ThemeVariant::default()` ever stops being Dark, this fails — which is the only
    /// warning anyone gets that the two defaults have drifted apart.
    #[test]
    fn the_default_theme_name_agrees_across_the_crate_boundary() {
        assert_eq!(Settings::default().theme(), ThemeVariant::default().name());
    }

    #[test]
    fn every_theme_name_survives_a_write_and_a_read() {
        for variant in [
            ThemeVariant::Dark,
            ThemeVariant::Light,
            ThemeVariant::OneDarkPro,
            ThemeVariant::GitHubDark,
            ThemeVariant::GitHubLight,
        ] {
            let mut settings = Settings::default();
            settings.set_theme(variant.name());

            let written = settings.to_json();
            let read = Settings::parse(std::path::Path::new("settings.json"), &written).unwrap();

            assert_eq!(theme_from(&read), variant, "{} did not survive", variant.label());
        }
    }

    #[test]
    fn an_unknown_theme_name_falls_back_without_rewriting_it() {
        let mut settings = Settings::default();
        settings.set_theme("solarized-lite");

        assert_eq!(theme_from(&settings), ThemeVariant::default());
        assert_eq!(settings.theme(), "solarized-lite", "the name the user typed is still there");
    }

    /// The whole feature, end to end, through the real `set_theme` — not a hand-rolled
    /// call to `Settings::save`.
    ///
    /// A test that saved the document directly would pass while the wiring in `set_theme`
    /// was missing, which is precisely the bug #60 exists to close: theme switching has
    /// worked all along and simply never reached a file.
    #[gpui::test]
    async fn switching_the_theme_writes_it_and_reads_it_back(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        // The startup sequence, against a temp file instead of the user's real one.
        cx.update(|cx| {
            let settings = Settings::load(&path).settings;
            set_theme(theme_from(&settings), cx);
            cx.set_global(LiveSettings { settings, path: path.clone() });
        });
        assert!(!path.exists(), "reading settings must not write them");

        cx.update(|cx| set_theme(ThemeVariant::GitHubLight, cx));
        // The save is a `background_spawn`; without this the assertion races it.
        cx.background_executor.run_until_parked();

        assert_eq!(
            Settings::load(&path).settings.theme(),
            "github-light",
            "the choice must survive the process that made it"
        );
    }

    /// The nastiest way this feature could have destroyed configuration, caught in review
    /// rather than by a user.
    ///
    /// A malformed file loads as *defaults*. Installing those as the live document would
    /// mean the next theme toggle saved a two-key file over everything the user had
    /// written — losing their configuration as a side effect of a keystroke, from a state
    /// they were already in because of a typo. So a file that could not be read is
    /// read-only for the launch.
    #[gpui::test]
    async fn a_malformed_file_is_never_saved_over(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let broken = r#"{ "theme": "light", "editor.fontFamily": "Berkeley Mono""#;
        std::fs::write(&path, broken).unwrap();

        // `load_and_apply` reads a path this test cannot choose, so this repeats its tail
        // against a temp file. The line under test is the early return on `error`.
        cx.update(|cx| {
            let Load { settings, error, .. } = Settings::load(&path);
            set_theme(theme_from(&settings), cx);
            assert!(error.is_some(), "the fixture must actually be malformed");
            if error.is_none() {
                cx.set_global(LiveSettings { settings, path: path.clone() });
            }
        });

        cx.update(|cx| set_theme(ThemeVariant::GitHubDark, cx));
        cx.background_executor.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            broken,
            "a file we could not parse must survive the session unchanged"
        );
    }

    /// The other half, and the reason the global exists: a render test — or anything else
    /// that never called `load_and_apply` — switches themes freely and writes nothing.
    ///
    /// Without this the 20-odd tests in `render_tests.rs` would each rewrite whatever
    /// `~/Library/Application Support/ellefuanti/settings.json` the machine running them
    /// happens to have, which is a test suite that edits the developer's editor.
    #[gpui::test]
    async fn a_theme_change_with_no_live_settings_writes_nothing(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        // Wire it up, switch once so the file exists, then take the global away again.
        // Removing it is what a render test's App looks like, and going through the wired
        // state first is what proves this test would notice if the wiring were always-on.
        cx.update(|cx| {
            let settings = Settings::load(&path).settings;
            set_theme(theme_from(&settings), cx);
            cx.set_global(LiveSettings { settings, path: path.clone() });
            set_theme(ThemeVariant::GitHubLight, cx);
        });
        cx.background_executor.run_until_parked();
        let written = std::fs::read_to_string(&path).expect("the wired switch wrote the file");

        cx.update(|cx| {
            cx.remove_global::<LiveSettings>();
            set_theme(ThemeVariant::Dark, cx);
            set_theme(ThemeVariant::GitHubDark, cx);
        });
        cx.background_executor.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            written,
            "no LiveSettings global means no persistence"
        );
    }
}
