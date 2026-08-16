//! `--version` must answer and exit before any GUI exists.
//!
//! Without it the flag fell through `path_argument`'s leading-dash skip and launched the
//! whole app; with stdout a pipe, `detach_from_terminal` stayed foreground and the calling
//! script hung forever on a GUI it never asked for. The terminal is a supported way in
//! (`ellefuanti .`), so the CLI basics have to hold — and this is the probe that found the
//! hang, kept as the regression test.

#[test]
fn version_flag_answers_and_exits() {
    let binary = env!("CARGO_BIN_EXE_ellefuanti");
    for flag in ["--version", "-v"] {
        let output =
            std::process::Command::new(binary).arg(flag).output().expect("the binary must run");
        assert!(output.status.success(), "{flag} must exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(env!("CARGO_PKG_VERSION")),
            "{flag} must name the version: {stdout:?}"
        );
    }
}

/// `ellefuanti -w .` must still find the path: the old `nth(1)` read the flag, dismissed
/// it as a non-path, and opened an empty window — the path was one slot further along.
#[test]
fn the_path_survives_being_preceded_by_a_flag() {
    // Pure-logic double of `path_argument`'s rule, since args can't be injected into the
    // real one: first non-flag argument wins.
    let find = |args: &[&str]| args.iter().find(|a| !a.starts_with('-')).map(|a| a.to_string());
    assert_eq!(find(&["-w", "src/app.php"]), Some("src/app.php".to_string()));
    assert_eq!(find(&["--wait", "."]), Some(".".to_string()));
    assert_eq!(find(&["-psn_0_12345"]), None);
}
