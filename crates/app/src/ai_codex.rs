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
/// defaults. They are the settings that decide whether a write can happen without being
/// asked about (see the module docs), so they are stated here where they can be read,
/// not inherited from whatever a future CLI version or a user's `~/.codex/config.toml`
/// happens to default to.
pub fn thread_start_request(cwd: Option<&Path>) -> String {
    let mut params = json!({
        // The thread's *base* is read-only: a turn that says nothing about its sandbox
        // cannot write. Each `turn/start` then overrides per mode
        // ([`turn_start_request`]) — Agent turns get `workspaceWrite`, Ask turns restate
        // `readOnly` — so the write permission is granted turn by turn, by the mode the
        // user picked, never inherited.
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
///
/// `write` is the mode, translated to the sandbox the turn runs under — the schema's
/// per-turn `sandboxPolicy` override, probed live against 0.146. Agent turns get
/// `workspaceWrite`: the CLI edits files inside the open project directly, which is its
/// own native flow (measured: 25 s and correct files, against 93 s of read-only flailing,
/// approval round-trips and decline-retries for the same two-file task — the owner's
/// "demora muito e não escreve"). Ask turns stay `readOnly`: a question must not write.
/// Neither variant grants network access — `workspaceWrite`'s `network_access` defaults
/// to false and is left there.
pub fn turn_start_request(thread_id: &str, text: &str, write: bool) -> String {
    line(json!({
        "jsonrpc": "2.0",
        "id": TURN_START_ID,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}],
            "sandboxPolicy": {"type": if write { "workspaceWrite" } else { "readOnly" }},
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

/// What a non-file approval is asking permission for.
///
/// Three cases rather than a bool because they read differently to a user and because
/// `Unknown` must exist: a method this build does not recognise still has to be answerable,
/// or the turn hangs (see [`CodexEvent::ActionApprovalRequested`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalKind {
    /// The turn wants to run a shell command.
    Command,
    /// The turn wants a permission profile granted — this is the MCP-server case.
    Permissions,
    /// An approval method this build does not know about.
    Unknown,
    /// An MCP server asking the user something (`mcpServer/elicitation/request`) — the
    /// "may I read this skill / use this server" prompts. Its answer is a different wire
    /// shape (`action`) from the command decisions (`decision`), which is why the kind
    /// travels with the request: the button click must pick the right encoder.
    McpElicitation,
}

/// How an approval was answered.
///
/// `AcceptForSession` is the CLI's own `acceptForSession`, which it offers for commands:
/// the same command shape stops asking until the session ends. Deliberately *not* a
/// persisted setting — a session-scoped grant dies with the panel, so a blanket allow can
/// never outlive the conversation the user granted it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Accept,
    AcceptForSession,
    Decline,
}

/// The answer to an [`CodexEvent::ActionApprovalRequested`], at the request's own id.
///
/// The wire words come from the CLI's schema (`CommandExecutionApprovalDecision`), not from
/// guessing: `accept`, `acceptForSession`, `decline`.
pub fn action_approval_response(request_id: u64, decision: Decision) -> String {
    let word = match decision {
        Decision::Accept => "accept",
        Decision::AcceptForSession => "acceptForSession",
        Decision::Decline => "decline",
    };
    line(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {"decision": word},
    }))
}

/// The answer to an MCP elicitation, at the request's own id.
///
/// `content: {}` on accept mirrors a bare confirmation; decline carries none (the schema
/// marks it nullable for exactly that). `cancel` is deliberately unused — the panel offers
/// two buttons, and a third state whose semantics differ per server is not worth a third
/// button nobody can predict the effect of.
pub fn elicitation_response(request_id: u64, accept: bool) -> String {
    let result = if accept {
        json!({"action": "accept", "content": {}})
    } else {
        json!({"action": "decline"})
    };
    line(json!({"jsonrpc": "2.0", "id": request_id, "result": result}))
}

/// A JSON-RPC "method not found", for server requests this build cannot serve.
///
/// **An unanswered request is a hang** — the whole lesson of the approval bugs: the CLI
/// blocks on its reply, the panel shows nothing, and the user reports a spinner. The
/// schema lists nine server→client methods and new ones will keep arriving; erroring the
/// unknown ones keeps the turn moving and downgrades "the app froze" to "a feature this
/// build lacks".
pub fn method_not_found_response(request_id: u64) -> String {
    line(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": -32601, "message": "this client does not implement that method"},
    }))
}

