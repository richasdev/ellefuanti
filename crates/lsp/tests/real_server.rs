//! Drives a REAL language server end to end. Not part of the suite.
//!
//! Run with:
//!   ELLE_LSP_REAL=/path/to/intelephense cargo test --test real_server -- --nocapture

use std::time::{Duration, Instant};

use elle_lsp::{Client, ServerConfig, ServerEvent, path_to_uri};

fn real_command() -> Option<String> {
    std::env::var("ELLE_LSP_REAL").ok()
}

fn root() -> String {
    std::env::var("ELLE_LSP_ROOT").unwrap_or_else(|_| "/tmp/laravel-teste".into())
}

fn config(command: &str) -> ServerConfig {
    ServerConfig::new("real", command, root()).with_args(["--stdio"]).with_language_ids(["php"])
}

/// Pids of every running server, so the kill test can pick out the one it started.
fn running_servers() -> Vec<u32> {
    let Ok(output) = std::process::Command::new("pgrep").args(["-f", "intelephense"]).output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect()
}

#[test]
fn a_real_server_reports_diagnostics() {
    let Some(command) = real_command() else {
        eprintln!("ELLE_LSP_REAL not set; skipping");
        return;
    };

    let started = Instant::now();
    let mut client = Client::start(&config(&command)).expect("the server must start");
    eprintln!("handshake took {:?}", started.elapsed());
    eprintln!("encoding = {:?}", client.encoding());
    eprintln!("sync     = {:?}", client.capabilities().sync);

    let path = std::path::PathBuf::from(root()).join("broken_for_test.php");
    let source = "<?php\n\nfunction f(): int {\n    return $undefinedVariable;\n}\n";
    std::fs::write(&path, source).unwrap();

    let uri = path_to_uri(&path).unwrap();
    client.did_open(uri.clone(), "php", source).unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut found = false;
    while Instant::now() < deadline && !found {
        for event in client.wait_for_events(Duration::from_millis(500)) {
            if let ServerEvent::Diagnostics { uri: u, diagnostics, .. } = event {
                eprintln!("DIAGNOSTICS {} -> {}", u.as_str(), diagnostics.len());
                for d in &diagnostics {
                    eprintln!("   {:?} {:?} {}", d.severity, d.range, d.message);
                }
                if u == uri && !diagnostics.is_empty() {
                    found = true;
                }
            }
        }
        if !client.is_alive() {
            panic!("the server died: {:?}", client.failure());
        }
    }

    let _ = std::fs::remove_file(&path);
    assert!(found, "a real server must have reported the undefined variable");

    client.stop().unwrap();
    eprintln!("stopped cleanly");
}

/// The scenario #43 is really about: a live server dying while a client is talking to it.
///
/// This is what could not be tested before — the mock dies on command, but a real server
/// killed by a signal closes its pipes from the outside, mid-conversation, with a reader
/// thread parked in `read_message` and a caller parked in a wait.
#[test]
fn a_real_server_killed_mid_session_is_reported_not_panicked() {
    let Some(command) = real_command() else {
        eprintln!("ELLE_LSP_REAL not set; skipping");
        return;
    };

    // Note which servers already exist, so the one this test starts can be told apart from
    // the ones the other tests in this file are using. Killing by name alone took those
    // down too, and the resulting failure read as a bug in the client.
    let before = running_servers();

    let mut client = Client::start(&config(&command)).expect("the server must start");

    let path = std::path::PathBuf::from(root()).join("killed_for_test.php");
    let source = "<?php\n$x = 1;\n";
    std::fs::write(&path, source).unwrap();
    let uri = path_to_uri(&path).unwrap();
    client.did_open(uri.clone(), "php", source).unwrap();

    // Let it settle so the kill lands mid-session rather than mid-handshake.
    let _ = client.wait_for_events(Duration::from_millis(500));
    assert!(client.is_alive(), "should be alive before the kill");

    // SIGKILL, not SIGTERM: a polite shutdown is the path that already works, and the one
    // worth testing is the server vanishing without saying anything.
    let mine: Vec<_> = running_servers().into_iter().filter(|pid| !before.contains(pid)).collect();
    assert!(!mine.is_empty(), "the test could not find the server it started");

    for pid in mine {
        std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().ok();
    }

    // The editor must notice, report, and stay usable.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && client.is_alive() {
        let _ = client.wait_for_events(Duration::from_millis(200));
    }

    assert!(!client.is_alive(), "the client must notice the server is gone");
    eprintln!("failure reported as: {:?}", client.failure());

    // And every subsequent call must be an error, never a panic and never a hang.
    let err = client.hover(&uri, 0);
    eprintln!("hover after death: {err:?}");

    let _ = client.did_change_full(&uri, "<?php\n$y = 2;\n");
    let events = client.wait_for_events(Duration::from_millis(200));
    assert!(events.is_empty());

    let _ = std::fs::remove_file(&path);
    // Stopping an already-dead server is a success, not a fault.
    client.stop().expect("stopping a dead server must not error");
    eprintln!("survived a mid-session kill");
}

