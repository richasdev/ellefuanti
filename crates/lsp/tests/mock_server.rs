//! A scriptable mock language server, and the end-to-end tests that drive it.
//!
//! The mock exists so the suite proves the client works without Intelephense — or any
//! other server — being installed. `cargo test` on a clean checkout must exercise the
//! real lifecycle, the real framing and the real correlation logic, because a test
//! suite that skips when a binary is missing is a suite that silently stops testing.
//!
//! It talks over `os_pipe` pipes rather than in-process channels deliberately: that
//! puts the actual `Content-Length` framing, partial reads and all, on the path under
//! test. The only thing it does not exercise is `Command::spawn`, which
//! `process::tests` covers separately.

use std::io::{BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use elle_lsp::jsonrpc::{Incoming, RequestId};
use elle_lsp::{Client, Connection, ServerConfig, ServerEvent, SyncKind};
use serde_json::{Value, json};

mod support;
use support::{Pipes, read_frame, write_frame};

/// How a mock server answers one request.
enum Reply {
    /// Answer immediately with this result.
    Result(Value),
    /// Answer with a JSON-RPC error.
    Error(i64, String),
    /// Never answer, to model a hung or indexing server.
    Silence,
    /// Wait, then answer — long enough for a test to cancel first.
    Delayed(Duration, Value),
}

/// Everything the mock server saw, for assertions about ordering.
#[derive(Default)]
struct Journal {
    /// Method names in the order they arrived, notifications included.
    methods: Vec<String>,
    /// Full params, keyed by method, for content assertions.
    params: Vec<(String, Value)>,
}

struct MockServer {
    journal: Arc<Mutex<Journal>>,
    exited: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn methods(&self) -> Vec<String> {
        self.journal.lock().unwrap().methods.clone()
    }

    fn params_for(&self, method: &str) -> Vec<Value> {
        self.journal
            .lock()
            .unwrap()
            .params
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Builds a mock server and a `Connection` wired to it.
///
/// `respond` decides what to reply to each request method. Notifications are journalled
/// and never answered, as the protocol requires.
fn mock(
    capabilities: Value,
    respond: impl Fn(&str, &Value) -> Reply + Send + 'static,
) -> (Connection, MockServer) {
    let Pipes { client_reader, client_writer, server_reader, server_writer } = Pipes::new();

    let journal = Arc::new(Mutex::new(Journal::default()));
    let exited = Arc::new(AtomicBool::new(false));

    let thread_journal = Arc::clone(&journal);
    let thread_exited = Arc::clone(&exited);

    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(server_reader);
        let mut writer = server_writer;

        while let Ok(Some(body)) = read_frame(&mut reader) {
            let Some(message) = Incoming::parse(&body) else { continue };

            let (id, method, params) = match message {
                Incoming::Request { id, method, params } => (Some(id), method, params),
                Incoming::Notification { method, params } => (None, method, params),
                // The mock never sends requests, so it never receives replies.
                Incoming::Response { .. } => continue,
            };

            {
                let mut journal = thread_journal.lock().unwrap();
                journal.methods.push(method.clone());
                journal.params.push((method.clone(), params.clone()));
            }

            if method == "exit" {
                thread_exited.store(true, Ordering::SeqCst);
                return;
            }

            let Some(id) = id else { continue };

            // `initialize` is answered by the harness so every test does not have to.
            if method == "initialize" {
                let result = json!({ "capabilities": capabilities });
                let _ = reply(&mut writer, &id, Ok(result));
                continue;
            }
            if method == "shutdown" {
                let _ = reply(&mut writer, &id, Ok(Value::Null));
                continue;
            }

            match respond(&method, &params) {
                Reply::Result(value) => {
                    let _ = reply(&mut writer, &id, Ok(value));
                }
                Reply::Error(code, message) => {
                    let _ = reply(&mut writer, &id, Err((code, message)));
                }
                Reply::Silence => {}
                Reply::Delayed(delay, value) => {
                    // A thread per delayed reply, so a slow request does not block the
                    // ones behind it — exactly how a real server behaves, and the
                    // condition cancellation has to cope with.
                    std::thread::sleep(delay);
                    let _ = reply(&mut writer, &id, Ok(value));
                }
            }
        }
    });

    let connection = Connection::new(client_reader, client_writer, "mock".into());
    (connection, MockServer { journal, exited, handle: Some(handle) })
}

fn reply(
    writer: &mut impl Write,
    id: &RequestId,
    result: Result<Value, (i64, String)>,
) -> std::io::Result<()> {
    let message = match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    };
    write_frame(writer, serde_json::to_vec(&message).unwrap().as_slice())
}

