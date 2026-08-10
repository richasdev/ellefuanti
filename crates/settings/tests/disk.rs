//! The failure modes that need a real filesystem.
//!
//! The unit tests in `src/file.rs` cover parsing, which needs no disk. These cover the
//! things that only go wrong when there is one: a file that cannot be read, a directory
//! that cannot be written, and the write-then-read cycle that a downgrade actually
//! performs.

use std::fs;
use std::path::Path;

use elle_settings::{SETTINGS_VERSION, Settings};

#[test]
fn a_missing_file_leaves_no_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");

    let load = Settings::load(&path);

    assert!(load.error.is_none());
    assert_eq!(load.settings.theme(), "dark");
    assert!(!path.exists(), "reading must not create the file; a first run writes nothing");
}

#[test]
fn a_malformed_file_names_the_file_and_the_problem_and_is_not_touched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let original = "{ this is not JSON at all";
    fs::write(&path, original).unwrap();

    let load = Settings::load(&path);

    let error = load.error.expect("a malformed file must be reported");
    let message = error.to_string();
    assert!(message.contains("settings.json"), "must name the file: {message}");
    assert!(message.contains("line 1"), "must name the problem: {message}");
    assert_eq!(load.settings.theme(), "dark", "and still hand back usable settings");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        original,
        "the user's file is theirs; loading must never repair it in place"
    );
}

#[test]
fn a_file_that_cannot_be_read_falls_back_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    // A directory where a file should be: an EISDIR that needs no permission games and
    // behaves the same under a test runner running as root, which chmod does not.
    let path = dir.path().join("settings.json");
    fs::create_dir(&path).unwrap();

    let load = Settings::load(&path);

    assert!(load.error.is_some(), "unreadable is worth reporting");
    assert_eq!(load.settings.theme(), "dark", "but never a reason not to launch");
}

#[test]
fn saving_creates_the_directory_a_first_run_does_not_have() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Application Support/ellefuanti/settings.json");

    Settings::default().save(&path).unwrap();

    assert!(path.exists());
    assert_eq!(Settings::load(&path).settings.theme(), "dark");
}

#[test]
fn an_unwritable_directory_reports_rather_than_panics() {
    let dir = tempfile::tempdir().unwrap();
    // A *file* standing where the settings directory should be. `create_dir_all` fails
    // with NotADirectory, which is a write failure like any other, and unlike chmod it
    // cannot be shrugged off by a privileged test runner.
    let blocked = dir.path().join("ellefuanti");
    fs::write(&blocked, "not a directory").unwrap();

    let error = Settings::default()
        .save(&blocked.join("settings.json"))
        .expect_err("writing into a file should fail");

    assert!(error.to_string().contains("ellefuanti"), "must name the path: {error}");
}

#[test]
fn a_failed_write_leaves_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    // Rename fails because the destination is a non-empty directory.
    let path = dir.path().join("settings.json");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("occupant"), "x").unwrap();

    assert!(Settings::default().save(&path).is_err());

    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "a failed save must clean up after itself: {leftovers:?}");
}

#[test]
fn the_write_is_atomic_in_the_sense_that_matters() {
    // Not a crash simulation — the property under test is that the destination path is
    // never the thing being written to, so a process dying mid-write cannot truncate it.
    // What is observable from here: the temp file lives in the same directory as the
    // destination (so `rename` is a same-filesystem operation and therefore atomic), and
    // an existing file is replaced whole.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, r#"{"theme": "light"}"#).unwrap();

    let mut settings = Settings::load(&path).settings;
    settings.set_theme("github-dark");
    settings.save(&path).unwrap();

    assert_eq!(Settings::load(&path).settings.theme(), "github-dark");
    let entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries, vec!["settings.json"], "the temp file must not survive a good write");
}

/// The downgrade this issue exists to prevent: a newer build wrote keys an older build has
/// never heard of, the older build saves, and the keys are still there.
#[test]
fn unknown_keys_survive_a_full_read_modify_write_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(
        &path,
        format!(
            r#"{{
  "version": {},
  "theme": "light",
  "editor.fontFamily": "Berkeley Mono",
  "terminal.scrollback": 50000,
  "laravel": {{ "artisan": "php artisan", "watch": ["app", "routes"] }}
}}"#,
            SETTINGS_VERSION + 1
        ),
    )
    .unwrap();

    let load = Settings::load(&path);
    assert_eq!(load.unknown_version, Some(SETTINGS_VERSION + 1), "a future version is noticed");
    assert!(load.error.is_none(), "but is not an error — the file is still read");

    let mut settings = load.settings;
    settings.set_theme("github-dark");
    settings.save(&path).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    let round_tripped: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(round_tripped["theme"], "github-dark", "the key we own changed");
    assert_eq!(round_tripped["editor.fontFamily"], "Berkeley Mono");
    assert_eq!(round_tripped["terminal.scrollback"], 50000);
    assert_eq!(round_tripped["laravel"]["artisan"], "php artisan");
    assert_eq!(round_tripped["laravel"]["watch"][1], "routes", "nested arrays too");
    assert_eq!(
        round_tripped["version"], SETTINGS_VERSION,
        "the version is stamped down to what this build actually wrote"
    );
}

#[test]
fn the_path_helper_sits_beside_the_index_directory() {
    // #72 put the index at `~/Library/Application Support/ellefuanti/index/`. If these two
    // ever disagree the app writes to two roots and cleaning one loses the other.
    let Some(support) = elle_settings::support_dir() else {
        // No HOME. Nothing to check, and nothing persists — see `support_dir`.
        return;
    };
    let settings = elle_settings::settings_path().unwrap();

    assert_eq!(settings.parent(), Some(support.as_path()));
    assert!(support.ends_with(Path::new("Library/Application Support/ellefuanti")), "{support:?}");
}
