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

/// What the real server *declares* as trigger characters (#61).
///
/// The whole trigger feature reads this list, so it is worth recording what a real server
/// actually puts in it — and the answer is not what a hardcoded implementation would have
/// guessed. Intelephense declares ten **single** characters, not the two-character `->` and
/// `::` sequences the feature is usually described in terms of.
///
/// The assertion is deliberately weak about the exact set. Pinning all ten would make this
/// test fail on an Intelephense upgrade that added one, which is not a regression in this
/// codebase — the property that matters is that the list is non-empty, comes from the
/// server, and is made of the single characters the trigger check compares against.
#[test]
fn a_real_server_declares_its_own_trigger_characters() {
    let Some(command) = real_command() else {
        eprintln!("ELLE_LSP_REAL not set; skipping");
        return;
    };

    let mut client = Client::start(&config(&command)).expect("the server must start");
    let triggers = client.capabilities().completion_triggers.clone();
    eprintln!("declared completion triggers: {triggers:?}");

    assert!(!triggers.is_empty(), "a real PHP server must declare some trigger characters");
    // The shape the app's `is_completion_trigger` compares against: one keystroke produces
    // one `key_char`, so a multi-character trigger could never match it. Recording that the
    // real server declares single characters is what makes whole-string equality the right
    // test rather than a lucky one.
    assert!(
        triggers.iter().all(|t| t.chars().count() == 1),
        "the trigger check compares one keystroke against each entry: {triggers:?}"
    );
    // The two PHP cares about most, as *characters* — `>` completes `->` and `:` completes
    // `::`, which is why no `->` string appears anywhere in the implementation.
    assert!(triggers.iter().any(|t| t == ">"), "member access must be triggerable: {triggers:?}");
    assert!(triggers.iter().any(|t| t == ":"), "static access must be triggerable: {triggers:?}");

    client.stop().unwrap();
}

/// Whether the *server* declines to complete inside strings and comments (#61).
///
/// This is the question that decides who owns context-sensitivity. A trigger fires on every
/// keystroke of a matching character, so `->` typed inside a string or a comment issues a
/// request — and if the server answered those with the same list it answers real code with,
/// the editor would have to re-derive PHP's grammar to suppress them.
///
/// It does not. Every one of the four positions below comes back empty, so the popup opens,
/// receives nothing, and closes itself. The editor keeps no second model of PHP syntax,
/// which is the point: the server knows about heredocs, interpolation and nested comments,
/// and a hand-rolled guess here would disagree with it somewhere and be confidently wrong
/// (RISKS.md #4).
#[test]
fn a_real_server_offers_nothing_after_an_arrow_in_a_string_or_comment() {
    let Some(command) = real_command() else {
        eprintln!("ELLE_LSP_REAL not set; skipping");
        return;
    };

    let mut client = Client::start(&config(&command)).expect("the server must start");

    let path = std::path::PathBuf::from(root()).join("context_for_test.php");
    let source = concat!(
        "<?php\n",
        "\n",
        "class Ctx {\n",
        "    public function alpha(): string { return ''; }\n",
        "}\n",
        "\n",
        "$ctx = new Ctx();\n",
        "$real = $ctx->;\n",
        "$single = 'ctx->x';\n",
        "$double = \"ctx->x\";\n",
        "// a line comment saying $ctx-> here\n",
        "/* a block comment saying $ctx-> here */\n",
    );
    std::fs::write(&path, source).unwrap();
    let uri = path_to_uri(&path).unwrap();
    client.did_open(uri.clone(), "php", source).unwrap();

    let ask = |client: &Client, offset: usize| -> usize {
        let id = client.request_completion(&uri, offset).expect("the request must go out");
        let answer = client
            .await_response::<elle_lsp::lsp_types::CompletionResponse>(&id, Duration::from_secs(20))
            .expect("the server must answer rather than erroring");
        match answer {
            Some(elle_lsp::lsp_types::CompletionResponse::Array(items)) => items.len(),
            Some(elle_lsp::lsp_types::CompletionResponse::List(list)) => list.items.len(),
            None => 0,
        }
    };

    // The control, and it has to come first: if real code offers nothing either, then the
    // server has simply not indexed and the four assertions below would pass while
    // establishing nothing at all. This is the shape of vacuous test this repository keeps
    // finding, so the control is the test.
    let real = source.find("$ctx->;").unwrap() + "$ctx->".len();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut in_code = ask(&client, real);
    while in_code == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        in_code = ask(&client, real);
    }
    eprintln!("after `$ctx->` in real code:      {in_code} items");
    assert!(in_code > 0, "the control must offer something, or the rest proves nothing");

    for (name, offset) in [
        ("single-quoted string", source.find("'ctx->x'").unwrap() + "'ctx->".len()),
        ("double-quoted string", source.find("\"ctx->x\"").unwrap() + "\"ctx->".len()),
        (
            "line comment",
            source.find("// a line comment saying $ctx->").unwrap()
                + "// a line comment saying $ctx->".len(),
        ),
        (
            "block comment",
            source.find("/* a block comment saying $ctx->").unwrap()
                + "/* a block comment saying $ctx->".len(),
        ),
    ] {
        let count = ask(&client, offset);
        eprintln!("after `->` inside a {name:22} {count} items");
        assert_eq!(
            count, 0,
            "the server must decline inside a {name}, or the editor would have to know PHP's \
             grammar itself to suppress the popup"
        );
    }

    let _ = std::fs::remove_file(&path);
    client.stop().unwrap();
}