fn config() -> ServerConfig {
    ServerConfig::new("mock", "mock", std::env::temp_dir()).with_language_ids(["php"])
}

fn full_capabilities() -> Value {
    json!({
        "positionEncoding": "utf-16",
        "textDocumentSync": 2,
        "completionProvider": { "triggerCharacters": ["$", ">", ":"] },
        "hoverProvider": true,
        "definitionProvider": true,
        "referencesProvider": true,
        "documentSymbolProvider": true,
        "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
    })
}

fn uri() -> lsp_types::Uri {
    "file:///srv/app/Model.php".parse().unwrap()
}

/// A serialised `initialize` reply advertising the full capability set.
fn initialize_reply(id: &RequestId) -> Vec<u8> {
    let result = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "capabilities": full_capabilities() }
    });
    serde_json::to_vec(&result).unwrap()
}

fn open_client(
    capabilities: Value,
    respond: impl Fn(&str, &Value) -> Reply + Send + 'static,
) -> (Client, MockServer) {
    let (connection, server) = mock(capabilities, respond);
    let client = Client::connect(&config(), connection).expect("handshake should succeed");
    (client, server)
}

// --- lifecycle -------------------------------------------------------------------

#[test]
fn handshake_sends_initialize_then_initialized() {
    // The specification forbids any other traffic until `initialized` has gone out, and
    // servers enforce it. Ordering is the assertion, not just delivery.
    let (client, mut server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    drop(client);
    server.join();

    let methods = server.methods();
    assert_eq!(methods[0], "initialize");
    assert_eq!(methods[1], "initialized");
}

#[test]
fn shutdown_precedes_exit() {
    // Sending `exit` first makes a conforming server exit with a non-zero status.
    let (mut client, mut server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    client.stop().unwrap();
    server.join();

    let methods = server.methods();
    let shutdown = methods.iter().position(|m| m == "shutdown").expect("shutdown must be sent");
    let exit = methods.iter().position(|m| m == "exit").expect("exit must be sent");
    assert!(shutdown < exit, "shutdown must precede exit, got {methods:?}");
    assert!(server.has_exited());
}

#[test]
fn stop_is_idempotent() {
    let (mut client, mut server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    client.stop().unwrap();
    // A second stop must not error or send a second shutdown.
    client.stop().unwrap();
    server.join();

    let shutdowns = server.methods().iter().filter(|m| *m == "shutdown").count();
    assert_eq!(shutdowns, 1);
}

#[test]
fn dropping_the_client_shuts_the_server_down() {
    // §24 in reverse: a leaked server is a runaway process on the user's machine.
    let (client, mut server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    drop(client);
    server.join();
    assert!(server.has_exited(), "dropping the client must send exit");
}

#[test]
fn capabilities_are_read_from_the_server_not_assumed() {
    let (client, _server) = open_client(
        json!({ "textDocumentSync": 1, "hoverProvider": true }),
        |_, _| Reply::Silence,
    );

    let capabilities = client.capabilities();
    assert!(capabilities.hover);
    // Everything the server did not advertise must read as unsupported.
    assert!(!capabilities.completion);
    assert!(!capabilities.definition);
    assert!(!capabilities.references);
    assert!(!capabilities.document_symbols);
    assert!(!capabilities.signature_help);
    assert_eq!(capabilities.sync, SyncKind::Full);
}

#[test]
fn trigger_characters_come_from_the_server() {
    // Hardcoding PHP's `$` and `->` here would be precisely the backend-specific leak
    // RISKS.md #2 forbids, so the client must simply relay what it was told.
    let (client, _server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    assert_eq!(client.capabilities().completion_triggers, ["$", ">", ":"]);
    assert_eq!(client.capabilities().signature_help_triggers, ["(", ","]);
}

#[test]
fn a_server_omitting_sync_capability_gets_full_documents() {
    // Guessing incremental would corrupt the server's copy silently.
    let (client, _server) = open_client(json!({}), |_, _| Reply::Silence);
    assert_eq!(client.capabilities().sync, SyncKind::Full);
}

#[test]
fn a_server_that_dies_during_startup_is_reported_not_panicked() {
    // §24: a server that never starts must leave the editor working, which means
    // `connect` returns an error rather than panicking or blocking for 30 seconds.
    let Pipes {
        client_reader,
        client_writer,
        server_reader,
        server_writer,
    } = Pipes::new();

    // Drop both server ends immediately: the client sees EOF where a reply should be.
    drop(server_reader);
    drop(server_writer);

    let connection = Connection::new(client_reader, client_writer, "dying".into());
    let started = std::time::Instant::now();
    let result = Client::connect(&config(), connection);

    assert!(result.is_err(), "a dead server must fail the handshake");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must not wait out the initialize timeout; took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_server_refusing_to_initialize_is_reported() {
    // A licence-gated server declining to start is exactly this case, and it must be a
    // clear message rather than a crash.
    let Pipes {
        client_reader,
        client_writer,
        server_reader,
        server_writer,
    } = Pipes::new();

    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(server_reader);
        let mut writer = server_writer;
        if let Ok(Some(body)) = read_frame(&mut reader)
            && let Some(Incoming::Request { id, .. }) = Incoming::parse(&body)
        {
            let error = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": "licence expired" }
            });
            let _ = write_frame(&mut writer, &serde_json::to_vec(&error).unwrap());
        }
    });

    let connection = Connection::new(client_reader, client_writer, "refusing".into());
    let err = match Client::connect(&config(), connection) {
        Ok(_) => panic!("initialize should have failed"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("licence expired"), "{err}");

    let _ = handle.join();
}

// --- requests and correlation -------------------------------------------------------

#[test]
fn responses_are_matched_to_their_request_by_id() {
    // Requests are answered out of order, which is legal and common: a fast hover can
    // overtake a slow completion. Matching by arrival order instead of by id would
    // hand each caller the other's answer.
    let (mut client, _server) = open_client(full_capabilities(), |method, _| match method {
        "textDocument/completion" => {
            Reply::Delayed(Duration::from_millis(120), json!({ "isIncomplete": false, "items": [] }))
        }
        "textDocument/hover" => Reply::Result(json!({ "contents": "fast" })),
        _ => Reply::Silence,
    });

    client.did_open(uri(), "php", "<?php\n").unwrap();

    let completion_id = client.request_completion(&uri(), 6).unwrap();
    // Issued second, answered first.
    let hover = client.hover(&uri(), 6).unwrap().expect("hover should answer");
    assert_eq!(
        serde_json::to_value(&hover).unwrap()["contents"],
        json!("fast"),
        "the hover reply must not be delivered to the completion caller"
    );

    let completion: Option<lsp_types::CompletionResponse> =
        client.await_response(&completion_id, Duration::from_secs(5)).unwrap();
    assert!(completion.is_some(), "the delayed completion must still arrive");
}

#[test]
fn every_supported_request_reaches_the_server_with_a_position() {
    let (mut client, _server) = open_client(full_capabilities(), |method, _| match method {
        "textDocument/completion" => Reply::Result(json!({ "isIncomplete": false, "items": [] })),
        "textDocument/hover" => Reply::Result(json!({ "contents": "x" })),
        "textDocument/definition" => Reply::Result(json!([])),
        "textDocument/references" => Reply::Result(json!([])),
        "textDocument/documentSymbol" => Reply::Result(json!([])),
        "textDocument/signatureHelp" => Reply::Result(json!({ "signatures": [] })),
        _ => Reply::Silence,
    });

    client.did_open(uri(), "php", "<?php\n$x = 1;\n").unwrap();

    assert!(client.completion(&uri(), 6).unwrap().is_some());
    assert!(client.hover(&uri(), 6).unwrap().is_some());
    assert!(client.definition(&uri(), 6).unwrap().is_some());
    assert!(client.references(&uri(), 6, true).unwrap().is_some());
    assert!(client.document_symbols(&uri()).unwrap().is_some());
    assert!(client.signature_help(&uri(), 6).unwrap().is_some());
}

#[test]
fn references_forwards_the_include_declaration_flag() {
    let (mut client, server) = open_client(full_capabilities(), |_, _| Reply::Result(json!([])));
    client.did_open(uri(), "php", "<?php\n").unwrap();
    client.references(&uri(), 6, false).unwrap();

    let params = server.params_for("textDocument/references");
    assert_eq!(params[0]["context"]["includeDeclaration"], json!(false));
}

#[test]
fn a_null_result_is_no_answer_rather_than_an_error() {
    // Hover over whitespace legitimately replies null; surfacing that as an error
    // would put a failure message on screen every time the cursor moved.
    let (mut client, _server) = open_client(full_capabilities(), |_, _| Reply::Result(Value::Null));
    client.did_open(uri(), "php", "<?php\n").unwrap();
    assert!(client.hover(&uri(), 6).unwrap().is_none());
}

#[test]
fn an_unimplemented_method_degrades_quietly() {
    // A lighter backend must be a drop-in replacement for a heavier one, so "I do not
    // implement this" is a `None`, not a failure.
    let (mut client, _server) =
        open_client(full_capabilities(), |_, _| Reply::Error(-32601, "not implemented".into()));
    client.did_open(uri(), "php", "<?php\n").unwrap();
    assert!(client.hover(&uri(), 6).unwrap().is_none());
}

#[test]
fn a_genuine_server_error_is_surfaced() {
    let (mut client, _server) =
        open_client(full_capabilities(), |_, _| Reply::Error(-32603, "internal boom".into()));
    client.did_open(uri(), "php", "<?php\n").unwrap();
    let err = client.hover(&uri(), 6).unwrap_err().to_string();
    assert!(err.contains("internal boom"), "{err}");
}

#[test]
fn requesting_against_an_unopened_document_is_an_error() {
    let (client, _server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    let err = client.hover(&uri(), 0).unwrap_err().to_string();
    assert!(err.contains("not open"), "{err}");
}

// --- cancellation --------------------------------------------------------------------

#[test]
fn a_cancelled_request_stops_waiting_immediately() {
    // §22: typing quickly must not queue a completion per keystroke. The local wait
    // must end at once, whether or not the server ever bothers to reply.
    let (mut client, _server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    client.did_open(uri(), "php", "<?php\n").unwrap();

    let id = client.request_completion(&uri(), 6).unwrap();
    client.cancel(&id);

    let started = std::time::Instant::now();
    let result: Option<lsp_types::CompletionResponse> =
        client.await_response(&id, Duration::from_secs(30)).unwrap();

    assert!(result.is_none(), "a cancelled request yields no answer");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancelling must not wait for the timeout; took {:?}",
        started.elapsed()
    );
}

#[test]
fn cancelling_notifies_the_server() {
    let (mut client, server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    client.did_open(uri(), "php", "<?php\n").unwrap();

    let id = client.request_completion(&uri(), 6).unwrap();
    client.cancel(&id);

    // The notification is best-effort but must be sent: it is what lets the server
    // stop indexing work nobody is waiting for.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if server.methods().iter().any(|m| m == "$/cancelRequest") {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("expected a $/cancelRequest, saw {:?}", server.methods());
}

#[test]
fn a_late_reply_to_a_cancelled_request_is_discarded() {
    // The server is allowed to finish and answer anyway. That reply must not be
    // delivered to whoever asks next.
    let (mut client, _server) = open_client(full_capabilities(), |method, _| {
        if method == "textDocument/completion" {
            Reply::Delayed(Duration::from_millis(80), json!({ "isIncomplete": false, "items": [] }))
        } else {
            Reply::Result(json!({ "contents": "hover" }))
        }
    });
    client.did_open(uri(), "php", "<?php\n").unwrap();

    let stale = client.request_completion(&uri(), 6).unwrap();
    client.cancel(&stale);

    // Let the stale reply arrive while nothing is waiting for it.
    std::thread::sleep(Duration::from_millis(200));

    let hover = client.hover(&uri(), 6).unwrap().expect("hover must answer");
    assert_eq!(serde_json::to_value(&hover).unwrap()["contents"], json!("hover"));
}

#[test]
fn a_server_acknowledged_cancellation_is_not_an_error() {
    // -32800 RequestCancelled is the expected outcome of cancelling, not a fault to
    // show the user.
    let (mut client, _server) =
        open_client(full_capabilities(), |_, _| Reply::Error(-32800, "cancelled".into()));
    client.did_open(uri(), "php", "<?php\n").unwrap();
    assert!(client.hover(&uri(), 6).unwrap().is_none());
}

#[test]
fn rapid_typing_leaves_only_the_last_request_outstanding() {
    // The shape the editor actually uses: issue, cancel the previous, repeat.
    let (mut client, _server) = open_client(full_capabilities(), |method, _| {
        if method == "textDocument/completion" {
            Reply::Delayed(Duration::from_millis(50), json!({ "isIncomplete": false, "items": [] }))
        } else {
            Reply::Silence
        }
    });
    client.did_open(uri(), "php", "<?php\n$this->").unwrap();

    let mut previous: Option<elle_lsp::RequestId> = None;
    for _ in 0..10 {
        let id = client.request_completion(&uri(), 13).unwrap();
        if let Some(stale) = previous.replace(id) {
            client.cancel(&stale);
        }
    }

    let last = previous.expect("a request must remain");
    let result: Option<lsp_types::CompletionResponse> =
        client.await_response(&last, Duration::from_secs(5)).unwrap();
    assert!(result.is_some(), "the final request must still be answered");
}

// --- diagnostics ----------------------------------------------------------------------

#[test]
fn diagnostics_notifications_become_events() {
    let Pipes { client_reader, client_writer, server_reader, server_writer } = Pipes::new();

    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(server_reader);
        let mut writer = server_writer;
        while let Ok(Some(body)) = read_frame(&mut reader) {
            let Some(Incoming::Request { id, method, .. }) = Incoming::parse(&body) else {
                continue;
            };
            if method == "initialize" {
                write_frame(&mut writer, &initialize_reply(&id)).unwrap();

                // Push diagnostics unprompted, the way a real server does after
                // indexing a file.
                let diagnostics = json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": "file:///srv/app/Model.php",
                        "version": 1,
                        "diagnostics": [{
                            "range": {
                                "start": { "line": 1, "character": 0 },
                                "end":   { "line": 1, "character": 5 }
                            },
                            "severity": 1,
                            "message": "Undefined variable $ação"
                        }]
                    }
                });
                write_frame(&mut writer, &serde_json::to_vec(&diagnostics).unwrap()).unwrap();
            }
        }
    });

    let connection = Connection::new(client_reader, client_writer, "mock".into());
    let client = Client::connect(&config(), connection).unwrap();

    let events = client.wait_for_events(Duration::from_secs(5));
    let diagnostics = events
        .iter()
        .find_map(|event| match event {
            ServerEvent::Diagnostics { uri, diagnostics, .. } => Some((uri, diagnostics)),
            _ => None,
        })
        .expect("diagnostics should have been surfaced");

    assert_eq!(diagnostics.0.as_str(), "file:///srv/app/Model.php");
    assert_eq!(diagnostics.1.len(), 1);
    assert_eq!(diagnostics.1[0].message, "Undefined variable $ação");

    drop(client);
    let _ = handle.join();
}