/// The other half of #43, against a real server: handshake (waits on `pending`) and then
/// listen for diagnostics (waits on `inbox`). Two mutexes, one condvar, before the fix.
#[test]
fn a_real_session_waits_on_both_mutexes_without_panicking() {
    let Some(command) = real_command() else {
        eprintln!("ELLE_LSP_REAL not set; skipping");
        return;
    };

    let mut client = Client::start(&config(&command)).expect("the server must start");

    let path = std::path::PathBuf::from(root()).join("both_waits_test.php");
    let source = "<?php\nclass A { public function b(): void {} }\n";
    std::fs::write(&path, source).unwrap();
    let uri = path_to_uri(&path).unwrap();
    client.did_open(uri.clone(), "php", source).unwrap();

    // Interleave the two wait kinds several times. Before the condvar fix this aborts the
    // process on the first switch.
    for round in 0..5 {
        let _ = client.hover(&uri, 20); // waits on `pending`
        let events = client.wait_for_events(Duration::from_millis(300)); // waits on `inbox`
        eprintln!("round {round}: {} event(s), alive={}", events.len(), client.is_alive());
        assert!(client.is_alive(), "round {round} killed the connection");
    }

    let _ = std::fs::remove_file(&path);
    client.stop().unwrap();
    eprintln!("interleaved waits survived");
}

/// The first real exercise of `request_completion`, which had never been called (#61).
///
/// `completion` and `request_completion` have existed since #45 and nothing invoked either,
/// so nothing had ever confirmed that the params they build are the ones Intelephense
/// expects. A mock answers whatever it is told to; only a real server can reject a malformed
/// position or answer about the wrong offset.
#[test]
fn a_real_server_completes_a_member_access() {
    let Some(command) = real_command() else {
        eprintln!("ELLE_LSP_REAL not set; skipping");
        return;
    };

    let mut client = Client::start(&config(&command)).expect("the server must start");

    let path = std::path::PathBuf::from(root()).join("completion_for_test.php");
    // A class with two methods and a variable typed by a docblock, which is how Intelephense
    // knows what `$user->` can offer without a whole framework indexed.
    let source = "<?php\n\nclass Person {\n    public function getName(): string { return ''; }\n    public function getAge(): int { return 0; }\n}\n\n$person = new Person();\n$person->\n";
    std::fs::write(&path, source).unwrap();

    let uri = path_to_uri(&path).unwrap();
    client.did_open(uri.clone(), "php", source).unwrap();

    // The offset just after `$person->`, which is where a user pressing ⌃space would be.
    let offset = source.rfind("$person->").unwrap() + "$person->".len();

    // Intelephense indexes before it answers usefully, so this retries rather than asking
    // once — the same shape as the diagnostics test above.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut labels: Vec<String> = Vec::new();
    while Instant::now() < deadline && labels.is_empty() {
        let id = client.request_completion(&uri, offset).expect("the request must go out");
        let answer = client
            .await_response::<elle_lsp::lsp_types::CompletionResponse>(&id, Duration::from_secs(10))
            .expect("the server must answer rather than erroring");

        if let Some(response) = answer {
            labels = match response {
                elle_lsp::lsp_types::CompletionResponse::Array(items) => {
                    items.into_iter().map(|i| i.label).collect()
                }
                elle_lsp::lsp_types::CompletionResponse::List(list) => {
                    list.items.into_iter().map(|i| i.label).collect()
                }
            };
        }
        if labels.is_empty() {
            std::thread::sleep(Duration::from_millis(500));
        }
        if !client.is_alive() {
            panic!("the server died: {:?}", client.failure());
        }
    }

    let _ = std::fs::remove_file(&path);
    eprintln!("COMPLETIONS -> {} items", labels.len());
    for label in labels.iter().take(20) {
        eprintln!("   {label}");
    }

    assert!(!labels.is_empty(), "a real server must offer something after `$person->`");
    // The specific claim, not just "non-empty": the position was interpreted as the member
    // access it is. An off-by-one in the offset conversion answers with globals instead, and
    // that list is also non-empty — which is exactly how this test would have passed while
    // being wrong.
    assert!(
        labels.iter().any(|l| l.contains("getName")),
        "the completion must be about `$person->`, got: {labels:?}"
    );

    client.stop().unwrap();
}

