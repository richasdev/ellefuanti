//! Driving an Xdebug session from the UI without blocking it.
//!
//! `elle-debug` is blocking by design (ADR-0007), and its longest call — `run` — does not
//! return until the debugged script hits the next breakpoint. On the main thread that is
//! a frozen window for as long as the user's code takes. So the session is owned by a
//! background task and reached only through channels: [`DebugCommand`] in, [`DebugEvent`]
//! out. Nothing here touches a gpui `Entity`; the workspace marshals events onto the
//! foreground thread exactly as it already does for the test runner (#25).
//!
//! # Why the loop looks like this
//!
//! One thread owns the `Session` for its whole life, because `Session` is `!Sync` in
//! spirit — every method needs `&mut self`, and the protocol is a strictly ordered
//! conversation. Sharing it behind a lock would serialise the same calls anyway while
//! adding a way to deadlock.
//!
//! The loop is therefore: block on the command channel, run the command, send back what
//! happened. A command arriving while `run` is in flight simply waits its turn, which is
//! correct — there is nothing sensible to do with "step over" while the script is running.

use std::path::PathBuf;

use elle_debug::{Listener, Session};

use crate::debug_view::{DebugCommand, DebugEvent};

/// Everything the session thread needs to start.
pub struct SessionConfig {
    pub port: u16,
    /// Breakpoints to register the moment a session opens, as `(path, 0-based row)`.
    ///
    /// Registered wholesale rather than one at a time, because the breakpoints the user
    /// cares about were almost all set before the page was ever loaded.
    pub breakpoints: Vec<(PathBuf, usize)>,
}

/// How long to wait for a connection before looping to check for a stop command.
///
/// Short enough that stopping the debugger feels immediate, long enough that the loop is
/// not a busy-wait. The listener does the real waiting.
const ACCEPT_SLICE: std::time::Duration = std::time::Duration::from_millis(250);

/// Listens, then drives one session to completion.
///
/// Blocking from the first line: the caller runs this on the background executor. Returns
/// when the script finishes, the user stops, or the command channel closes.
pub fn run_session(
    config: SessionConfig,
    commands: smol::channel::Receiver<DebugCommand>,
    events: smol::channel::Sender<DebugEvent>,
) {
    let listener = match Listener::bind(config.port) {
        Ok(listener) => listener,
        Err(error) => {
            // Almost always another debugger already on the port, and the message from
            // `Listener::bind` says so. A dead end, not a retry.
            let _ = events.send_blocking(DebugEvent::Failed { message: format!("{error:#}") });
            return;
        }
    };

    let _ = events.send_blocking(DebugEvent::Listening { port: config.port });

    // Wait for PHP to dial in, giving up only when the user stops the debugger. This is
    // where a session spends most of its life: armed, with the page not yet loaded.
    let mut session = loop {
        if commands.is_closed() {
            return;
        }
        // A `Stop` arriving while nothing has connected ends the listen.
        if let Ok(DebugCommand::Stop) = commands.try_recv() {
            return;
        }

        match listener.accept(ACCEPT_SLICE) {
            Ok(Some(session)) => break session,
            // Nobody yet, or something connected and hung up without speaking. Keep waiting.
            Ok(None) => continue,
            Err(error) => {
                let _ = events.send_blocking(DebugEvent::Failed { message: format!("{error:#}") });
                return;
            }
        }
    };

    let init = session.init().clone();
    let _ = events.send_blocking(DebugEvent::Connected {
        file_uri: init.file_uri.clone(),
        engine_version: init.engine_version.clone(),
    });

    // Breakpoints are registered *before* the first continuation command, while the engine
    // is still in `starting`. Registering them after `run` would mean the first request
    // sails past every one of them.
    for (path, row) in &config.breakpoints {
        let uri = elle_debug::path_to_uri(path);
        // 0-based rows in the editor, 1-based lines on the wire.
        let line = (*row as u32).saturating_add(1);
        match session.set_breakpoint(&uri, line) {
            Ok(engine_id) => {
                let _ = events.send_blocking(DebugEvent::BreakpointBound {
                    file_uri: uri,
                    line,
                    engine_id,
                });
            }
            Err(error) => {
                // One breakpoint the engine will not take — a line with no executable
                // statement is the usual cause — must not abandon the session or the other
                // breakpoints (§24). It is logged and the rest are still registered.
                tracing::debug!(%error, path = %path.display(), row, "Xdebug refused a breakpoint");
            }
        }
    }

    drive(&mut session, &commands, &events);

    // Detach rather than stop, so a web request finishes and returns its page instead of
    // dying mid-response with the browser still waiting.
    let _ = session.detach();
    let _ = events.send_blocking(DebugEvent::Finished);
}

/// The command loop, until the script ends or the user stops.
fn drive(
    session: &mut Session,
    commands: &smol::channel::Receiver<DebugCommand>,
    events: &smol::channel::Sender<DebugEvent>,
) {
    while let Ok(command) = commands.recv_blocking() {
        match command {
            DebugCommand::Stop => return,

            DebugCommand::Run
            | DebugCommand::StepInto
            | DebugCommand::StepOver
            | DebugCommand::StepOut => {
                let outcome = match command {
                    DebugCommand::Run => session.run(),
                    DebugCommand::StepInto => session.step_into(),
                    DebugCommand::StepOver => session.step_over(),
                    _ => session.step_out(),
                };

                match outcome {
                    Ok(stop) => {
                        if stop.is_paused() {
                            // The position comes from the continuation reply itself, so the
                            // gutter arrow moves without waiting for a stack request.
                            if let Some((file_uri, line)) = stop.position {
                                let _ = events.send_blocking(DebugEvent::Paused { file_uri, line });
                            }
                            // The stack follows, because the panel shows it on every stop.
                            // Variables do not: they are fetched per frame, when a frame is
                            // selected, so a stop costs one extra round trip rather than one
                            // per frame.
                            if let Ok(stack) = session.stack() {
                                let _ = events.send_blocking(DebugEvent::Stack(stack));
                            }
                            if let Ok(properties) = session.locals(0) {
                                let _ = events
                                    .send_blocking(DebugEvent::Variables { depth: 0, properties });
                            }
                        } else {
                            // The script ran to the end. Normal, and the end of this session.
                            return;
                        }
                    }
                    // The connection dropped mid-command, which is what a fatal error in
                    // the debugged script looks like from here.
                    Err(error) => {
                        tracing::debug!(%error, "the debug session ended during a step");
                        return;
                    }
                }
            }

            DebugCommand::Inspect { depth } => match session.locals(depth) {
                Ok(properties) => {
                    let _ = events.send_blocking(DebugEvent::Variables { depth, properties });
                }
                Err(error) => {
                    tracing::debug!(%error, depth, "could not read a frame's variables");
                }
            },

            DebugCommand::Expand { full_name, depth } => {
                match session.property(&full_name, depth) {
                    Ok(Some(property)) => {
                        let _ = events.send_blocking(DebugEvent::Expanded { full_name, property });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!(%error, full_name, "could not expand a value");
                    }
                }
            }

            DebugCommand::SetBreakpoint { file_uri, line } => {
                match session.set_breakpoint(&file_uri, line) {
                    Ok(engine_id) => {
                        let _ = events.send_blocking(DebugEvent::BreakpointBound {
                            file_uri,
                            line,
                            engine_id,
                        });
                    }
                    Err(error) => {
                        tracing::debug!(%error, "Xdebug refused a breakpoint set mid-session");
                    }
                }
            }

            DebugCommand::ClearBreakpoint { engine_id } => {
                let _ = session.remove_breakpoint(&engine_id);
            }
        }
    }
}
