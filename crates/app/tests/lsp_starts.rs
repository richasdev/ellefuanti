//! Does a language server actually start? (#125)
//!
//! Every existing test asserts on *resolution* — which directories are searched, what
//! `config_for` returns — and none of them start a process. That is exactly the gap that
//! let the shebang bug through: `config_for` was correct, the spawn succeeded, and the
//! server died before writing a byte.
//!
//! This is the one test that runs the real thing. It is skipped, loudly, when no server is
//! installed, because that is the normal state of CI and of a fresh machine (§24).

use std::path::PathBuf;

/// A Laravel-shaped project, so the root markers and the language detection both apply.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("composer.json"), "{}").unwrap();
    std::fs::create_dir_all(dir.path().join("app/Models")).unwrap();
    std::fs::write(
        dir.path().join("app/Models/User.php"),
        "<?php\n\nclass User\n{\n    public string $name;\n}\n",
    )
    .unwrap();
    dir
}

/// The whole point: a configured server must start, handshake, and report capabilities.
///
/// Uses `elle_lsp::Client` directly — the same call `lsp_session::start` makes — so what is
/// under test is the real transport and the real handshake, not a mock of them.
#[test]
fn a_configured_server_starts_and_completes_the_handshake() {
    let dir = project();

    // Built the way the app builds it. If this is None the machine has no server and there
    // is nothing to test — say so rather than passing quietly.
    let Some(config) = ellefuanti_config_for(dir.path()) else {
        eprintln!("SKIPPED: no language server installed on this machine");
        return;
    };

    eprintln!("spawning: {}", config.command);
    let mut client = match elle_lsp::Client::start(&config) {
        Ok(client) => client,
        Err(err) => panic!(
            "a resolved server must start. This is the failure a resolution-only test \
             cannot see — the binary was found and the spawn still did not produce a \
             working server: {err:#}"
        ),
    };

    // Capabilities are the proof the handshake completed: they come from the server's
    // `initialize` reply and cannot be fabricated by a process that died.
    let capabilities = client.capabilities();
    assert!(
        capabilities.completion,
        "the server must advertise completion — without it #125's popup can never open"
    );

    eprintln!("triggers: {:?}", capabilities.completion_triggers);
}

/// `config_for` is private to the binary, so this mirrors it. Kept deliberately small: the
/// unit tests in `lsp_session` cover the search itself, and what this file is for is the
/// spawn that happens after.
fn ellefuanti_config_for(root: &std::path::Path) -> Option<elle_lsp::ServerConfig> {
    let raw = std::env::var("ELLE_LSP_COMMAND").unwrap_or_else(|_| "intelephense --stdio".into());
    let mut parts = raw.split_whitespace().map(str::to_string);
    let command = parts.next()?;
    let args: Vec<String> = parts.collect();

    let dirs = search_dirs();
    let binary = dirs.iter().map(|dir| dir.join(&command)).find(|candidate| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(candidate)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })?;

    Some(
        elle_lsp::ServerConfig::new(command, binary.to_string_lossy().into_owned(), root)
            .with_args(args)
            .with_env("PATH", std::env::join_paths(&dirs).ok()?.to_string_lossy().into_owned())
            .with_language_ids(["php"]),
    )
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for prefix in [
        ".nvm/versions/node/*/bin",
        "Library/Application Support/Herd/config/nvm/versions/node/*/bin",
        ".local/bin",
        ".npm-global/bin",
    ] {
        let Some(home) = home.as_ref() else { break };
        match prefix.split_once('*') {
            Some((before, after)) => {
                let after = after.trim_start_matches('/');
                if let Ok(entries) = std::fs::read_dir(home.join(before.trim_end_matches('/'))) {
                    dirs.extend(entries.flatten().map(|entry| entry.path().join(after)));
                }
            }
            None => dirs.push(home.join(prefix)),
        }
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].iter().map(PathBuf::from));
    dirs
}
