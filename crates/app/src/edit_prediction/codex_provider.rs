//! Experimental: ghost-text completions through the Codex CLI (a subscription, no key).
//!
//! # Why this exists, and why it is gated
//!
//! `codex app-server` is a *turn-based agent*, not a completion endpoint — the wrong
//! transport for a between-keystrokes feature, which is why the HTTP path refuses Codex
//! outright. But for an owner whose only working provider is their Codex subscription,
//! a slow suggestion beats no suggestion — *if* it is actually usable. So this path is
//! opt-in (`ai.codex_autocomplete`, off by default) and self-disabling: a rolling median
//! of turn round-trips above [`LATENCY_GATE`] switches the feature off with a reason the
//! settings row can print, rather than silently degrading typing into waiting.
//!
//! # Containment rules
//!
//! - **Its own child.** Never the chat panel's session — a completion must not inject
//!   turns into the user's conversation, and the chat's turn lock must never wait on one.
//! - **Read-only, rootless.** The thread gets no `cwd` and a `read-only` sandbox: the
//!   prompt already carries the only context this feature is allowed to send (the same
//!   window `provider::build_user_turn` builds), and a completion thread free to explore
//!   a project would be both slow and a privacy hole.
//! - **Every server question is refused.** A completion thread must never raise GUI; any
//!   approval/elicitation is auto-declined and unknown methods get `method_not_found`,
//!   because an unanswered id parks the CLI forever — the chat panel's hardest-won lesson.
//! - **One turn at a time.** The session sits behind a mutex; a fire while one is in
//!   flight is skipped (`try_lock`), not queued. The validity stamp on the editor side
//!   already discards a suggestion that lands late.
//!
//! ponytail: no `turn/interrupt` — a skipped fire just lets the running turn finish and
//! the stamp discards a stale result; the latency gate handles chronic slowness. Wire
//! the interrupt if profiling ever shows abandoned turns burning the subscription.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Codex pays per-turn latency the HTTP wire does not, so it earns a longer pause: the
/// request should fire when the user has actually stopped, not between two keystrokes.
pub const DEBOUNCE: Duration = Duration::from_millis(1000);

/// The usability bar: a rolling median above this disables the feature out loud.
pub const LATENCY_GATE: Duration = Duration::from_secs(3);

/// Turns measured before the gate may judge — a single cold-start spike must not kill
/// the feature on its first suggestion.
const GATE_WINDOW: usize = 5;

/// The instruction that rides in front of the context window. The "no tools" sentence is
/// load-bearing: without it the agent explores, and exploration is 20 s nobody asked for.
const TURN_PROMPT: &str = "You are a code-completion engine. Given a code prefix and \
    suffix around a <|cursor|> marker, reply with ONLY the raw text to insert at the \
    cursor. No prose, no markdown fences, no repetition of the prefix. Do not run \
    commands, do not read files, do not use any tools.";

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    thread_id: String,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The child has no other owner; a dead session must not leave a process behind.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Predictor {
    session: Option<Session>,
    /// The last few turn round-trips, for the gate's rolling median.
    latency: VecDeque<Duration>,
    /// `Some` once the gate has tripped; the sentence is what the settings row prints.
    disabled: Option<String>,
}

/// One process-wide predictor: every editor shares the session (and the gate verdict),
/// and the settings panel can read the status without a path to any particular editor.
fn predictor() -> &'static Mutex<Predictor> {
    static PREDICTOR: OnceLock<Mutex<Predictor>> = OnceLock::new();
    PREDICTOR.get_or_init(|| Mutex::new(Predictor::default()))
}

/// Why the feature is currently not firing, if the gate tripped. For the settings row.
pub fn disabled_reason() -> Option<String> {
    predictor().lock().ok()?.disabled.clone()
}

/// One completion request, end to end, **blocking** — run it via `background_spawn`.
///
/// `Err` is for the log (the editor's failure mode is silence); `Ok("")` means the model
/// had nothing to offer. A fire while a turn is already running returns `Err` immediately
/// rather than queueing — see the module doc.
pub fn complete(user_turn: String) -> Result<String, String> {
    let Ok(mut predictor) = predictor().try_lock() else {
        return Err("a Codex completion turn is already running".to_string());
    };
    if let Some(reason) = &predictor.disabled {
        return Err(reason.clone());
    }

    let started = Instant::now();
    let text = predictor.run_turn(&user_turn)?;
    predictor.record_latency(started.elapsed());
    Ok(text)
}

impl Predictor {
    /// Feeds the gate: keeps the rolling window, and trips `disabled` when the median
    /// crosses [`LATENCY_GATE`]. A median, not a mean, so one cold-start spike among
    /// fast turns cannot kill the feature on its own.
    fn record_latency(&mut self, elapsed: Duration) {
        self.latency.push_back(elapsed);
        if self.latency.len() > GATE_WINDOW {
            self.latency.pop_front();
        }
        if self.latency.len() < GATE_WINDOW {
            return;
        }
        let mut sorted: Vec<Duration> = self.latency.iter().copied().collect();
        sorted.sort();
        let median = sorted[sorted.len() / 2];
        if median > LATENCY_GATE {
            // Sized for the settings row it prints in — the row truncates, and a
            // sentence that must fit its row is one short enough to act on.
            self.disabled = Some(format!(
                "codex answered too slowly (median {}s) — disabled",
                median.as_secs()
            ));
            self.session = None; // and the child goes with it
        }
    }