/// Cancellation against a real server: the claim #61 makes about typing fast (#45's
/// `request_completion` plus `cancel`).
///
/// What this establishes is that `$/cancelRequest` goes out and the client stops waiting —
/// the two halves ADR-0007 asks for. What it deliberately does **not** claim is that
/// Intelephense abandoned the work internally: a server is free to finish and discard, the
/// protocol does not let a client observe which it did, and asserting otherwise would be a
/// statement about someone else's implementation.
#[test]
fn a_real_server_completion_can_be_cancelled() {
    let Some(command) = real_command() else {
        eprintln!("ELLE_LSP_REAL not set; skipping");
        return;
    };

    let mut client = Client::start(&config(&command)).expect("the server must start");

    let path = std::path::PathBuf::from(root()).join("cancel_for_test.php");
    let source = "<?php\n\nclass Thing {\n    public function alpha(): string { return ''; }\n}\n\n$thing = new Thing();\n$thing->\n";
    std::fs::write(&path, source).unwrap();

    let uri = path_to_uri(&path).unwrap();
    client.did_open(uri.clone(), "php", source).unwrap();
    let offset = source.rfind("$thing->").unwrap() + "$thing->".len();

    // Three requests in a row, as fast typing produces, cancelling each as the next is
    // issued. Only the last is awaited — which is precisely the popup's behaviour.
    let first = client.request_completion(&uri, offset).expect("first request");
    let second = client.request_completion(&uri, offset).expect("second request");
    client.cancel(&first);

    let third = client.request_completion(&uri, offset).expect("third request");
    client.cancel(&second);

    // A cancelled id resolves *immediately* rather than blocking out its timeout, because
    // `Connection::cancel` removes the pending entry first and `wait` reads a missing entry
    // as `RequestOutcome::Cancelled`. That is the half which reclaims the slot an abandoned
    // request would otherwise hold for the life of the process.
    //
    // The timing is the assertion, not the value. `Ok(None)` alone would also be what a
    // server answering "nothing here" looks like — I wrote this asserting `is_err()` first,
    // watched it fail against a real server, and the honest property turned out to be that
    // it comes back at once instead of after the two-second wait.
    let waited = Instant::now();
    let cancelled = client
        .await_response::<elle_lsp::lsp_types::CompletionResponse>(&first, Duration::from_secs(2));
    let elapsed = waited.elapsed();
    eprintln!("a cancelled request resolved in {elapsed:?} -> {:?}", cancelled.is_ok());
    assert!(
        elapsed < Duration::from_millis(500),
        "a cancelled request must resolve at once, not block for its timeout: {elapsed:?}"
    );
    assert!(cancelled.is_ok(), "cancellation is an outcome, not a fault: {cancelled:?}");

    // The surviving request still answers, which is what proves cancelling the other two did
    // not poison the connection.
    let answer = client
        .await_response::<elle_lsp::lsp_types::CompletionResponse>(&third, Duration::from_secs(30));
    eprintln!("after two cancellations the live request answered: {:?}", answer.is_ok());
    assert!(answer.is_ok(), "the surviving request must still be answerable: {answer:?}");

    let _ = std::fs::remove_file(&path);
    assert!(client.is_alive(), "cancelling must not kill the server");
    client.stop().unwrap();
}