// --- fault isolation (§24) ---------------------------------------------------------------

#[test]
fn a_server_that_dies_mid_request_reports_rather_than_hangs() {
    // The single most important fault case: the editor must keep working, and the
    // caller must get an error instead of waiting forever.
    let Pipes { client_reader, client_writer, server_reader, server_writer } = Pipes::new();

    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(server_reader);
        let mut writer = server_writer;
        while let Ok(Some(body)) = read_frame(&mut reader) {
            let Some(Incoming::Request { id, method, .. }) = Incoming::parse(&body) else {
                continue;
            };
            if method == "initialize" {
                write_frame(&mut writer, &initialize_reply(&id)).unwrap();
            } else {
                // Die mid-request: drop both streams without answering.
                return;
            }
        }
    });

    let connection = Connection::new(client_reader, client_writer, "mock".into());
    let mut client = Client::connect(&config(), connection).unwrap();
    client.did_open(uri(), "php", "<?php\n").unwrap();

    let started = std::time::Instant::now();
    let result = client.hover(&uri(), 6);

    assert!(result.is_err(), "a dead server must produce an error, not a null answer");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must not wait out the full timeout; took {:?}",
        started.elapsed()
    );
    assert!(!client.is_alive(), "the client must know the server is gone");

    let _ = handle.join();
}