/// A one-line description of a command approval, for the panel to show verbatim.
///
/// Falls back through the fields the schema marks optional, and never returns empty: a
/// button with no question above it is worse than a vague question.
fn command_summary(params: Option<&Value>) -> String {
    let Some(params) = params else { return "run a command".to_string() };
    if let Some(command) = params.get("command").and_then(Value::as_str)
        && !command.trim().is_empty()
    {
        return format!("run: {command}");
    }
    // Some shapes carry argv instead of a rendered string.
    if let Some(argv) = params.get("argv").and_then(Value::as_array) {
        let joined: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
        if !joined.is_empty() {
            return format!("run: {}", joined.join(" "));
        }
    }
    "run a command".to_string()
}

/// A one-line description of a permissions approval (the MCP case).
fn permissions_summary(params: Option<&Value>) -> String {
    let Some(params) = params else { return "grant permissions".to_string() };
    let reason = params.get("reason").and_then(Value::as_str).unwrap_or("").trim();
    if !reason.is_empty() {
        return format!("grant permissions: {reason}");
    }
    if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
        return format!("grant permissions in {cwd}");
    }
    "grant permissions".to_string()
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
    /// A `fileChange` item completed with `status: "completed"` — the CLI already wrote
    /// these changes itself (a workspace-write turn). The panel's job is to record them
    /// and re-sync any open tab, not to ask about them: the write has happened.
    Applied { item_id: String, changes: Vec<ProposedChange> },
    /// The CLI is blocked on the user: may it write the files of `item_id`? Nothing is on
    /// disk yet, and nothing will be until [`approval_response`] says `accept`.
    ApprovalRequested { request_id: u64, item_id: String },
    /// The CLI is blocked on the user for something that is **not** a file write: a command
    /// it wants to run, or a permission profile (an MCP server) it wants granted.
    ///
    /// # Why this variant exists
    ///
    /// It did not, and that was the "loop infinito": `codex app-server`'s own schema
    /// (`codex app-server generate-json-schema`) declares *three* approval methods —
    /// `item/fileChange/requestApproval`, `item/commandExecution/requestApproval` and
    /// `item/permissions/requestApproval` — and this parser matched only the first. The
    /// other two fell into the `_ => Ignored` arm, so the panel drew nothing, the user had
    /// no button to press, and the CLI waited forever for an answer that could not be given.
    ///
    /// Carried as a `summary` the panel can render verbatim rather than a typed shape per
    /// method: what the user needs is the *sentence* ("run `composer install`", "grant
    /// filesystem access to server X") and two buttons. Parsing every optional field of two
    /// evolving schemas to rebuild that sentence would be a second source of truth that
    /// silently rots when the CLI changes.
    ActionApprovalRequested {
        request_id: u64,
        kind: ApprovalKind,
        /// What is being asked, in the user's words. Never empty — falls back to the method.
        summary: String,
    },
    /// The turn started doing something visible: thinking, running a command, calling an
    /// MCP tool. The label is ready for the UI — built here so every consumer says the
    /// same words for the same act.
    Activity(String),
    /// The item that was running finished. The panel marks its activity rows done.
    ActivityEnded,
    /// A server request this build cannot serve at all. The reader must answer it with
    /// [`method_not_found_response`] — an unanswered id is a hang — and may tell the user.
    UnservableRequest { request_id: u64, method: String },
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
/// The `changes` array of a `fileChange` item, shared by `item/started` (a proposal)
/// and `item/completed` (a write the CLI already made).
fn parse_changes(item: &Value) -> Vec<ProposedChange> {
    item.get("changes")
        .and_then(Value::as_array)
        .map(|changes| {
            changes
                .iter()
                .filter_map(|change| {
                    Some(ProposedChange {
                        path: change.get("path")?.as_str()?.to_string(),
                        kind: change.get("kind")?.get("type")?.as_str()?.to_string(),
                        // A change with no diff is nothing the user could review, so it
                        // is dropped rather than rendered as an empty pane with an Apply
                        // button under it.
                        diff: change.get("diff")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_line(text: &str) -> Option<CodexEvent> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;

    // A *server request* carries both `id` and `method` — it is the CLI asking us
    // something and waiting for an answer at that id. It has to be matched before the
    // reply branch below, which would otherwise read the id and treat it as an ack.
    if let (Some(id), Some(method)) =
        (value.get("id").and_then(Value::as_u64), value.get("method").and_then(Value::as_str))
    {
        match method {
            "item/fileChange/requestApproval" => {
                let item_id = value.get("params")?.get("itemId")?.as_str()?;
                return Some(CodexEvent::ApprovalRequested {
                    request_id: id,
                    item_id: item_id.to_string(),
                });
            }
            "item/commandExecution/requestApproval" => {
                return Some(CodexEvent::ActionApprovalRequested {
                    request_id: id,
                    kind: ApprovalKind::Command,
                    summary: command_summary(value.get("params")),
                });
            }
            "item/permissions/requestApproval" => {
                return Some(CodexEvent::ActionApprovalRequested {
                    request_id: id,
                    kind: ApprovalKind::Permissions,
                    summary: permissions_summary(value.get("params")),
                });
            }
            // MCP servers asking the user (`mode: confirm` skills prompts and friends).
            // The message is the server's own sentence; shown verbatim like the others.
            "mcpServer/elicitation/request" => {
                let message = value
                    .get("params")
                    .and_then(|params| params.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("an MCP server is asking for confirmation");
                return Some(CodexEvent::ActionApprovalRequested {
                    request_id: id,
                    kind: ApprovalKind::McpElicitation,
                    summary: message.to_string(),
                });
            }
            // An approval method this build does not know. **Not `Ignored`** — an
            // unanswered request is a turn that hangs forever, which is the bug this whole
            // branch exists to stop. Surfaced with the method name so the user can decline
            // and see what was asked, and so the next such method is a visible gap rather
            // than a freeze.
            other if other.ends_with("/requestApproval") => {
                return Some(CodexEvent::ActionApprovalRequested {
                    request_id: id,
                    kind: ApprovalKind::Unknown,
                    summary: format!("an approval this version does not recognise ({other})"),
                });
            }
            // Any other server *request* — `item/tool/call`, `item/tool/requestUserInput`,
            // `attestation/generate`, whatever ships next — cannot go to `Ignored`: it has
            // an id, the CLI is blocked on its answer, and silence is the freeze the owner
            // kept reporting. The reader answers it with a JSON-RPC error so the turn
            // moves, and the event carries the method so the transcript can say what was
            // declined instead of hiding it.
            other => {
                return Some(CodexEvent::UnservableRequest {
                    request_id: id,
                    method: other.to_string(),
                });
            }
        }
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
            let kind = item.get("type")?.as_str()?;
            // Every visible act becomes a labelled activity (the owner's report: the
            // panel "fica pensando e tal, mas não tem nenhuma indicação"). The labels
            // come from the CLI's own item types — nine in the schema — so the panel
            // narrates what is actually happening rather than a generic spinner.
            match kind {
                "reasoning" => return Some(CodexEvent::Activity("Thinking…".to_string())),
                "agentMessage" => return Some(CodexEvent::Activity("Writing…".to_string())),
                "webSearch" => {
                    return Some(CodexEvent::Activity("Searching the web…".to_string()));
                }
                "imageGeneration" => {
                    return Some(CodexEvent::Activity("Generating an image…".to_string()));
                }
                "commandExecution" => {
                    let command = item.get("command").and_then(Value::as_str).unwrap_or("");
                    return Some(CodexEvent::Activity(if command.is_empty() {
                        "Running a command…".to_string()
                    } else {
                        format!("Running: {command}")
                    }));
                }
                "mcpToolCall" => {
                    let server = item.get("server").and_then(Value::as_str).unwrap_or("");
                    let tool = item.get("tool").and_then(Value::as_str).unwrap_or("");
                    return Some(CodexEvent::Activity(match (server, tool) {
                        ("", "") => "Using an MCP tool…".to_string(),
                        (server, "") => format!("Using {server}…"),
                        (server, tool) => format!("Using {server}: {tool}"),
                    }));
                }
                "fileChange" => {}
                // `userMessage` is the echo of what was typed; `error` arrives again as
                // `turn/failed` with the message intact.
                _ => return Some(CodexEvent::Ignored),
            }
            let item_id = item.get("id")?.as_str()?.to_string();
            let changes = parse_changes(item);
            if changes.is_empty() {
                return Some(CodexEvent::Ignored);
            }
            Some(CodexEvent::Proposed { item_id, changes })
        }
        // A completed `fileChange` is the CLI reporting bytes it wrote itself — the
        // workspace-write path, where no approval gates the edit. It carries the same
        // `changes` array as the proposal, plus a `status`; only `"completed"` counts,
        // a failed item wrote nothing worth recording. Everything else completing is
        // just the end of an activity.
        "item/completed" => {
            let applied = (|| {
                let item = params?.get("item")?;
                if item.get("type")?.as_str()? != "fileChange" {
                    return None;
                }
                if item.get("status").and_then(Value::as_str) != Some("completed") {
                    return None;
                }
                let item_id = item.get("id")?.as_str()?.to_string();
                let changes = parse_changes(item);
                if changes.is_empty() {
                    return None;
                }
                Some(CodexEvent::Applied { item_id, changes })
            })();
            Some(applied.unwrap_or(CodexEvent::ActivityEnded))
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
    match status() {
        Availability::Ready => Ok(()),
        other => Err(other.message()),
    }
}

/// Why Codex can or cannot answer, as something the caller can *branch on*.
///
/// # Why not just the sentence
///
/// `availability` returns `Result<(), String>`, so every caller could show the problem and
/// none could act on it. The two failures need different offers: a missing CLI needs an
/// install link, a logged-out one needs a **button that logs in** — and telling them apart
/// from prose means matching on error text, which is how a message reword silently turns a
/// button off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// Installed and logged in.
    Ready,
    /// No `codex` binary anywhere this app looks.
    NotInstalled,
    /// The CLI is there; nobody is signed in. **This one is actionable in-app.**
    NotLoggedIn,
}

impl Availability {
    /// The sentence to show. Names the exact command, because the button is a convenience
    /// and a terminal is still the answer when it fails.
    pub fn message(&self) -> String {
        match self {
            Availability::Ready => String::new(),
            Availability::NotInstalled => {
                "Codex CLI not found — install it and run `codex login`".to_string()
            }
            Availability::NotLoggedIn => {
                "Codex is installed but not logged in — run `codex login`".to_string()
            }
        }
    }
}

/// Whether this machine can chat through Codex, and why not when it cannot.
///
/// **Blocking** — it runs the CLI — so call it off the main thread.
///
/// Login is decided by `codex login status` rather than by the presence of
/// `~/.codex/auth.json`: an expired or revoked session leaves that file exactly where it
/// was, so the file test reported "logged in" for an account that could no longer answer.
/// Asking the CLI is asking the thing that actually knows, and it still tells this app
/// nothing about the credential itself — only "Logged in" or "Not logged in".
pub fn status() -> Availability {
    let Some(binary) = binary() else { return Availability::NotInstalled };
    let installed = std::process::Command::new(&binary)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !installed {
        return Availability::NotInstalled;
    }

    let logged_in = std::process::Command::new(&binary)
        .arg("login")
        .arg("status")
        .output()
        .map(|output| {
            // The **exit code**, measured against `codex-cli 0.146.0`: 0 when signed in, 1
            // when not. Both write their sentence to **stderr** and leave stdout empty, so
            // a first version of this that matched "not logged in" in stdout was reading a
            // stream that is always blank — it happened to return the right answer for the
            // logged-in case and would have reported a logged-out user as ready.
            //
            // The text is still consulted, but only as a tiebreak for a future CLI that
            // stops using the exit code: an unrecognised phrasing reads as logged in and
            // lets the turn try, rather than hiding the panel behind a login button from
            // someone who is already signed in.
            let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
            if stderr.contains("not logged in") {
                false
            } else {
                output.status.success() || stderr.contains("logged in")
            }
        })
        // The CLI failing to answer is not proof of being logged out; fall back to the file,
        // which is what this checked before `login status` existed here.
        .unwrap_or_else(|_| auth_path().is_some_and(|path| path.exists()));

    if logged_in { Availability::Ready } else { Availability::NotLoggedIn }
}

/// Runs `codex login`, which opens the browser for OpenAI's own sign-in.
///
/// **This app never sees a credential.** The CLI owns the OAuth flow end to end and writes
/// `~/.codex/auth.json` itself; all this does is start it, which is the difference between
/// "the editor logs you in" and "the editor saves you a trip to the terminal". Nothing here
/// reads a password, a token or that file — the same rule the module docs state.
///
/// Spawned rather than waited on: the flow ends in a browser the user has to interact with,
/// so blocking on the child would freeze whatever called it for as long as they take. The
/// caller polls [`status`] to learn when it finished.
///
/// **Blocking** only for the spawn itself.
pub fn begin_login() -> Result<std::process::Child, String> {
    let Some(binary) = binary() else { return Err(Availability::NotInstalled.message()) };
    std::process::Command::new(&binary)
        .arg("login")
        // The child's own output is not the user interface — the browser is, and `status`
        // is how completion is detected. Left on null so a chatty CLI cannot block on a
        // pipe nobody reads.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|err| format!("could not start `codex login`: {err}"))
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

/// Runs `codex logout`, which removes the stored credential.
///
/// # Why this exists
///
/// The panel grew a sign-in button and nothing to undo it — a door that only opened
/// inwards, reported by the owner as exactly that. Signing out is the CLI's own
/// `codex logout` (probed against a scratch `CODEX_HOME`: it deletes `auth.json` and says
/// "Successfully logged out"), so the same boundary holds in both directions: this app
/// never touches the credential, not to write it and not to remove it.
///
/// Synchronous, unlike [`begin_login`]: there is no browser and no user step to wait on,
/// so the caller can just be told whether it worked. **Blocking** — run it off the main
/// thread.
pub fn sign_out() -> Result<(), String> {
    let Some(binary) = binary() else { return Err(Availability::NotInstalled.message()) };
    let output = std::process::Command::new(&binary)
        .arg("logout")
        .output()
        .map_err(|err| format!("could not run `codex logout`: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        // The CLI's own words, because "sign-out failed" with no reason is unactionable.
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
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
            serde_json::from_str(&turn_start_request("019ff658-abc", "explain this", false))
                .unwrap();
        assert_eq!(value["method"], "turn/start");
        assert_eq!(value["params"]["threadId"], "019ff658-abc");
        let input = value["params"]["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[0]["text"], "explain this");
    }

    /// The mode decides the turn's sandbox: Agent writes inside the workspace, Ask
    /// restates read-only. Stated per turn so the permission is never inherited — and
    /// the write variant must never open the network.
    #[test]
    fn the_turn_sandbox_follows_the_mode() {
        let agent: Value = serde_json::from_str(&turn_start_request("t", "go", true)).unwrap();
        assert_eq!(agent["params"]["sandboxPolicy"]["type"], "workspaceWrite");
        assert!(
            agent["params"]["sandboxPolicy"].get("networkAccess").is_none(),
            "network stays at the CLI's default of none: {agent}"
        );

        let ask: Value = serde_json::from_str(&turn_start_request("t", "what", false)).unwrap();
        assert_eq!(ask["params"]["sandboxPolicy"]["type"], "readOnly");
    }

    /// The captured wire shape of a write the CLI made itself (workspace-write turn):
    /// `item/completed` with `status: "completed"` and the same `changes` array a
    /// proposal carries. It must parse as `Applied`, and anything less — a failed item,
    /// another item type — stays a plain activity ending.
    #[test]
    fn a_completed_file_change_reports_what_was_written() {
        let captured = r#"{"method":"item/completed","params":{"item":{"type":"fileChange","id":"exec-90587fda","changes":[{"path":"/Users/u/filminho/hello.php","kind":{"type":"update","move_path":null},"diff":"@@ -2,3 +2,3 @@\n function hello() {\n-    return 'turnwrite';\n+    return 'shape';\n }\n"}],"status":"completed"},"threadId":"01a00343","turnId":"01a00343-3260"}}"#;
        let Some(CodexEvent::Applied { item_id, changes }) = parse_line(captured) else {
            panic!("expected Applied, got {:?}", parse_line(captured));
        };
        assert_eq!(item_id, "exec-90587fda");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "/Users/u/filminho/hello.php");
        assert_eq!(changes[0].kind, "update");
        assert!(changes[0].diff.contains("+    return 'shape';"));

        let failed = r#"{"method":"item/completed","params":{"item":{"type":"fileChange","id":"x","changes":[{"path":"/a","kind":{"type":"update"},"diff":"d"}],"status":"failed"}}}"#;
        assert_eq!(
            parse_line(failed),
            Some(CodexEvent::ActivityEnded),
            "a failed write wrote nothing"
        );
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
        // Once Ignored; now the activity label — but the guarantee this test pins is
        // unchanged: an item *start* must never be treated as reply text. The words
        // arrive only through `item/agentMessage/delta`.
        assert_eq!(
            parse_line(agent_started),
            Some(CodexEvent::Activity("Writing…".to_string())),
            "an agentMessage start is a status, never text"
        );

        let agent_completed = r#"{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"msg_0157b6","text":"OK","phase":"final_answer"},"threadId":"019ff65a","turnId":"019ff65a"}}"#;
        // ActivityEnded now, and the original point stands verbatim: the completed item
        // repeats the whole reply in `text`, and treating it as text would double it.
        assert_eq!(
            parse_line(agent_completed),
            Some(CodexEvent::ActivityEnded),
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
        // Once Ignored; now it checks activities off — but the guarantee this test pins
        // is unchanged: a completed fileChange must never come out as `Proposed`, or every
        // file would be offered a second time under a stale Apply button.
        assert_eq!(parse_line(captured), Some(CodexEvent::ActivityEnded));
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
            crate::lsp_session::resolve_binary("codex-probe", std::slice::from_ref(&dir)),
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

#[cfg(test)]
mod approval_kind_tests {
    use super::*;

    /// The bug: `codex app-server` declares **three** approval methods and this parser
    /// matched one. The other two fell into `_ => Ignored`, so nothing rendered, no button
    /// existed, and the CLI waited forever — the owner's "loop infinito" when the model
    /// wanted to run a command or an MCP server asked for permissions.
    ///
    /// The method names are not guessed: they come from `codex app-server
    /// generate-json-schema`, which lists `item/fileChange/requestApproval`,
    /// `item/commandExecution/requestApproval` and `item/permissions/requestApproval`.
    #[test]
    fn every_approval_method_the_cli_can_send_is_answerable() {
        let command = r#"{"jsonrpc":"2.0","id":7,"method":"item/commandExecution/requestApproval","params":{"itemId":"exec-1","threadId":"t","turnId":"u","startedAtMs":0,"command":"composer install"}}"#;
        match parse_line(command) {
            Some(CodexEvent::ActionApprovalRequested { request_id, kind, summary }) => {
                assert_eq!(request_id, 7);
                assert_eq!(kind, ApprovalKind::Command);
                assert!(summary.contains("composer install"), "the user must see it: {summary}");
            }
            other => panic!("a command approval must be answerable, got {other:?}"),
        }

        let permissions = r#"{"jsonrpc":"2.0","id":9,"method":"item/permissions/requestApproval","params":{"itemId":"perm-1","threadId":"t","turnId":"u","startedAtMs":0,"cwd":"/srv/app","permissions":{},"reason":"filesystem access for the docs server"}}"#;
        match parse_line(permissions) {
            Some(CodexEvent::ActionApprovalRequested { request_id, kind, summary }) => {
                assert_eq!(request_id, 9);
                assert_eq!(kind, ApprovalKind::Permissions);
                assert!(
                    summary.contains("docs server"),
                    "the reason must reach the user: {summary}"
                );
            }
            other => panic!("a permissions approval must be answerable, got {other:?}"),
        }
    }

    /// An approval method added by a future CLI must still be *answerable*, not ignored.
    ///
    /// This is the rule the original bug broke. Ignoring an unknown `/requestApproval` is
    /// indistinguishable, from the user's chair, from the app freezing: the CLI blocks on a
    /// reply that no code path can send. Declining an unknown request loses a feature;
    /// ignoring it loses the session.
    #[test]
    fn an_unknown_approval_method_is_surfaced_rather_than_ignored() {
        let future = r#"{"jsonrpc":"2.0","id":11,"method":"item/somethingNew/requestApproval","params":{"itemId":"x"}}"#;
        match parse_line(future) {
            Some(CodexEvent::ActionApprovalRequested { request_id, kind, summary }) => {
                assert_eq!(request_id, 11);
                assert_eq!(kind, ApprovalKind::Unknown);
                assert!(summary.contains("somethingNew"), "name what was asked: {summary}");
            }
            other => panic!("an unknown approval must not be ignored, got {other:?}"),
        }
    }

    /// The file-change path must keep working exactly as before — it is the one that
    /// already had a diff and buttons.
    #[test]
    fn the_file_change_approval_still_parses() {
        let file = r#"{"jsonrpc":"2.0","id":0,"method":"item/fileChange/requestApproval","params":{"itemId":"exec-10e2"}}"#;
        assert_eq!(
            parse_line(file),
            Some(CodexEvent::ApprovalRequested { request_id: 0, item_id: "exec-10e2".to_string() })
        );
    }

    /// The MCP prompt path — "may I read this skill / use this server". Its answer is a
    /// different wire shape (`action`, not `decision`), so the kind must survive parsing.
    #[test]
    fn an_mcp_elicitation_is_answerable_with_its_own_shape() {
        let line = r#"{"jsonrpc":"2.0","id":21,"method":"mcpServer/elicitation/request","params":{"message":"Allow the docs server to read your skills?","mode":"confirm","requestedSchema":{}}}"#;
        match parse_line(line) {
            Some(CodexEvent::ActionApprovalRequested { request_id, kind, summary }) => {
                assert_eq!(request_id, 21);
                assert_eq!(kind, ApprovalKind::McpElicitation);
                assert!(summary.contains("docs server"), "the server's sentence: {summary}");
            }
            other => panic!("an elicitation must reach the user, got {other:?}"),
        }
        assert!(elicitation_response(21, true).contains(r#""action":"accept""#));
        assert!(elicitation_response(21, false).contains(r#""action":"decline""#));
    }

    /// Every *other* server request must come out answerable too — an id with no reply is
    /// a hang, which is the bug class behind every "loop infinito" this panel has had.
    /// `item/tool/call` stands in for the lot (the schema lists nine and counting).
    #[test]
    fn any_unknown_server_request_is_surfaced_for_an_error_reply() {
        let line = r#"{"jsonrpc":"2.0","id":33,"method":"item/tool/call","params":{}}"#;
        match parse_line(line) {
            Some(CodexEvent::UnservableRequest { request_id, method }) => {
                assert_eq!(request_id, 33);
                assert_eq!(method, "item/tool/call");
            }
            other => panic!("an unservable request must not be ignored, got {other:?}"),
        }
        let reply = method_not_found_response(33);
        assert!(reply.contains("-32601"), "the JSON-RPC error code: {reply}");
        assert!(reply.contains(r#""id":33"#), "addressed to the blocked id: {reply}");
    }

    /// The decision words are the CLI's, from `CommandExecutionApprovalDecision`.
    #[test]
    fn the_decision_words_match_the_cli_schema() {
        assert!(action_approval_response(1, Decision::Accept).contains(r#""decision":"accept""#));
        assert!(
            action_approval_response(1, Decision::AcceptForSession)
                .contains(r#""decision":"acceptForSession""#)
        );
        assert!(action_approval_response(1, Decision::Decline).contains(r#""decision":"decline""#));
    }
}

#[cfg(test)]
mod login_boundary_tests {
    /// The credential boundary, checked mechanically.
    ///
    /// Adding a "Sign in to Codex" button is the moment this rule is easiest to break: the
    /// obvious next step for anyone extending it is to collect an email and a password in
    /// the panel and hand them over, or to read `auth.json` to show *who* is signed in.
    /// Both would make this editor a credential holder, which the module docs say it must
    /// never be — and which is exactly what the owner objected to before the button existed.
    ///
    /// So the rule is a test rather than a paragraph: this module may check that the auth
    /// file *exists*, and may never read it.
    #[test]
    fn the_auth_file_is_never_read() {
        let source = include_str!("ai_codex.rs");
        let source = source.split("mod login_boundary_tests").next().unwrap_or(source);

        for forbidden in
            ["read_to_string", "fs::read", "File::open", "read_link", "with_api_key", "password"]
        {
            let hits: Vec<_> = source
                .lines()
                .enumerate()
                .filter(|(_, line)| {
                    let code = line.split("//").next().unwrap_or("");
                    code.contains(forbidden)
                })
                .map(|(i, line)| format!("  {}: {}", i + 1, line.trim()))
                .collect();

            assert!(
                hits.is_empty(),
                "this module must never read a credential — `codex login` owns the flow and \
                 writes ~/.codex/auth.json, and all this app may ask is whether it exists. \
                 Found `{forbidden}`:\n{}",
                hits.join("\n")
            );
        }
    }

    /// `begin_login` must run the CLI's own flow and nothing else — no flags that would
    /// route a secret through this process.
    ///
    /// `--with-api-key` and `--with-access-token` read a credential from **stdin**, which
    /// would mean this app holding one to pipe it. Using them would be the easy way to
    /// "improve" the button and the exact thing that must not happen.
    #[test]
    fn the_login_command_takes_no_credential_flags() {
        let source = include_str!("ai_codex.rs");
        let begin = source
            .split("pub fn begin_login")
            .nth(1)
            .expect("begin_login must exist")
            .split("\n}")
            .next()
            .expect("a function body");

        assert!(begin.contains(r#".arg("login")"#), "it must run `codex login`");
        assert!(
            !begin.contains("with-api-key") && !begin.contains("with-access-token"),
            "no flag may route a credential through this process: {begin}"
        );
        assert!(
            begin.contains("Stdio::null()"),
            "stdin stays closed, so nothing can be piped into the login: {begin}"
        );
    }
}
