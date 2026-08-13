//! Chat through the user's own `codex` login (#99): a subscription instead of a key.
//!
//! # Why this exists at all
//!
//! Every other provider in [`crate::ai`] wants an API key. The owner already pays for a
//! ChatGPT subscription, and the `codex` CLI already knows how to use it — so the cheapest
//! honest path to "chat without a second bill" is to drive that CLI instead of inventing
//! an auth flow. PhpStorm's Codex integration is the same shape.
//!
//! # The credential is never ours
//!
//! `codex login` writes `~/.codex/auth.json` and the CLI reads it; this editor never
//! opens that file, never holds a token, never logs one. [`availability`] checks that the
//! path *exists* and stops there — existence is the whole question, contents are none of
//! our business. Nothing here is bundled with an account either: we spawn whatever
//! `codex` is on the user's PATH, and it answers with whoever is logged into it.
//!
//! Anthropic forbids the equivalent for Claude, which is why there is no Claude OAuth
//! provider next to this one and must not be.
//!
//! # The protocol, as probed rather than as documented
//!
//! `codex app-server` speaks JSON-RPC 2.0 over newline-delimited JSON on stdin/stdout,
//! one message per line. The published docs still describe `newConversation`, which the
//! binary no longer has; everything below was captured live against `codex-cli 0.146.0`
//! and the exact captured lines are the test fixtures at the bottom of this file.
//!
//! ```text
//! → initialize   {"clientInfo":{"name":"ellefuanti","version":"…"}}
//! ← {"id":1,"result":{"userAgent":…,"codexHome":…}}
//! → thread/start {"cwd":"<project root>"}
//! ← {"id":2,"result":{"thread":{"id":"019ff658-…"},"model":"gpt-5.6-luna",…}}
//! → turn/start   {"threadId":"019ff658-…","input":[{"type":"text","text":"…"}]}
//! ← item/agentMessage/delta ×N, then turn/completed
//! ```
//!
//! # Read-only, in both modes, and that is what makes Agent mode safe
//!
//! `thread/start` defaults to `sandbox: readOnly` with `approvalPolicy: on-request`, and
//! **this stays true in Agent mode**. That is the opposite of the obvious implementation,
//! so the probe that settled it is worth recording:
//!
//! | `sandbox`         | what the CLI does when it wants to edit a file          |
//! |-------------------|---------------------------------------------------------|
//! | `workspace-write` | writes it. No approval request, no patch notification.  |
//! | `read-only`       | emits `item/fileChange/requestApproval` and **waits**.  |
//!
//! Asking for write access would therefore *remove* the consent step rather than add one:
//! inside a `workspace-write` sandbox a write is not an escape, so nothing escalates and
//! the file is already changed by the time the panel hears about it. Under `read-only`
//! every write is an escape, which is exactly the interception point Agent mode needs —
//! declining leaves the file byte-for-byte untouched (verified against the real CLI).
//!
//! The content arrives *before* the question, as an `item/started` whose item is a
//! `fileChange` carrying the path and a ready-made unified diff, correlated to the
//! approval by `itemId`:
//!
//! ```text
//! ← item/started {"item":{"type":"fileChange","id":"exec-10e2…",
//!                  "changes":[{"path":"…/hello.php","kind":{"type":"update"},
//!                              "diff":"@@ -2,3 +2,3 @@\n…"}]}}
//! ← item/fileChange/requestApproval {"id":0,"params":{"itemId":"exec-10e2…"}}
//! → {"id":0,"result":{"decision":"accept"|"decline"}}
//! ```
//!
//! The `cwd` param is verified accepted — the returned thread echoes it back, and the
//! spawned child's own working directory is set to the same root as a belt-and-braces
//! fallback, since that is what actually roots a thread when a param is ignored.

use std::path::Path;

use serde_json::{Value, json};

/// The JSON-RPC ids, fixed per message kind. The app-server answers `initialize` and
/// `thread/start` by id, and nothing here ever has two of either in flight, so constants
/// beat a counter that would need synchronising across the reader and the writer.
pub const INITIALIZE_ID: u64 = 1;
pub const THREAD_START_ID: u64 = 2;
/// Every turn reuses this id: replies to `turn/start` are not what drives the UI (the
/// `item/agentMessage/delta` notifications are), so a unique id per turn would buy
/// nothing and cost a counter.
pub const TURN_START_ID: u64 = 3;
pub const INTERRUPT_ID: u64 = 4;