#[test]
fn a_hung_server_times_out_rather_than_blocking_forever() {
    let (mut client, _server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    client.did_open(uri(), "php", "<?php\n").unwrap();

    let id = client.request_completion(&uri(), 6).unwrap();
    let started = std::time::Instant::now();
    let outcome: Result<Option<lsp_types::CompletionResponse>, _> =
        client.await_response(&id, Duration::from_millis(200));

    assert!(outcome.is_err(), "a silent server must time out");
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(started.elapsed() < Duration::from_secs(3));
    // And the client is still usable afterwards, not poisoned.
    assert!(client.is_alive());
}

#[test]
fn garbage_from_the_server_does_not_end_the_session() {
    // One malformed frame must not stop LSP for the rest of the session.
    let Pipes { client_reader, client_writer, server_reader, server_writer } = Pipes::new();

    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(server_reader);
        let mut writer = server_writer;
        while let Ok(Some(body)) = read_frame(&mut reader) {
            let Some(Incoming::Request { id, method, .. }) = Incoming::parse(&body) else {
                continue;
            };
            if method == "initialize" {
                // A well-framed but semantically meaningless message first.
                write_frame(&mut writer, br#"{"jsonrpc":"2.0","nonsense":true}"#).unwrap();
                write_frame(&mut writer, b"not json at all").unwrap();

                write_frame(&mut writer, &initialize_reply(&id)).unwrap();
            }
        }
    });

    let connection = Connection::new(client_reader, client_writer, "mock".into());
    // The handshake still completes despite the garbage that preceded the reply.
    let client = Client::connect(&config(), connection).expect("garbage must not break the handshake");
    assert!(client.is_alive());

    drop(client);
    let _ = handle.join();
}

