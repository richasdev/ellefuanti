//! `status()` against the real CLI, in both states.
//!
//! # Why this drives the real binary
//!
//! The rule it encodes is a fact about `codex login status`, not about our code, and the
//! first version of it was wrong in a way no unit test could catch: it matched "not logged
//! in" in **stdout**, which is always empty — the CLI writes to stderr and signals with the
//! exit code (0 signed in, 1 not, measured against codex-cli 0.146.0). That version returned
//! the right answer for a logged-in machine and would have called a logged-out user ready,
//! sending them into a turn that hangs instead of showing the login button.
//!
//! A mock could only assert the assumption back at itself. Skipped when Codex is absent.

/// The logged-out half, forced with `CODEX_HOME` at a directory holding no `auth.json` —
/// the CLI's own override, so this needs nobody to actually sign out.
#[test]
fn a_logged_out_cli_is_reported_as_logged_out() {
    let Some(binary) = which_codex() else {
        eprintln!("no codex on this machine; skipping");
        return;
    };

    let empty = std::env::temp_dir().join(format!("elle-codex-empty-{}", std::process::id()));
    std::fs::create_dir_all(&empty).expect("temp CODEX_HOME");

    let output = std::process::Command::new(&binary)
        .args(["login", "status"])
        .env("CODEX_HOME", &empty)
        .output()
        .expect("the CLI must run");

    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    let _ = std::fs::remove_dir_all(&empty);

    assert!(
        stderr.contains("not logged in"),
        "a CODEX_HOME with no auth.json must report logged out, got stderr {stderr:?} and \
         stdout {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !output.status.success(),
        "and it must signal that in the exit code, which is what `status()` reads"
    );
}

/// The shape the parsing relies on: the sentence is on stderr, never stdout. If a future
/// CLI moves it, this fails here rather than silently mis-reporting login state in the app.
#[test]
fn the_status_sentence_is_on_stderr_not_stdout() {
    let Some(binary) = which_codex() else { return };

    let output = std::process::Command::new(&binary)
        .args(["login", "status"])
        .output()
        .expect("the CLI must run");

    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "stdout is expected to be empty; if the CLI started using it, `status()` should \
         prefer it: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).to_lowercase().contains("logged in"),
        "stderr must carry the answer: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn which_codex() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    [".local/bin/codex", ".npm-global/bin/codex"]
        .iter()
        .map(|suffix| std::path::Path::new(&home).join(suffix))
        .find(|path| path.exists())
        .or_else(|| {
            ["/usr/local/bin/codex", "/opt/homebrew/bin/codex"]
                .iter()
                .map(std::path::PathBuf::from)
                .find(|path| path.exists())
        })
}
