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
    let client = match elle_lsp::Client::start(&config) {
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

/// The full completion round trip, exactly as the app performs it (#125, final piece).
///
/// This is the automated version of the test the owner ran by hand five times: didOpen,
/// then the pre-request resync `request_lsp_completions` does, then a completion request —
/// all through `elle_lsp::Client`, the same methods in the same order.
///
/// # What is and is not asserted
///
/// The bug this hunts took a wire capture to find: without the resync, the server's copy
/// of the file is the one from `didOpen`, so a completion request about text the user just
/// typed asks about a position where that text does not exist, and the server correctly
/// answers nothing. Opening with the final text directly would pass without the resync
/// working, which is the exact hole the popup fell through — hence open-then-resync below.
///
/// Only the *live* position is asserted. During diagnosis, `$this->` written after a
/// `return` answered **0 items in one file shape and 3 in another** (a first draft of this
/// test asserted 0 and was refuted by its own first run) — the answer depends on how the
/// server's parser recovers around the error, which is its business. Asserting either
/// number would bake a heuristic we do not own into our suite; the empty-answer UX is
/// covered by `close_if_empty_trigger`'s own tests, which do not need a real server to
/// decide what to do with an empty list.
#[test]
fn completion_round_trips_against_a_real_server() {
    let dir = project();
    // `$this->` on the line BEFORE the return: live code, the server must answer.
    // A second `$this->` after the return: dead code, the server answers empty.
    let text = "<?php\n\nclass User\n{\n    public string $name;\n    public string $email;\n\n    \
                public function greet(): string\n    {\n        $this->\n        return $this->name;\n        \
                $this->\n    }\n}\n";
    std::fs::write(dir.path().join("app/Models/User.php"), text).unwrap();

    let Some(config) = ellefuanti_config_for(dir.path()) else {
        eprintln!("SKIPPED: no language server installed on this machine");
        return;
    };
    let mut client = match elle_lsp::Client::start(&config) {
        Ok(client) => client,
        Err(err) => panic!("server must start: {err:#}"),
    };

    let uri: elle_lsp::lsp_types::Uri =
        format!("file://{}/app/Models/User.php", dir.path().canonicalize().unwrap().display())
            .parse()
            .unwrap();

    // The app's order: open with the *original* text, then resync to the edited one — the
    // flow `request_lsp_completions` performs. Opening with the final text directly would
    // pass without the resync working, which is the exact hole the popup fell through.
    let original = text.replace("        $this->\n        return", "        \n        return");
    client.did_open(uri.clone(), "php", &original).unwrap();
    client.did_change_full(&uri, text).unwrap();

    let live = text.find("$this->").unwrap() + "$this->".len();
    let items = complete_at(&mut client, &uri, live);
    assert!(
        items.iter().any(|label| label == "name"),
        "live `$this->` must offer the class's own members, got: {items:?}"
    );
    assert!(items.iter().any(|label| label == "greet"), "methods too: {items:?}");
}

/// Asks and polls, the way `poll_query` does, with a test-sized timeout.
fn complete_at(
    client: &mut elle_lsp::Client,
    uri: &elle_lsp::lsp_types::Uri,
    offset: usize,
) -> Vec<String> {
    use elle_lsp::lsp_types::CompletionResponse;

    let id = client.request_completion(uri, offset).expect("request goes out");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match client.poll_response::<CompletionResponse>(&id) {
            Ok(Some(Some(response))) => {
                let items = match response {
                    CompletionResponse::Array(items) => items,
                    CompletionResponse::List(list) => list.items,
                };
                return items.into_iter().map(|item| item.label).collect();
            }
            Ok(Some(None)) => return Vec::new(),
            Ok(None) => {
                assert!(std::time::Instant::now() < deadline, "server never answered");
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(err) => panic!("completion failed: {err:#}"),
        }
    }
}

/// Go-to-definition answers with the declaration site, against a real server.
///
/// ⌘-click and F12 both end here: the editor emits `GoToDefinition`, the workspace asks
/// Laravel first and the server second, and the server's answer is a `Location` whose
/// range this test pins to the *declaration* line. The owner asked for the feature after
/// it was already wired — reasonable, since until this session the popup was wired too
/// and never worked; what was missing every time was the document reaching the server,
/// which the resync now guarantees and this exercises through the same client calls.
#[test]
fn definition_round_trips_against_a_real_server() {
    use elle_lsp::lsp_types::GotoDefinitionResponse;

    let dir = project();
    // `$this->name` on line 9 (0-based); `public string $name;` declared on line 4.
    let text = "<?php\n\nclass User\n{\n    public string $name;\n    public string $email;\n\n    \
                public function greet(): string\n    {\n        return $this->name;\n    }\n}\n";
    std::fs::write(dir.path().join("app/Models/User.php"), text).unwrap();

    let Some(config) = ellefuanti_config_for(dir.path()) else {
        eprintln!("SKIPPED: no language server installed on this machine");
        return;
    };
    let mut client = elle_lsp::Client::start(&config).expect("server starts");

    let uri: elle_lsp::lsp_types::Uri =
        format!("file://{}/app/Models/User.php", dir.path().canonicalize().unwrap().display())
            .parse()
            .unwrap();
    client.did_open(uri.clone(), "php", text).unwrap();

    // The byte offset of `name` inside `$this->name` — the place a ⌘-click lands.
    let usage = text.rfind("name;").unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let answer = loop {
        match client.definition(&uri, usage) {
            Ok(Some(answer)) => break answer,
            Ok(None) => {
                // The server may still be indexing; a definition of nothing is its honest
                // interim answer. Ask again until the deadline rather than failing on a
                // cold server — the retry is the test's, not the app's.
                assert!(std::time::Instant::now() < deadline, "server never resolved it");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(err) => panic!("definition failed: {err:#}"),
        }
    };

    let location = match answer {
        GotoDefinitionResponse::Scalar(location) => location,
        GotoDefinitionResponse::Array(mut locations) => locations.remove(0),
        GotoDefinitionResponse::Link(mut links) => {
            let link = links.remove(0);
            elle_lsp::lsp_types::Location { uri: link.target_uri, range: link.target_range }
        }
    };
    assert_eq!(location.range.start.line, 4, "the declaration is `public string $name;`");
}
