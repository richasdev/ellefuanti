//! The wire between the editor and a plugin process.
//!
//! Newline-delimited JSON-RPC 2.0 over stdin/stdout: one message per line, the newline is
//! the framing. The same shape `crates/app/src/ai_codex.rs` speaks to the Codex CLI, and
//! deliberately not the `Content-Length` framing `crates/lsp` uses — LSP's header framing
//! exists because the protocol predates the convention, and a new protocol has no reason
//! to inherit the parsing cost.
//!
//! Everything here is a pure function over strings. No process, no executor, no gpui — so
//! the whole contract is testable without spawning a plugin, and the fixtures below double
//! as the specification a plugin author reads.
//!
//! ```text
//! → initialize      {"apiVersion":1,"hostVersion":"0.4.0"}
//! ← {"id":1,"result":{"ok":true}}
//! → command/invoke  {"id":"sort.lines"}
//! ← {"id":2,"result":{}}                     // ran, nothing to say
//! ← {"id":2,"error":{"message":"no buffer"}} // ran, failed, and says why
//! ```

use serde_json::{Value, json};

/// The handshake's id. Fixed rather than counted for the same reason `ai_codex` fixes
/// its own: nothing here ever has two initializes in flight.
pub const INITIALIZE_ID: u64 = 1;

/// The first id used for a command invocation. Invocations are counted from here, because
/// a user can run several commands before any of them answers — unlike the handshake,
/// these genuinely need distinguishing.
pub const FIRST_INVOKE_ID: u64 = 2;

/// The handshake. Sends the version the *host* implements, so a plugin that supports
/// several can pick, and names the host honestly rather than impersonating an editor.
///
/// The manifest already declared the plugin's version and [`crate::manifest::parse`]
/// refused it if unsupported, so this is a statement rather than a negotiation. It is sent
/// anyway: a plugin whose manifest drifted from its binary finds out from the wire.
pub fn initialize_request(api_version: u32, host_version: &str) -> String {
    line(json!({
        "jsonrpc": "2.0",
        "id": INITIALIZE_ID,
        "method": "initialize",
        "params": {
            "apiVersion": api_version,
            "hostInfo": {"name": "ellefuanti", "version": host_version},
        },
    }))
}

/// Runs one of the plugin's commands, by the id its manifest declared.
///
/// The id is the whole payload. That is the point of #28's observation that every action
/// already has a stable dotted id — the plugin binds to the id, and the host does not need
/// to describe what the command *is*.
pub fn invoke_request(request_id: u64, command_id: &str) -> String {
    line(json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "command/invoke",
        "params": {"id": command_id},
    }))
}

/// The polite shutdown. A plugin that ignores it gets killed — see the host's `shutdown`,
/// which is why this returns a message rather than waiting for anything.
pub fn shutdown_notification() -> String {
    line(json!({"jsonrpc": "2.0", "method": "shutdown"}))
}

/// Serialises one JSON-RPC message as exactly one line: the framing *is* the newline.
fn line(value: Value) -> String {
    format!("{value}\n")
}

/// One parsed line of a plugin's output, reduced to what the host acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginEvent {
    /// The plugin answered the handshake and is ready for commands.
    Initialized,
    /// A command finished. `message` is an optional line for the status bar — a command
    /// that simply worked says nothing, which is the quiet default a good command wants.
    CommandFinished { request_id: u64, message: Option<String> },
    /// A command failed, and this is the reason the user sees.
    CommandFailed { request_id: u64, message: String },
    /// The plugin wants to tell the user something outside of any command.
    Log(String),
    /// Well-formed and deliberately carrying nothing. Distinct from `None` (unparseable)
    /// so a reader can tell "understood and skipped" from "not JSON at all".
    Ignored,
}