/// The handshake. `clientInfo` is what shows up in the CLI's own user-agent string, so
/// it names this editor honestly rather than impersonating another client.
pub fn initialize_request(client_version: &str) -> String {
    line(json!({
        "jsonrpc": "2.0",
        "id": INITIALIZE_ID,
        "method": "initialize",
        "params": {"clientInfo": {"name": "ellefuanti", "version": client_version}},
    }))
}

/// Opens the conversation. One thread per panel session — the thread *is* the history,
/// which is why the panel keeps the child alive across turns instead of respawning.
///
/// `cwd` is how Codex gets to read the open project for context. Omitted when no folder
/// is open, because a thread rooted at wherever the app happened to launch would let it
/// read a directory the user never opened.
///
/// `sandbox` and `approvalPolicy` are sent explicitly rather than left to the CLI's
/// defaults — in **both** modes, and both to the same values. They are the settings that
/// decide whether a write can happen without being asked about (see the module docs), so
/// they are stated here where they can be read, not inherited from whatever a future CLI
/// version or a user's `~/.codex/config.toml` happens to default to.
pub fn thread_start_request(cwd: Option<&Path>) -> String {
    let mut params = json!({
        // Every write is a sandbox escape, and an escape is a question. This is what
        // makes "nothing reaches disk unapproved" a property of the protocol rather than
        // a promise the panel makes.
        "sandbox": "read-only",
        "approvalPolicy": "on-request",
    });
    if let Some(cwd) = cwd {
        params["cwd"] = json!(cwd.to_string_lossy());
    }
    line(json!({
        "jsonrpc": "2.0",
        "id": THREAD_START_ID,
        "method": "thread/start",
        "params": params,
    }))
}

/// One user message. The `input` array is a content-block list like the HTTP wires'; only
/// text blocks are sent, because only text is what the panel can produce.
pub fn turn_start_request(thread_id: &str, text: &str) -> String {
    line(json!({
        "jsonrpc": "2.0",
        "id": TURN_START_ID,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}],
        },
    }))
}

/// Cancel, the polite half. The panel also kills the child as a fallback — an interrupt
/// that is never read because the reader already went away must not leave a turn running.
pub fn interrupt_request(thread_id: &str) -> String {
    line(json!({
        "jsonrpc": "2.0",
        "id": INTERRUPT_ID,
        "method": "turn/interrupt",
        "params": {"threadId": thread_id},
    }))
}

/// The answer to an `item/fileChange/requestApproval`, addressed to the request's own id.
///
/// This is the one message in the protocol that can let bytes reach the user's disk, so
/// it is only ever built from a click on Apply — never from a timer, a default, or a
/// "the user probably meant yes". `accept` writes; `decline` leaves the file untouched
/// and lets the turn continue, so the model can adapt instead of silently retrying.
///
/// `acceptForSession` is deliberately **not** offered: it is the "trust this session"
/// escape hatch, and the whole point of Agent mode here is that every file is seen before
/// it is written.
pub fn approval_response(request_id: u64, approve: bool) -> String {
    line(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {"decision": if approve { "accept" } else { "decline" }},
    }))
}

/// One file a turn wants to change, as the CLI describes it.
///
/// The `diff` is the CLI's own unified diff for this file — already in the `@@` format the
/// diff renderer reads, which is why the panel does not recompute one for the Codex path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposedChange {
    /// Absolute path, as the CLI reports it.
    pub path: String,
    /// `"add"`, `"update"` or `"delete"` — what the change does to the file.
    pub kind: String,
    pub diff: String,
}

/// Serialises one JSON-RPC message as exactly one line: the framing *is* the newline.
fn line(value: Value) -> String {
    format!("{value}\n")
}

