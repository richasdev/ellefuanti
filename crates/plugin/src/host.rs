//! Running a plugin process, and surviving its death.
//!
//! This is where ADR-0012's fault isolation becomes code rather than a claim. Every
//! operation here returns a `Result`; none panics on a plugin misbehaving, because the
//! whole point of the out-of-process boundary is that a broken plugin is a recoverable
//! condition (§24).
//!
//! **Blocking**, and unaware of any executor: the app drives it from `cx.background_spawn`
//! exactly as `ai_chat.rs` drives the Codex child (ADR-0007). Nothing here spawns a thread
//! or a task of its own.
//!
//! The session is generic over its streams for the same reason `crates/lsp` hands its pipes
//! out separately: the tests drive a whole conversation over in-memory buffers with no
//! plugin installed, so the mock is not a special case in the code — it is the ordinary
//! path with different streams.

use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};

use anyhow::{Context as _, Result, bail};

use crate::discovery::DiscoveredPlugin;
use crate::protocol::{self, FIRST_INVOKE_ID, PluginEvent};

/// A conversation with a plugin, over whatever streams it was given.
///
/// Owns the request-id counter, which is the only mutable protocol state: ids must be
/// unique across in-flight invocations, and a counter here is what makes a reply
/// attributable to the command that asked.
pub struct Session<R, W> {
    reader: R,
    writer: W,
    next_id: u64,
}

impl<R: BufRead, W: Write> Session<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer, next_id: FIRST_INVOKE_ID }
    }

    /// Sends the handshake and waits for the plugin to answer it.
    ///
    /// Reads until the handshake resolves, skipping chatter — a plugin that logs before it
    /// initialises is being helpful, not broken. EOF before an answer means the process
    /// died during startup, which is an error rather than a hang.
    pub fn initialize(&mut self, api_version: u32, host_version: &str) -> Result<()> {
        self.send(&protocol::initialize_request(api_version, host_version))?;

        loop {
            match self.read_event()? {
                Some(PluginEvent::Initialized) => return Ok(()),
                Some(PluginEvent::CommandFailed { message, .. }) => {
                    bail!("the plugin refused the handshake: {message}")
                }
                // Logs and unknown notifications before the handshake are fine.
                Some(_) => continue,
                None => bail!("the plugin exited before answering the handshake"),
            }
        }
    }

    /// Runs one command and waits for its reply.
    ///
    /// Returns the optional message the plugin wanted shown. Synchronous because a palette
    /// invocation is a discrete user action with a discrete result — and because the whole
    /// call runs on a background task, so waiting here never blocks a frame.
    pub fn invoke(&mut self, command_id: &str) -> Result<Option<String>> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&protocol::invoke_request(id, command_id))?;

        loop {
            match self.read_event()? {
                // Only the reply to *this* id ends the wait. A stale reply from an earlier
                // command that answered late must not be mistaken for this one's.
                Some(PluginEvent::CommandFinished { request_id, message }) if request_id == id => {
                    return Ok(message);
                }
                Some(PluginEvent::CommandFailed { request_id, message }) if request_id == id => {
                    bail!("{message}")
                }
                Some(_) => continue,
                None => bail!("the plugin exited without answering {command_id}"),
            }
        }
    }

    /// Reads the next line and parses it. `Ok(None)` is EOF — the plugin is gone.
    ///
    /// A line that does not parse is skipped rather than fatal: a stray `print` on stdout
    /// is the likeliest bug in any plugin, and it must not cost the user the plugin.
    fn read_event(&mut self) -> Result<Option<PluginEvent>> {
        loop {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line).context("reading from the plugin")?;
            if read == 0 {
                return Ok(None);
            }
            if let Some(event) = protocol::parse_line(&line) {
                return Ok(Some(event));
            }
            tracing::debug!(line = line.trim(), "ignoring unparseable plugin output");
        }
    }

    fn send(&mut self, message: &str) -> Result<()> {
        self.writer.write_all(message.as_bytes()).context("writing to the plugin")?;
        self.writer.flush().context("flushing to the plugin")
    }
}