    fn run_turn(&mut self, user_turn: &str) -> Result<String, String> {
        // An existing session first; a dead one falls through to a respawn once.
        if self.session.is_some() {
            match self.turn_on_session(user_turn) {
                Ok(text) => return Ok(text),
                Err(_) => self.session = None,
            }
        }
        self.spawn_session()?;
        self.turn_on_session(user_turn)
    }

    /// Spawns `codex app-server`, handshakes, and opens the rootless read-only thread.
    fn spawn_session(&mut self) -> Result<(), String> {
        let binary = crate::ai_codex::binary().ok_or("Codex CLI not found")?;
        let mut child = std::process::Command::new(binary)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("could not run the Codex CLI: {err}"))?;
        let mut stdin = child.stdin.take().ok_or("the Codex CLI gave no stdin")?;
        let stdout = child.stdout.take().ok_or("the Codex CLI gave no stdout")?;
        let mut stdout = BufReader::new(stdout);

        // No cwd on purpose — see the module doc's containment rules.
        let handshake = crate::ai_codex::initialize_request(env!("CARGO_PKG_VERSION"))
            + &crate::ai_codex::thread_start_request(None);
        stdin
            .write_all(handshake.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|err| format!("handshake write failed: {err}"))?;

        let mut line = String::new();
        let thread_id = loop {
            line.clear();
            match stdout.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    return Err(
                        "the Codex CLI closed during the handshake — try `codex login`".to_string()
                    );
                }
                Ok(_) => {}
            }
            match crate::ai_codex::parse_line(&line) {
                Some(crate::ai_codex::CodexEvent::ThreadStarted(id)) => break id,
                Some(crate::ai_codex::CodexEvent::TurnFailed(message)) => return Err(message),
                _ => {}
            }
        };

        self.session = Some(Session { child, stdin, stdout, thread_id });
        Ok(())
    }

    /// One turn on the live session: send, then read until the turn ends, refusing every
    /// question the CLI asks along the way.
    fn turn_on_session(&mut self, user_turn: &str) -> Result<String, String> {
        let session = self.session.as_mut().ok_or("no session")?;
        let request = crate::ai_codex::turn_start_request(
            &session.thread_id,
            &format!("{TURN_PROMPT}\n\n{user_turn}"),
            false,
        );
        session
            .stdin
            .write_all(request.as_bytes())
            .and_then(|()| session.stdin.flush())
            .map_err(|err| format!("turn write failed: {err}"))?;

        let mut text = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            match session.stdout.read_line(&mut line) {
                Ok(0) | Err(_) => return Err("the Codex CLI closed mid-turn".to_string()),
                Ok(_) => {}
            }
            use crate::ai_codex::CodexEvent;
            match crate::ai_codex::parse_line(&line) {
                Some(CodexEvent::Delta(delta)) => text.push_str(&delta),
                Some(CodexEvent::TurnCompleted) => return Ok(text),
                Some(CodexEvent::TurnFailed(message)) => return Err(message),
                // Every question is refused — a completion thread never raises GUI, and
                // an unanswered id would park the CLI (and this reader) forever.
                Some(CodexEvent::ApprovalRequested { request_id, .. }) => {
                    let reply = crate::ai_codex::approval_response(request_id, false);
                    let _ = session.stdin.write_all(reply.as_bytes());
                    let _ = session.stdin.flush();
                }
                Some(CodexEvent::ActionApprovalRequested { request_id, kind, .. }) => {
                    let reply = if kind == crate::ai_codex::ApprovalKind::McpElicitation {
                        crate::ai_codex::elicitation_response(request_id, false)
                    } else {
                        crate::ai_codex::action_approval_response(
                            request_id,
                            crate::ai_codex::Decision::Decline,
                        )
                    };
                    let _ = session.stdin.write_all(reply.as_bytes());
                    let _ = session.stdin.flush();
                }
                Some(CodexEvent::UnservableRequest { request_id, .. }) => {
                    let reply = crate::ai_codex::method_not_found_response(request_id);
                    let _ = session.stdin.write_all(reply.as_bytes());
                    let _ = session.stdin.flush();
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The gate arithmetic, exercised on the real method — no CLI, no child.
    #[test]
    fn the_gate_trips_on_a_slow_median_not_on_one_spike() {
        let mut predictor = Predictor::default();

        // One 30s cold-start spike among fast turns: the median holds, no trip.
        for secs in [30, 1, 1, 2, 1] {
            predictor.record_latency(Duration::from_secs(secs));
        }
        assert!(predictor.disabled.is_none(), "one spike must not kill the feature");

        // Chronically slow: the median crosses the gate and the feature says so.
        for secs in [8, 9, 10] {
            predictor.record_latency(Duration::from_secs(secs));
        }
        assert!(predictor.disabled.is_some(), "a slow median disables");
        assert!(
            predictor.disabled.as_deref().unwrap_or("").contains("too slowly"),
            "the reason is a sentence the settings row can print"
        );

        // Latencies at the gate exactly do not trip it — `>` not `>=`, so the bar is
        // "worse than 3s", not "3s".
        let mut at_the_bar = Predictor::default();
        for _ in 0..GATE_WINDOW {
            at_the_bar.record_latency(LATENCY_GATE);
        }
        assert!(at_the_bar.disabled.is_none());
    }

    #[test]
    fn the_turn_prompt_forbids_tools_and_demands_raw_text() {
        // The two load-bearing sentences — losing either one regresses to a 20s
        // exploring agent or to fenced markdown the cleaner has to guess at.
        assert!(TURN_PROMPT.contains("do not use any tools"));
        assert!(TURN_PROMPT.contains("ONLY the raw text"));
    }
}