/// One parsed line of the app-server's output, reduced to what the panel acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexEvent {
    /// The `initialize` reply landed; the handshake may proceed.
    Initialized,
    /// The thread exists, and this is the id every later message needs.
    ThreadStarted(String),
    /// A piece of assistant text to append.
    Delta(String),
    /// The turn finished normally.
    TurnCompleted,
    /// The turn failed; the string is for the user.
    TurnFailed(String),
    /// A turn wants to change files, and has said which and how. Arrives *before* the
    /// approval request, which is why the panel can show a diff when the question lands.
    Proposed { item_id: String, changes: Vec<ProposedChange> },
    /// The CLI is blocked on the user: may it write the files of `item_id`? Nothing is on
    /// disk yet, and nothing will be until [`approval_response`] says `accept`.
    ApprovalRequested { request_id: u64, item_id: String },
    /// A line that is well-formed and deliberately carries nothing: lifecycle chatter,
    /// rate-limit updates, MCP startup noise. Distinct from `None` (unparseable) so a
    /// reader can tell "understood and skipped" from "not JSON at all".
    Ignored,
}

/// Parses one line of app-server output. `None` for anything that is not a JSON object —
/// a stray log line on stdout must never take the stream down.
///
/// Note what is *not* here: `item/started` and `item/completed` carry a whole item, and
/// the first `item/started` of a turn is the echoed **user** message, not the reply. Only
/// `item/agentMessage/delta` is treated as text, so nothing gets appended twice and the
/// user's own words never come back as the assistant's.
pub fn parse_line(text: &str) -> Option<CodexEvent> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;

    // A *server request* carries both `id` and `method` — it is the CLI asking us
    // something and waiting for an answer at that id. It has to be matched before the
    // reply branch below, which would otherwise read the id and treat it as an ack.
    if let (Some(id), Some("item/fileChange/requestApproval")) =
        (value.get("id").and_then(Value::as_u64), value.get("method").and_then(Value::as_str))
    {
        let item_id = value.get("params")?.get("itemId")?.as_str()?;
        return Some(CodexEvent::ApprovalRequested {
            request_id: id,
            item_id: item_id.to_string(),
        });
    }

    // A reply carries `id`; a notification carries `method`. Replies first, because the
    // handshake is the only thing that waits on one.
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the Codex CLI refused the request");
            return Some(CodexEvent::TurnFailed(message.to_string()));
        }
        let result = value.get("result")?;
        return match id {
            INITIALIZE_ID => Some(CodexEvent::Initialized),
            THREAD_START_ID => {
                let thread_id = result.get("thread")?.get("id")?.as_str()?;
                Some(CodexEvent::ThreadStarted(thread_id.to_string()))
            }
            // `turn/start` and `turn/interrupt` acknowledgements: the notifications are
            // what the panel follows, so the acks are noise.
            _ => Some(CodexEvent::Ignored),
        };
    }

    let method = value.get("method").and_then(Value::as_str)?;
    let params = value.get("params");
    match method {
        "item/agentMessage/delta" => {
            // `delta` is a bare String here, not an object — the shape the probe pinned.
            let delta = params?.get("delta")?.as_str()?;
            Some(CodexEvent::Delta(delta.to_string()))
        }
        // The proposal itself. Only `item/started` is read, never `item/completed`: the
        // completed copy repeats the same changes with a final `status`, and acting on
        // both would show every proposed file twice.
        "item/started" => {
            let item = params?.get("item")?;
            if item.get("type")?.as_str()? != "fileChange" {
                return Some(CodexEvent::Ignored);
            }
            let item_id = item.get("id")?.as_str()?.to_string();
            let changes = item
                .get("changes")?
                .as_array()?
                .iter()
                .filter_map(|change| {
                    Some(ProposedChange {
                        path: change.get("path")?.as_str()?.to_string(),
                        kind: change.get("kind")?.get("type")?.as_str()?.to_string(),
                        // A change with no diff is nothing the user could review, so it is
                        // dropped rather than rendered as an empty pane with an Apply
                        // button under it.
                        diff: change.get("diff")?.as_str()?.to_string(),
                    })
                })
                .collect::<Vec<_>>();
            if changes.is_empty() {
                return Some(CodexEvent::Ignored);
            }
            Some(CodexEvent::Proposed { item_id, changes })
        }
        "turn/completed" => Some(CodexEvent::TurnCompleted),
        "turn/failed" => {
            let message = params
                .and_then(|params| params.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("the Codex turn failed");
            Some(CodexEvent::TurnFailed(message.to_string()))
        }
        // Everything else is lifecycle or telemetry: thread/started, turn/started,
        // item/completed, remoteControl/status/changed,
        // mcpServer/startupStatus/updated, account/rateLimits/updated,
        // thread/tokenUsage/updated, thread/status/changed.
        _ => Some(CodexEvent::Ignored),
    }
}