/// The measurement that ruled out a debounce (#61).
///
/// A trigger multiplies request volume, and #103 settled find-in-project on a 250 ms
/// debounce, so the obvious move was to reuse that number here. This is the test that says
/// not to: on a real project a completion request is roughly two orders of magnitude cheaper
/// than the debounce that would hide it, and adding one would be pure latency.
///
/// **It asserts a generous bound, not the measured figure.** The numbers observed while
/// writing this — 15 ms for the very first request against a server 478 ms old on a
/// 10,061-file project with a 199 MB `vendor/`, and a 1.4 ms warm median — are recorded here
/// as prose because they are facts about one machine. What the assertion pins is the
/// decision they support: that a completion is far below the 250 ms a debounce would cost,
/// which stays true across any machine where the conclusion holds.
#[test]
fn a_real_server_answers_a_completion_far_faster_than_a_debounce_would_cost() {
    let Some(command) = real_command() else {
        eprintln!("ELLE_LSP_REAL not set; skipping");
        return;
    };

    // The number #103 chose for find-in-project, and the one this test exists to reject.
    const FIND_IN_PROJECT_DEBOUNCE: Duration = Duration::from_millis(250);

    let spawned = Instant::now();
    let mut client = Client::start(&config(&command)).expect("the server must start");
    eprintln!("handshake took {:?}", spawned.elapsed());

    let path = std::path::PathBuf::from(root()).join("debounce_for_test.php");
    let source = "<?php\n$x = str;\n";
    std::fs::write(&path, source).unwrap();
    let uri = path_to_uri(&path).unwrap();
    client.did_open(uri.clone(), "php", source).unwrap();
    let offset = source.find("= str;").unwrap() + "= str".len();

    // The very first request, with no settle at all — the cold case the debounce question
    // was really about.
    let started = Instant::now();
    let id = client.request_completion(&uri, offset).expect("the request must go out");
    let _ = client
        .await_response::<elle_lsp::lsp_types::CompletionResponse>(&id, Duration::from_secs(30));
    let cold = started.elapsed();
    eprintln!("the first completion, {:?} after spawn, took {cold:?}", spawned.elapsed());

    let mut times = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        let id = client.request_completion(&uri, offset).expect("the request must go out");
        let _ = client.await_response::<elle_lsp::lsp_types::CompletionResponse>(
            &id,
            Duration::from_secs(30),
        );
        times.push(started.elapsed());
    }
    times.sort();
    let median = times[times.len() / 2];
    eprintln!("warm: min {:?} median {median:?} max {:?}", times[0], times[times.len() - 1]);

    let _ = std::fs::remove_file(&path);

    assert!(
        cold < FIND_IN_PROJECT_DEBOUNCE,
        "a 250 ms debounce would cost more than the request it hides, even cold: {cold:?}"
    );
    assert!(median < FIND_IN_PROJECT_DEBOUNCE, "and far more than the warm case: {median:?}");

    client.stop().unwrap();
}