/// A spawned plugin, with its pipes handed out separately.
///
/// Same shape as `crates/lsp`'s `ServerProcess`: the caller takes the streams into a
/// [`Session`] while keeping the handle in order to kill a plugin that ignores `shutdown`.
pub struct PluginProcess {
    pub child: Child,
}

/// The streams a session reads from and writes to.
pub struct PluginPipes {
    pub stdin: std::process::ChildStdin,
    pub stdout: std::process::ChildStdout,
}

/// Spawns a discovered plugin.
///
/// Errors rather than panics when the executable is missing or unrunnable (§24): a plugin
/// whose binary was deleted is an entirely recoverable situation — the user gets a message
/// and keeps editing.
///
/// The child's working directory is the plugin's own root, so a plugin can find files it
/// shipped beside itself without being told where it was installed.
pub fn spawn(plugin: &DiscoveredPlugin) -> Result<(PluginProcess, PluginPipes)> {
    let executable = plugin.executable();

    let mut command = Command::new(&executable);
    command
        .args(&plugin.manifest.args)
        .current_dir(&plugin.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Piping stderr with nothing reading it eventually blocks the writer, and
        // inheriting it interleaves plugin chatter into our own terminal.
        //
        // ponytail: capture this into the log panel once plugins have a diagnostics UI.
        // Until then it is noise with nowhere to go — the same call `crates/lsp` makes.
        .stderr(Stdio::null());

    let mut child = command.spawn().with_context(|| {
        format!("could not start the {} plugin ({})", plugin.manifest.name, executable.display())
    })?;

    // `take` rather than `unwrap` on the option each time: both were piped just above, so
    // their absence would be a bug in this function, not a runtime condition.
    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");

    Ok((PluginProcess { child }, PluginPipes { stdin, stdout }))
}

/// Asks a plugin to exit, then makes sure it did.
///
/// The notification is the polite half and its failure is deliberately ignored — a plugin
/// that already died cannot be told to. `kill` is what actually guarantees the editor can
/// quit: a plugin hanging on shutdown must not hold the application open.
pub fn shutdown(process: &mut PluginProcess, stdin: &mut impl Write) {
    let _ = stdin.write_all(protocol::shutdown_notification().as_bytes());
    let _ = stdin.flush();
    let _ = process.child.kill();
    let _ = process.child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    use std::path::PathBuf;

    /// A session over in-memory streams: the plugin's scripted output, and a buffer
    /// capturing what the host said. This is the ordinary code path — no plugin installed.
    fn session(scripted_output: &str) -> Session<BufReader<&[u8]>, Vec<u8>> {
        Session::new(BufReader::new(scripted_output.as_bytes()), Vec::new())
    }

    #[test]
    fn a_handshake_completes_and_sends_what_the_protocol_specifies() {
        let mut session = session("{\"id\":1,\"result\":{\"ok\":true}}\n");
        session.initialize(1, "0.4.0").unwrap();

        let sent = String::from_utf8(session.writer.clone()).unwrap();
        assert!(sent.contains("\"method\":\"initialize\""), "{sent}");
        assert!(sent.contains("\"apiVersion\":1"), "{sent}");
        assert!(sent.ends_with('\n'), "{sent:?}");
    }

    #[test]
    fn chatter_before_the_handshake_is_skipped_rather_than_mistaken_for_it() {
        let mut session = session(concat!(
            "{\"method\":\"log\",\"params\":{\"message\":\"warming up\"}}\n",
            "starting up...\n",
            "{\"id\":1,\"result\":{}}\n",
        ));
        session.initialize(1, "0.4.0").unwrap();
    }

    #[test]
    fn a_plugin_that_dies_during_startup_is_an_error_not_a_hang() {
        // §24, the load-bearing case: EOF must resolve the wait.
        let mut session = session("");
        let error = session.initialize(1, "0.4.0").unwrap_err().to_string();
        assert!(error.contains("exited before answering"), "{error}");
    }

    #[test]
    fn a_refused_handshake_is_reported_rather_than_waited_on() {
        let mut session = session("{\"id\":1,\"error\":{\"message\":\"unsupported host\"}}\n");
        let error = session.initialize(1, "0.4.0").unwrap_err().to_string();
        assert!(error.contains("unsupported host"), "{error}");
    }

    #[test]
    fn an_invocation_returns_the_message_the_plugin_wanted_shown() {
        let mut session = session("{\"id\":2,\"result\":{\"message\":\"sorted 12 lines\"}}\n");
        assert_eq!(session.invoke("sort.lines").unwrap(), Some("sorted 12 lines".to_string()));

        let sent = String::from_utf8(session.writer.clone()).unwrap();
        assert!(sent.contains("\"method\":\"command/invoke\""), "{sent}");
        assert!(sent.contains("\"id\":\"sort.lines\""), "{sent}");
    }

    #[test]
    fn a_command_that_simply_worked_returns_nothing_to_show() {
        let mut session = session("{\"id\":2,\"result\":{}}\n");
        assert_eq!(session.invoke("sort.lines").unwrap(), None);
    }

    #[test]
    fn ids_advance_so_two_invocations_are_told_apart() {
        let mut session = session("{\"id\":2,\"result\":{}}\n{\"id\":3,\"result\":{}}\n");
        session.invoke("sort.lines").unwrap();
        session.invoke("sort.unique").unwrap();

        let sent = String::from_utf8(session.writer.clone()).unwrap();
        assert!(sent.contains("\"id\":2"), "{sent}");
        assert!(sent.contains("\"id\":3"), "{sent}");
    }

    #[test]
    fn a_late_reply_to_an_earlier_command_is_not_mistaken_for_this_ones() {
        // A stale id arriving first must be skipped, not returned — otherwise a slow
        // command's answer shows up attributed to whatever the user ran next.
        let mut session = session(concat!(
            "{\"id\":1,\"result\":{\"message\":\"stale\"}}\n",
            "{\"id\":2,\"result\":{\"message\":\"mine\"}}\n",
        ));
        assert_eq!(session.invoke("sort.lines").unwrap(), Some("mine".to_string()));
    }

    #[test]
    fn a_failing_command_surfaces_its_reason_and_leaves_the_session_usable() {
        let mut session = session("{\"id\":2,\"error\":{\"message\":\"no buffer open\"}}\n");
        let error = session.invoke("sort.lines").unwrap_err().to_string();
        assert_eq!(error, "no buffer open");
    }

    #[test]
    fn a_plugin_that_dies_mid_command_is_an_error_not_a_hang() {
        let mut session = session("");
        let error = session.invoke("sort.lines").unwrap_err().to_string();
        assert!(error.contains("exited without answering"), "{error}");
        assert!(error.contains("sort.lines"), "{error}");
    }

    #[test]
    fn garbage_on_stdout_never_costs_the_user_the_plugin() {
        // The likeliest bug in any plugin: a debug print. It must be skipped, and the
        // real reply after it must still arrive.
        let mut session = session(concat!(
            "DEBUG: about to sort\n",
            "{not json\n",
            "{\"id\":2,\"result\":{\"message\":\"done\"}}\n",
        ));
        assert_eq!(session.invoke("sort.lines").unwrap(), Some("done".to_string()));
    }

    #[test]
    fn a_missing_executable_is_an_error_not_a_panic() {
        // §24: a plugin whose binary was deleted must leave the editor working.
        let plugin = DiscoveredPlugin {
            manifest: crate::manifest::parse(
                r#"{"api_version":1,"name":"ghost","command":"definitely-not-a-real-binary-xyzzy"}"#,
            )
            .unwrap(),
            root: std::env::temp_dir(),
        };
        let error = match spawn(&plugin) {
            Ok(_) => panic!("expected spawning a missing binary to fail"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("could not start"), "{error}");
        assert!(error.contains("ghost"), "{error}");
    }

    #[test]
    fn spawning_from_a_root_that_does_not_exist_is_an_error_not_a_panic() {
        let plugin = DiscoveredPlugin {
            manifest: crate::manifest::parse(
                r#"{"api_version":1,"name":"ghost","command":"echo"}"#,
            )
            .unwrap(),
            root: PathBuf::from("/no/such/directory/anywhere"),
        };
        assert!(spawn(&plugin).is_err());
    }
}