#[test]
fn a_server_request_is_answered_so_it_does_not_stall() {
    // A server left waiting on a reply can stall its own queue, which the user sees as
    // a hang.
    let Pipes { client_reader, client_writer, server_reader, server_writer } = Pipes::new();
    let answered = Arc::new(AtomicBool::new(false));
    let thread_answered = Arc::clone(&answered);

    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(server_reader);
        let mut writer = server_writer;
        while let Ok(Some(body)) = read_frame(&mut reader) {
            match Incoming::parse(&body) {
                Some(Incoming::Request { id, method, .. }) if method == "initialize" => {
                    write_frame(&mut writer, &initialize_reply(&id)).unwrap();

                    // Ask the client something, as servers do at startup.
                    let request = json!({
                        "jsonrpc": "2.0",
                        "id": 9001,
                        "method": "workspace/configuration",
                        "params": { "items": [{ "section": "php" }] }
                    });
                    write_frame(&mut writer, &serde_json::to_vec(&request).unwrap()).unwrap();
                }
                // The client's reply to our request.
                Some(Incoming::Response { id: RequestId::Number(9001), .. }) => {
                    thread_answered.store(true, Ordering::SeqCst);
                    return;
                }
                _ => {}
            }
        }
    });

    let connection = Connection::new(client_reader, client_writer, "mock".into());
    let client = Client::connect(&config(), connection).unwrap();
    let _ = client.wait_for_events(Duration::from_secs(2));

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !answered.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(answered.load(Ordering::SeqCst), "the client must answer a server request");

    drop(client);
    let _ = handle.join();
}