/// `isIncomplete`, and why filtering the stale list is not good enough (#61).
///
/// This is the measurement behind the re-request. Intelephense caps a bare-word completion
/// at 100 items and marks the list incomplete; it then **re-ranks against each longer
/// prefix**, reaching past its own cap. So the list for `strl` is not a subset of the list
/// for `str` — filtering the earlier answer locally shows a fraction of what the server
/// would have said.
///
/// The honest statement of the harm is *under-reporting*, not wrongness. Both lists here
/// contain `strlen`; what filtering loses is the other matches, and a completion list that
/// shows one row reads as "that is all there is" (RISKS.md #4).
#[test]
fn a_real_server_marks_a_large_list_incomplete_and_re_ranks_on_the_next_character() {
    let Some(command) = real_command() else {
        eprintln!("ELLE_LSP_REAL not set; skipping");
        return;
    };

    let mut client = Client::start(&config(&command)).expect("the server must start");

    let path = std::path::PathBuf::from(root()).join("incomplete_for_test.php");
    let uri = path_to_uri(&path).unwrap();

    let labels_for = |client: &mut Client, typed: &str| -> (Vec<String>, bool) {
        let source = format!("<?php\n$x = {typed};\n");
        std::fs::write(&path, &source).unwrap();
        let _ = client.did_close(&uri);
        client.did_open(uri.clone(), "php", &source).unwrap();
        let offset = source.find(&format!("= {typed};")).unwrap() + 2 + typed.len();
        let id = client.request_completion(&uri, offset).expect("the request must go out");
        let answer = client
            .await_response::<elle_lsp::lsp_types::CompletionResponse>(&id, Duration::from_secs(30))
            .expect("the server must answer");
        match answer {
            Some(elle_lsp::lsp_types::CompletionResponse::List(list)) => {
                (list.items.into_iter().map(|i| i.label).collect(), list.is_incomplete)
            }
            Some(elle_lsp::lsp_types::CompletionResponse::Array(items)) => {
                (items.into_iter().map(|i| i.label).collect(), false)
            }
            None => (Vec::new(), false),
        }
    };

    // Wait for enough of an index that a bare word is a big answer; without this the list is
    // small, complete, and the test proves nothing.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut at_str = labels_for(&mut client, "str");
    while at_str.0.len() < 50 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        at_str = labels_for(&mut client, "str");
    }
    let (str_labels, str_incomplete) = at_str;
    eprintln!("`str`  -> {} items, isIncomplete={str_incomplete}", str_labels.len());
    assert!(
        str_labels.len() >= 50,
        "the server never returned a large list, so there is nothing to truncate: {}",
        str_labels.len()
    );
    assert!(
        str_incomplete,
        "a truncated list must be marked incomplete, or #61's re-request \
                             has no trigger to act on"
    );

    let (strl_labels, _) = labels_for(&mut client, "strl");
    eprintln!("`strl` -> {} items", strl_labels.len());

    // What a client that filtered the stale list would have shown instead.
    let filtered: Vec<&String> =
        str_labels.iter().filter(|l| l.to_lowercase().starts_with("strl")).collect();
    eprintln!("filtering the stale `str` list by `strl` would show {} items", filtered.len());

    let _ = std::fs::remove_file(&path);

    assert!(
        strl_labels.len() > filtered.len(),
        "re-requesting must beat filtering the stale list, or the whole re-request is \
         pointless: {} vs {}",
        strl_labels.len(),
        filtered.len()
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