/// Whether this machine can chat through Codex at all, as a sentence the user can act on.
///
/// **Blocking** — it runs `codex --version` — so call it off the main thread, the same
/// rule `resolve_auth` follows.
///
/// The two failures are deliberately distinct: an uninstalled CLI and a logged-out one
/// need different commands, and "AI chat is broken" would be neither.
pub fn availability() -> Result<(), String> {
    let Some(binary) = binary() else {
        return Err("Codex CLI not found — install it and run `codex login`".to_string());
    };
    let installed = std::process::Command::new(&binary)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !installed {
        return Err("Codex CLI not found — install it and run `codex login`".to_string());
    }
    if !auth_path().is_some_and(|path| path.exists()) {
        return Err(
            "Codex is installed but not logged in — run `codex login` in a terminal".to_string()
        );
    }
    Ok(())
}

/// The `codex` executable, looked up the same way language servers are.
///
/// # Why not `Command::new("codex")`
///
/// That is #123 again, in the other feature. `open ellefuanti.app` hands the process
/// **launchd's** environment, whose `PATH` is empty on a normal macOS install, so a bare
/// command name resolves only when the app was started from a terminal. The owner's own
/// machine is the case: `codex` sits in `~/.local/bin`, which their shell puts on `PATH`
/// and the Dock does not. Logged in, CLI installed, and the panel still reported "Codex
/// CLI not found" — from the Dock only.
///
/// `search_dirs` already covers this for language servers and already lists `.local/bin`;
/// the Codex path simply never used it. Sharing the resolver rather than copying the list
/// is also what keeps the two from drifting when the next installer prefix is added.
pub(crate) fn binary() -> Option<std::path::PathBuf> {
    crate::lsp_session::resolve_binary("codex", &crate::lsp_session::search_dirs())
}