// --- document sync over the wire ----------------------------------------------------------

#[test]
fn document_lifecycle_notifications_reach_the_server_in_order() {
    let (mut client, mut server) = open_client(full_capabilities(), |_, _| Reply::Silence);

    client.did_open(uri(), "php", "<?php\n// ação\n").unwrap();
    let edit = elle_text::Edit::new(6..6, "// olá\n", "");
    client.did_change(&uri(), &edit).unwrap();
    client.did_close(&uri()).unwrap();
    client.stop().unwrap();
    server.join();

    let methods: Vec<String> = server
        .methods()
        .into_iter()
        .filter(|m| m.starts_with("textDocument/"))
        .collect();
    assert_eq!(
        methods,
        [
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didClose"
        ]
    );
}

#[test]
fn incremental_changes_carry_utf16_ranges() {
    let (mut client, _server) = open_client(full_capabilities(), |_, _| Reply::Silence);

    // "// ação" is 7 characters but 9 bytes. An edit after it must be reported at the
    // UTF-16 column, not the byte column.
    let text = "<?php\n// ação\n";
    client.did_open(uri(), "php", text).unwrap();

    let offset = text.len();
    client
        .did_change(&uri(), &elle_text::Edit::new(offset..offset, "$x = 1;", ""))
        .unwrap();

    let changes = wait_for_params(&_server, "textDocument/didChange");
    let range = &changes[0]["contentChanges"][0]["range"];
    assert_eq!(range["start"]["line"], 2);
    assert_eq!(range["start"]["character"], 0);
}