/// Parses one line of plugin output.
///
/// `None` for anything that is not a JSON object. A plugin that prints a stray log line to
/// stdout — a `print` left in during development is the likeliest bug in any plugin — must
/// not take the stream down with it (§24).
pub fn parse_line(text: &str) -> Option<PluginEvent> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;

    // A reply carries `id`; a notification carries `method`. Replies first, because the
    // handshake and every invocation are waiting on one.
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the plugin refused the request");
            // A failed handshake is reported as a failure of the request that asked, so the
            // host tears the plugin down instead of waiting forever for `Initialized`.
            return Some(PluginEvent::CommandFailed {
                request_id: id,
                message: message.to_string(),
            });
        }

        let result = value.get("result")?;
        if id == INITIALIZE_ID {
            return Some(PluginEvent::Initialized);
        }
        // `message` is optional by design: most commands do their work and say nothing.
        let message = result.get("message").and_then(Value::as_str).map(str::to_string);
        return Some(PluginEvent::CommandFinished { request_id: id, message });
    }

    let method = value.get("method").and_then(Value::as_str)?;
    match method {
        "log" => {
            let message = value.get("params")?.get("message")?.as_str()?;
            Some(PluginEvent::Log(message.to_string()))
        }
        // Anything else is a notification this version does not implement. Ignored rather
        // than refused: a plugin written against a later API may chatter about panels or
        // completions, and the honest answer is to not act on it, not to crash.
        _ => Some(PluginEvent::Ignored),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_is_exactly_one_line_because_the_newline_is_the_framing() {
        for message in [
            initialize_request(1, "0.4.0"),
            invoke_request(FIRST_INVOKE_ID, "sort.lines"),
            shutdown_notification(),
        ] {
            assert!(message.ends_with('\n'), "{message:?}");
            assert_eq!(message.matches('\n').count(), 1, "{message:?}");
        }
    }

    #[test]
    fn the_handshake_states_the_api_version_the_host_implements() {
        let value: Value = serde_json::from_str(&initialize_request(1, "0.4.0")).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], INITIALIZE_ID);
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["apiVersion"], 1);
        assert_eq!(value["params"]["hostInfo"]["name"], "ellefuanti");
        assert_eq!(value["params"]["hostInfo"]["version"], "0.4.0");
    }

    #[test]
    fn an_invocation_carries_the_command_id_and_nothing_else() {
        let value: Value = serde_json::from_str(&invoke_request(7, "sort.lines")).unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "command/invoke");
        assert_eq!(value["params"]["id"], "sort.lines");
    }

    #[test]
    fn the_initialize_reply_opens_the_gate_for_commands() {
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#),
            Some(PluginEvent::Initialized)
        );
    }

    #[test]
    fn a_command_that_simply_worked_says_nothing() {
        // The quiet default: no `message` means no status-bar noise for a command that did
        // exactly what its title promised.
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#),
            Some(PluginEvent::CommandFinished { request_id: 2, message: None })
        );
    }

    #[test]
    fn a_command_may_answer_with_a_line_for_the_user() {
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":3,"result":{"message":"sorted 12 lines"}}"#),
            Some(PluginEvent::CommandFinished {
                request_id: 3,
                message: Some("sorted 12 lines".into()),
            })
        );
    }

    #[test]
    fn a_failure_carries_its_reason_to_the_user() {
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"no buffer"}}"#),
            Some(PluginEvent::CommandFailed { request_id: 4, message: "no buffer".into() })
        );

        // A failure with no message still ends the request rather than hanging it.
        let bare = r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32000}}"#;
        assert!(matches!(parse_line(bare), Some(PluginEvent::CommandFailed { .. })));
    }

    #[test]
    fn a_refused_handshake_fails_the_request_rather_than_waiting_forever() {
        // Reported against INITIALIZE_ID so the host can tear the plugin down instead of
        // sitting on a handshake that will never complete.
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":1,"error":{"message":"unsupported host"}}"#),
            Some(PluginEvent::CommandFailed {
                request_id: INITIALIZE_ID,
                message: "unsupported host".into(),
            })
        );
    }

    #[test]
    fn a_plugin_can_say_something_outside_any_command() {
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","method":"log","params":{"message":"warming up"}}"#),
            Some(PluginEvent::Log("warming up".into()))
        );
    }

    #[test]
    fn a_notification_this_version_does_not_implement_is_understood_and_skipped() {
        // A plugin written against a later API may talk about panels or completions.
        // Ignoring it is the honest answer; crashing on it would make the API unversionable.
        for captured in [
            r#"{"jsonrpc":"2.0","method":"panel/register","params":{"id":"sort.panel"}}"#,
            r#"{"jsonrpc":"2.0","method":"completion/provide","params":{}}"#,
        ] {
            assert_eq!(parse_line(captured), Some(PluginEvent::Ignored), "{captured}");
        }
    }

    #[test]
    fn a_line_that_is_not_json_never_takes_the_stream_down() {
        // The likeliest bug in any plugin: a debug `print` left on stdout.
        assert_eq!(parse_line("starting up..."), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("{not json"), None);
        assert_eq!(parse_line("[1,2,3]"), None, "an array carries neither id nor method");
    }
}