/// Where the CLI keeps its login. Only ever asked whether it *exists* — reading it would
/// mean holding a credential this app has no business holding.
fn auth_path() -> Option<std::path::PathBuf> {
    // `CODEX_HOME` is the CLI's own override; honouring it keeps the check truthful for
    // anyone who moved their config, instead of reporting "not logged in" at a stale path.
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Some(Path::new(&home).join("auth.json"));
    }
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".codex").join("auth.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // The fixtures below are real captured lines from `codex-cli 0.146.0`, trimmed only
    // where a field is irrelevant to parsing. They are the contract: if a future CLI
    // changes shape, these fail and say exactly which message moved.

    #[test]
    fn the_handshake_requests_are_one_line_each_with_the_ids_the_replies_use() {
        let init = initialize_request("0.3.0");
        assert!(init.ends_with('\n'), "the newline is the framing");
        assert_eq!(init.matches('\n').count(), 1, "exactly one line per message");
        let value: Value = serde_json::from_str(&init).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], INITIALIZE_ID);
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["clientInfo"]["name"], "ellefuanti");
        assert_eq!(value["params"]["clientInfo"]["version"], "0.3.0");
    }

    #[test]
    fn thread_start_carries_the_project_root_when_there_is_one() {
        let rooted: Value =
            serde_json::from_str(&thread_start_request(Some(&PathBuf::from("/srv/app")))).unwrap();
        assert_eq!(rooted["method"], "thread/start");
        assert_eq!(rooted["id"], THREAD_START_ID);
        assert_eq!(rooted["params"]["cwd"], "/srv/app", "how Codex gets to read the project");

        // No folder open: no cwd, rather than a cwd pointing at wherever we launched.
        let rootless: Value = serde_json::from_str(&thread_start_request(None)).unwrap();
        assert!(rootless["params"].get("cwd").is_none(), "{rootless}");
    }

    #[test]
    fn a_turn_sends_the_thread_id_and_one_text_block() {
        let value: Value =
            serde_json::from_str(&turn_start_request("019ff658-abc", "explain this")).unwrap();
        assert_eq!(value["method"], "turn/start");
        assert_eq!(value["params"]["threadId"], "019ff658-abc");
        let input = value["params"]["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[0]["text"], "explain this");
    }

    #[test]
    fn an_interrupt_names_the_thread_it_cancels() {
        let value: Value = serde_json::from_str(&interrupt_request("019ff658-abc")).unwrap();
        assert_eq!(value["method"], "turn/interrupt");
        assert_eq!(value["params"]["threadId"], "019ff658-abc");
    }

    #[test]
    fn the_initialize_reply_opens_the_handshake() {
        let captured = r#"{"id":1,"result":{"userAgent":"ellefuanti/0.146.0 (Mac OS 26.5.2; arm64)","codexHome":"/Users/x/.codex","platformFamily":"unix","platformOs":"macos"}}"#;
        assert_eq!(parse_line(captured), Some(CodexEvent::Initialized));
    }

    #[test]
    fn the_thread_start_reply_yields_the_id_every_later_message_needs() {
        let captured = r#"{"id":2,"result":{"thread":{"id":"019ff659-c12b-7331-9690-228b71b89440","cwd":"/Users/x/app","status":{"type":"idle"}},"model":"gpt-5.6-luna","approvalPolicy":"on-request","sandbox":{"type":"readOnly","networkAccess":false}}}"#;
        assert_eq!(
            parse_line(captured),
            Some(CodexEvent::ThreadStarted("019ff659-c12b-7331-9690-228b71b89440".to_string()))
        );
    }

    #[test]
    fn a_delta_is_the_text_the_panel_appends() {
        let captured = r#"{"method":"item/agentMessage/delta","params":{"threadId":"019ff65a-131a","turnId":"019ff65a-1354","itemId":"msg_0157b6","delta":"OK"},"emittedAtMs":1786544528866}"#;
        assert_eq!(parse_line(captured), Some(CodexEvent::Delta("OK".to_string())));
    }

    #[test]
    fn a_completed_turn_is_the_signal_to_stop_reading() {
        let captured = r#"{"method":"turn/completed","params":{"threadId":"019ff65a-131a","turn":{"id":"019ff65a-1354","status":"completed","error":null,"durationMs":3032}},"emittedAtMs":1786544529198}"#;
        assert_eq!(parse_line(captured), Some(CodexEvent::TurnCompleted));
    }

    #[test]
    fn a_failed_turn_carries_its_message_to_the_user() {
        let captured = r#"{"method":"turn/failed","params":{"threadId":"019ff65a","error":{"message":"usage limit reached"}}}"#;
        assert_eq!(
            parse_line(captured),
            Some(CodexEvent::TurnFailed("usage limit reached".to_string()))
        );

        // A failure with no message still ends the turn rather than hanging it.
        let bare = r#"{"method":"turn/failed","params":{"threadId":"019ff65a"}}"#;
        assert!(matches!(parse_line(bare), Some(CodexEvent::TurnFailed(_))));
    }

    #[test]
    fn the_echoed_user_message_is_never_mistaken_for_the_reply() {
        // The first `item/started` of every turn is the *user's* own message coming back.
        // Treating item payloads as text would append the question to the answer, and
        // then append the answer twice when `item/completed` repeats it in full.
        let user_item = r#"{"method":"item/started","params":{"item":{"type":"userMessage","id":"019ff65a","content":[{"type":"text","text":"Responda apenas: OK"}]},"threadId":"019ff65a","turnId":"019ff65a"}}"#;
        assert_eq!(parse_line(user_item), Some(CodexEvent::Ignored));

        let agent_started = r#"{"method":"item/started","params":{"item":{"type":"agentMessage","id":"msg_0157b6","text":"","phase":"final_answer"},"threadId":"019ff65a","turnId":"019ff65a"}}"#;
        assert_eq!(
            parse_line(agent_started),
            Some(CodexEvent::Ignored),
            "only a fileChange item/started carries a proposal"
        );

        let agent_completed = r#"{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"msg_0157b6","text":"OK","phase":"final_answer"},"threadId":"019ff65a","turnId":"019ff65a"}}"#;
        assert_eq!(
            parse_line(agent_completed),
            Some(CodexEvent::Ignored),
            "the full text here would double every reply"
        );
    }

    #[test]
    fn lifecycle_and_telemetry_chatter_is_understood_and_skipped() {
        for captured in [
            r#"{"method":"thread/started","params":{"thread":{"id":"019ff65a","cwd":"/tmp"}}}"#,
            r#"{"method":"turn/started","params":{"threadId":"019ff65a","turn":{"id":"019ff65a","status":"inProgress"}}}"#,
            r#"{"method":"remoteControl/status/changed","params":{"status":"disabled"}}"#,
            r#"{"method":"mcpServer/startupStatus/updated","params":{"name":"herd","status":"starting"}}"#,
            r#"{"method":"account/rateLimits/updated","params":{}}"#,
            r#"{"method":"thread/tokenUsage/updated","params":{}}"#,
            r#"{"method":"thread/status/changed","params":{}}"#,
        ] {
            assert_eq!(parse_line(captured), Some(CodexEvent::Ignored), "{captured}");
        }
    }

    // --- agent mode ------------------------------------------------------------------
    //
    // The fixtures below were captured from `codex-cli 0.146.0` running a real turn under
    // `sandbox: read-only`, asked to edit a PHP file. The file on disk was **unchanged**
    // afterwards, because the probe answered `decline` — that run is what these encode.

    #[test]
    fn a_thread_pins_the_sandbox_that_makes_a_write_ask_first() {
        // The load-bearing line of the whole feature. `workspace-write` was measured
        // writing files with no approval request at all, so this must not drift to it.
        let value: Value = serde_json::from_str(&thread_start_request(None)).unwrap();
        assert_eq!(value["params"]["sandbox"], "read-only", "a write must stay an escape");
        assert_eq!(value["params"]["approvalPolicy"], "on-request");
    }

    #[test]
    fn a_file_change_item_carries_the_path_and_the_diff_before_the_question() {
        let captured = r#"{"method":"item/started","params":{"item":{"type":"fileChange","id":"exec-10e21aa1","changes":[{"path":"/tmp/corr/hello.php","kind":{"type":"update","move_path":null},"diff":"@@ -2,3 +2,3 @@\n function hello() {\n-    return 'hi';\n+    return 'hello world';\n }\n"}],"status":"inProgress"},"threadId":"019ff8c2","turnId":"019ff8c2"}}"#;
        let Some(CodexEvent::Proposed { item_id, changes }) = parse_line(captured) else {
            panic!("expected a proposal, got {:?}", parse_line(captured));
        };
        assert_eq!(item_id, "exec-10e21aa1", "the id the approval will name");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "/tmp/corr/hello.php");
        assert_eq!(changes[0].kind, "update");
        assert!(changes[0].diff.starts_with("@@ -2,3 +2,3 @@"), "{}", changes[0].diff);
        assert!(changes[0].diff.contains("+    return 'hello world';"));
    }

    #[test]
    fn the_approval_request_is_a_question_with_an_id_to_answer_at() {
        // It carries `id` *and* `method`: parsed as a server request, not as the ack of
        // some request of ours, which is what the id would otherwise look like.
        let captured = r#"{"method":"item/fileChange/requestApproval","id":0,"params":{"threadId":"019ff8c2","turnId":"019ff8c2","itemId":"exec-10e21aa1","startedAtMs":1786584955278,"reason":null,"grantRoot":null}}"#;
        assert_eq!(
            parse_line(captured),
            Some(CodexEvent::ApprovalRequested {
                request_id: 0,
                item_id: "exec-10e21aa1".to_string(),
            })
        );
    }

    #[test]
    fn an_approval_answers_at_the_requests_id_and_offers_no_blanket_yes() {
        let accept: Value = serde_json::from_str(&approval_response(0, true)).unwrap();
        assert_eq!(accept["id"], 0, "answered at the id that asked");
        assert_eq!(accept["result"]["decision"], "accept");

        let decline: Value = serde_json::from_str(&approval_response(7, false)).unwrap();
        assert_eq!(decline["id"], 7);
        assert_eq!(decline["result"]["decision"], "decline");

        // `acceptForSession` is the "trust this session" hatch the feature deliberately
        // does not build: every file is seen before it is written.
        for id in [0, 7] {
            for approve in [true, false] {
                assert!(
                    !approval_response(id, approve).contains("acceptForSession"),
                    "no blanket approval"
                );
            }
        }
    }

    #[test]
    fn the_completed_copy_of_a_proposal_is_ignored_so_nothing_is_offered_twice() {
        // `item/completed` repeats the whole change with a final status. Acting on it
        // would show every proposed file a second time, under a stale Apply button.
        let captured = r#"{"method":"item/completed","params":{"item":{"type":"fileChange","id":"exec-10e21aa1","changes":[{"path":"/tmp/corr/hello.php","kind":{"type":"update","move_path":null},"diff":"@@ -2,3 +2,3 @@\n x\n"}],"status":"declined"},"threadId":"019ff8c2","turnId":"019ff8c2"}}"#;
        assert_eq!(parse_line(captured), Some(CodexEvent::Ignored));
    }

    #[test]
    fn a_file_change_with_nothing_reviewable_is_not_offered_as_a_proposal() {
        // No `diff` field means nothing to show; an Apply button over an empty pane would
        // be asking the user to approve something they cannot see.
        let no_diff = r#"{"method":"item/started","params":{"item":{"type":"fileChange","id":"exec-1","changes":[{"path":"/tmp/a.php","kind":{"type":"update"}}],"status":"inProgress"}}}"#;
        assert_eq!(parse_line(no_diff), Some(CodexEvent::Ignored));
    }

    #[test]
    fn a_line_that_is_not_json_never_takes_the_stream_down() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("codex: starting app-server"), None);
        assert_eq!(parse_line("{not json"), None);
        assert_eq!(parse_line("[1,2,3]"), None, "an array carries neither id nor method");
    }

    #[test]
    fn a_jsonrpc_error_reply_reaches_the_user_instead_of_hanging_the_handshake() {
        let captured = r#"{"id":2,"error":{"code":-32602,"message":"invalid params"}}"#;
        assert_eq!(
            parse_line(captured),
            Some(CodexEvent::TurnFailed("invalid params".to_string()))
        );
    }
}