#[test]
fn a_utf8_server_receives_byte_columns() {
    // The same edit, against a server that negotiated UTF-8, must produce different
    // numbers. If both encodings produced the same output the conversion would be
    // doing nothing.
    let mut capabilities = full_capabilities();
    capabilities["positionEncoding"] = json!("utf-8");

    let (mut client, server) = open_client(capabilities, |_, _| Reply::Silence);
    assert_eq!(client.encoding(), elle_lsp::OffsetEncoding::Utf8);

    let text = "// ação";
    client.did_open(uri(), "php", text).unwrap();
    let offset = text.len();
    client
        .did_change(&uri(), &elle_text::Edit::new(offset..offset, "!", ""))
        .unwrap();

    let changes = wait_for_params(&server, "textDocument/didChange");
    // 9 bytes, versus 7 UTF-16 code units.
    assert_eq!(changes[0]["contentChanges"][0]["range"]["start"]["character"], 9);
}

#[test]
fn the_same_edit_differs_between_encodings() {
    let text = "// ação";
    let offset = text.len();

    let character_for = |encoding: &str| -> i64 {
        let mut capabilities = full_capabilities();
        capabilities["positionEncoding"] = json!(encoding);
        let (mut client, server) = open_client(capabilities, |_, _| Reply::Silence);
        client.did_open(uri(), "php", text).unwrap();
        client
            .did_change(&uri(), &elle_text::Edit::new(offset..offset, "!", ""))
            .unwrap();
        let changes = wait_for_params(&server, "textDocument/didChange");
        changes[0]["contentChanges"][0]["range"]["start"]["character"].as_i64().unwrap()
    };

    assert_eq!(character_for("utf-8"), 9);
    assert_eq!(character_for("utf-16"), 7);
    assert_eq!(character_for("utf-32"), 7);
}

#[test]
fn a_full_sync_server_receives_whole_documents() {
    let mut capabilities = full_capabilities();
    capabilities["textDocumentSync"] = json!(1);

    let (mut client, server) = open_client(capabilities, |_, _| Reply::Silence);
    client.did_open(uri(), "php", "<?php\n").unwrap();
    client
        .did_change(&uri(), &elle_text::Edit::new(6..6, "$x = 1;", ""))
        .unwrap();

    let changes = wait_for_params(&server, "textDocument/didChange");
    let change = &changes[0]["contentChanges"][0];
    assert!(change.get("range").is_none() || change["range"].is_null());
    assert_eq!(change["text"], "<?php\n$x = 1;");
}

#[test]
fn document_versions_increase_with_each_change() {
    let (mut client, server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    client.did_open(uri(), "php", "a").unwrap();
    for _ in 0..3 {
        client
            .did_change(&uri(), &elle_text::Edit::new(0..0, "x", ""))
            .unwrap();
    }

    let changes = wait_for_n_params(&server, "textDocument/didChange", 3);
    let versions: Vec<i64> =
        changes.iter().map(|c| c["textDocument"]["version"].as_i64().unwrap()).collect();
    assert_eq!(versions, [2, 3, 4]);
}

#[test]
fn changes_to_an_untracked_document_are_ignored_not_errors() {
    // Editing a file the server was never told about is ordinary.
    let (mut client, _server) = open_client(full_capabilities(), |_, _| Reply::Silence);
    let other: lsp_types::Uri = "file:///srv/app/Never.php".parse().unwrap();
    assert!(client.did_change(&other, &elle_text::Edit::new(0..0, "x", "")).is_ok());
}

/// Polls the journal until `method` has arrived at least once, so tests do not race the
/// mock's thread.
fn wait_for_params(server: &MockServer, method: &str) -> Vec<Value> {
    wait_for_n_params(server, method, 1)
}

/// Polls until `count` occurrences of `method` have arrived.
///
/// The count matters: notifications are asynchronous, so waiting for merely *one*
/// `didChange` and then asserting about three is a race that passes most of the time
/// and fails under load. Waiting for the number the test actually asserts on is the
/// difference between a real assertion and a flaky one.
fn wait_for_n_params(server: &MockServer, method: &str, count: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let params = server.params_for(method);
        if params.len() >= count {
            return params;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "the server received {} of an expected {count} {method}; saw {:?}",
        server.params_for(method).len(),
        server.methods()
    );
}
