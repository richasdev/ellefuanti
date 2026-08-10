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