#[cfg(test)]
mod binary_resolution_tests {
    /// The Finder-launch case, which is how the owner hit it: logged in, CLI installed,
    /// and the panel still reported "Codex CLI not found".
    ///
    /// `open ellefuanti.app` inherits launchd's environment, whose `PATH` is empty on a
    /// normal macOS install, so `Command::new("codex")` resolves nothing — while the
    /// binary sits in `~/.local/bin`, one of the prefixes `search_dirs` already searches
    /// for language servers (#123). The LSP was fixed for this; the Codex path still used
    /// a bare name.
    ///
    /// `resolve_binary` is called with explicit directories rather than through the
    /// environment: this suite shares a process, and `set_var` would make it
    /// order-dependent — the rule `the_default_command_is_used_when_the_variable_is_unset`
    /// states in `lsp_session`.
    #[test]
    fn a_binary_is_found_in_a_fallback_directory_that_path_would_not_have() {
        let dir = std::env::temp_dir().join(format!("elle-codex-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let fake = dir.join("codex-probe");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("write probe");

        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        // The empty slice is the Finder launch: no PATH entries at all.
        assert_eq!(
            crate::lsp_session::resolve_binary("codex-probe", &[]),
            None,
            "with no directories to search there is nothing to find — the old behaviour"
        );

        // With the fallback directory supplied, exactly as `search_dirs` supplies it.
        assert_eq!(
            crate::lsp_session::resolve_binary("codex-probe", &[dir.clone()]),
            Some(fake.clone()),
            "the resolver must find a CLI that PATH does not mention"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `search_dirs` must actually include the prefix the owner's CLI lives under,
    /// otherwise the resolver is correct and still finds nothing on their machine.
    #[test]
    fn the_search_path_covers_local_bin_where_codex_installs() {
        let dirs = crate::lsp_session::search_dirs();
        assert!(
            dirs.iter().any(|dir| dir.ends_with(".local/bin")),
            "~/.local/bin is where `codex` installs; it must be searched: {dirs:?}"
        );
    }
}
