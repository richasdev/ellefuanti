//! A live debug session: one connected PHP request, driven command by command.
//!
//! # The IDE listens; PHP dials in
//!
//! This is the structural difference from `elle-lsp`, and it shapes everything here. A
//! language server is a child process we spawn and own. Xdebug is not: PHP is started by
//! someone else — a web server, a CLI run, a queue worker — and *it* connects out to a
//! port we are listening on. So there is no process to manage, no stdin/stdout pair, and
//! no lifetime we control. There is a socket that may or may not ever be dialled.
//!
//! One consequence worth stating plainly: a session ends when the script ends, and a
//! second page load is a *second session*. The listener therefore outlives sessions, and
//! [`Listener::accept`] is expected to be called repeatedly.
//!
//! # Blocking, with timeouts, like every other domain crate
//!
//! No gpui and no async runtime (ADR-0004, ADR-0007). Calls block the calling thread and
//! the app wraps them in `cx.background_spawn`, exactly as it already does for
//! `elle-lsp` and `elle-workspace`.
//!
//! Every blocking call is bounded by a socket timeout. Unlike LSP, the wait here is
//! usually *unbounded by design* — `run` does not answer until the script hits the next
//! breakpoint, which may be a minute of query time away, or never. A dead peer must still
//! not wedge the caller forever, so the read timeout is long rather than absent and
//! [`Session::is_alive`] lets the caller distinguish "still running" from "gone".
//!
//! # Serial by construction
//!
//! Xdebug's command loop reads one command and answers it before reading the next, so
//! there is no correlation machinery here — no pending map, no reader thread, none of
//! what `elle-lsp`'s `Connection` needs. Send, then read the next packet, then check the
//! transaction id matches. That check is not ceremony: a reply to a command that timed
//! out earlier would otherwise be read as the answer to this one, and every subsequent
//! reply would be off by one.

use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::protocol::{
    self, Feature, Init, Packet, Property, Response, StackFrame, Status, context,
};
use crate::transport::{read_packet, write_command};

/// Xdebug 3's default `xdebug.client_port`.
///
/// 9003, not 9000: Xdebug 2 used 9000 and it collided with PHP-FPM, which is exactly the
/// setup this IDE is for. A user on Xdebug 2 must override this.
pub const DEFAULT_PORT: u16 = 9003;

/// How long a read may block before the peer is presumed gone.
///
/// Long on purpose. A `run` that does not answer for thirty seconds is a slow request,
/// not a fault, and a short timeout here would abort real sessions — the failure mode
/// that makes a debugger untrustworthy. This is the outer bound after which a peer that
/// has stopped speaking is reported instead of waited on forever.
pub const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// A socket waiting for PHP to connect.
pub struct Listener {
    inner: TcpListener,
}

impl Listener {
    /// Binds the debug port.
    ///
    /// Loopback only, never `0.0.0.0`. A debugger port accepts commands that read
    /// arbitrary program state, and binding it to every interface would expose that to
    /// the network. Docker's `host.docker.internal` reaches loopback, so the container
    /// case still works.
    pub fn bind(port: u16) -> Result<Self> {
        let inner = TcpListener::bind(("127.0.0.1", port)).with_context(|| {
            format!(
                "could not listen on port {port} for Xdebug. Another debugger — PhpStorm, \
                 VS Code, or a second window of this one — is probably already listening."
            )
        })?;
        Ok(Self { inner })
    }

    /// The port actually bound. Differs from the requested one when 0 was asked for,
    /// which is how the tests get a free port.
    pub fn port(&self) -> Result<u16> {
        Ok(self.inner.local_addr()?.port())
    }

    /// Blocks until PHP connects and sends its `<init>`.
    ///
    /// Returns `Ok(None)` on timeout, which is not a failure: nobody loaded a page yet.
    /// The caller polls this on a background thread and keeps waiting.
    pub fn accept(&self, timeout: Duration) -> Result<Option<Session>> {
        self.inner.set_nonblocking(true)?;
        let deadline = std::time::Instant::now() + timeout;

        let stream = loop {
            match self.inner.accept() {
                Ok((stream, address)) => break (stream, address),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        self.inner.set_nonblocking(false)?;
                        return Ok(None);
                    }
                    // Polling rather than blocking so the caller can be shut down between
                    // attempts; a blocking accept is not interruptible.
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    self.inner.set_nonblocking(false)?;
                    return Err(err).context("accepting an Xdebug connection");
                }
            }
        };

        self.inner.set_nonblocking(false)?;
        let (stream, address) = stream;
        stream.set_nonblocking(false)?;
        Session::start(stream, address)
    }
}

/// One connected PHP request.
pub struct Session {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    next_transaction: u32,
    init: Init,
    status: Status,
    peer: SocketAddr,
}

impl Session {
    /// Reads the `<init>` packet and negotiates the features that make inspection work.
    fn start(stream: TcpStream, peer: SocketAddr) -> Result<Option<Self>> {
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        // Commands are small and latency matters more than packing them; without this,
        // each one waits on Nagle's algorithm before Xdebug ever sees it.
        stream.set_nodelay(true)?;

        let writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);

        let Some(xml) = read_packet(&mut reader)? else {
            // Connected and hung up without speaking. A port scanner, or a second IDE
            // probing the port. Not an error worth surfacing.
            return Ok(None);
        };

        let packet = protocol::parse_packet(&xml)
            .context("the peer that connected to the debug port did not send valid DBGp")?;
        let Packet::Init(init) = packet else {
            bail!("expected an <init> packet to open the session, got a response");
        };

        let mut session =
            Self { writer, reader, next_transaction: 1, init, status: Status::Starting, peer };

        // Before anything else, and before the caller sets breakpoints: the defaults make
        // every array look empty. See `Feature`.
        for (name, value) in [
            ("max_depth", Feature::MAX_DEPTH),
            ("max_children", Feature::MAX_CHILDREN),
            ("max_data", Feature::MAX_DATA),
        ] {
            // A refused feature is survivable — inspection is merely shallower — so this
            // logs rather than aborting a session that is otherwise fine.
            if let Err(err) = session.feature_set(name, &value.to_string()) {
                tracing::warn!(%err, feature = name, "Xdebug refused a feature; continuing");
            }
        }

        Ok(Some(session))
    }

    /// The `<init>` packet that opened this session.
    pub fn init(&self) -> &Init {
        &self.init
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// The last status the engine reported.
    pub fn status(&self) -> Status {
        self.status
    }

    /// Whether the script can still be stepped.
    pub fn is_alive(&self) -> bool {
        self.status.is_live()
    }

    fn feature_set(&mut self, name: &str, value: &str) -> Result<Response> {
        let id = self.next_transaction;
        self.exchange(&protocol::feature_set(id, name, value))
    }

    /// Sends a command and reads its reply.
    ///
    /// The transaction id is checked rather than assumed. Xdebug answers serially, so a
    /// mismatch means the stream is out of step — a reply to something that timed out
    /// earlier — and reading on would misattribute every reply after it.
    fn exchange(&mut self, command: &str) -> Result<Response> {
        let expected = self.next_transaction;
        self.next_transaction += 1;

        write_command(&mut self.writer, command)
            .context("the debug session closed while sending a command")?;

        let Some(xml) = read_packet(&mut self.reader)? else {
            // The script ended. Normal, and the caller learns it from the status rather
            // than from an error.
            self.status = Status::Stopped;
            bail!("the debug session ended: the script finished or the connection dropped");
        };

        let packet = protocol::parse_packet(&xml)?;
        let Packet::Response(response) = packet else {
            bail!("expected a response, got a second <init>");
        };

        if response.transaction_id != expected {
            bail!(
                "DBGp replies are out of step: expected transaction {expected}, got {}",
                response.transaction_id
            );
        }

        if let Some(status) = response.status {
            self.status = status;
        }

        Ok(response)
    }

    /// Sets a line breakpoint, returning the id needed to remove it.
    ///
    /// `file_uri` must be the path *PHP* sees. Locally that is the editor's own path; the
    /// Docker case where they differ is the path mapping left out of scope.
    pub fn set_breakpoint(&mut self, file_uri: &str, line: u32) -> Result<String> {
        let id = self.next_transaction;
        let response = self.exchange(&protocol::breakpoint_set_line(id, file_uri, line))?;
        if let Some(error) = response.error {
            bail!("could not set a breakpoint at {file_uri}:{line}: {error}");
        }
        response
            .breakpoint_id
            .context("Xdebug accepted the breakpoint but did not return an id to remove it by")
    }

    pub fn remove_breakpoint(&mut self, breakpoint_id: &str) -> Result<()> {
        let id = self.next_transaction;
        let response = self.exchange(&protocol::breakpoint_remove(id, breakpoint_id))?;
        if let Some(error) = response.error {
            // A breakpoint the engine has already dropped is not a fault worth showing:
            // the desired state — no breakpoint there — already holds.
            tracing::debug!(%error, breakpoint_id, "Xdebug rejected a breakpoint removal");
        }
        Ok(())
    }

    /// Resumes until the next breakpoint or the end of the script.
    pub fn run(&mut self) -> Result<Stop> {
        let id = self.next_transaction;
        let response = self.exchange(&protocol::run(id))?;
        Ok(self.stop_from(response))
    }

    pub fn step_into(&mut self) -> Result<Stop> {
        let id = self.next_transaction;
        let response = self.exchange(&protocol::step_into(id))?;
        Ok(self.stop_from(response))
    }

    pub fn step_over(&mut self) -> Result<Stop> {
        let id = self.next_transaction;
        let response = self.exchange(&protocol::step_over(id))?;
        Ok(self.stop_from(response))
    }

    pub fn step_out(&mut self) -> Result<Stop> {
        let id = self.next_transaction;
        let response = self.exchange(&protocol::step_out(id))?;
        Ok(self.stop_from(response))
    }

    /// The call stack, innermost frame first.
    pub fn stack(&mut self) -> Result<Vec<StackFrame>> {
        let id = self.next_transaction;
        let response = self.exchange(&protocol::stack_get(id))?;
        if let Some(error) = response.error {
            // `stack_get` before the script starts is an error rather than an empty stack.
            // Reporting it as "no frames" is the honest reading for the panel.
            tracing::debug!(%error, "stack_get was refused");
            return Ok(Vec::new());
        }
        Ok(response.stack)
    }

    /// The local variables of one stack frame.
    pub fn locals(&mut self, stack_depth: u32) -> Result<Vec<Property>> {
        self.context(context::LOCALS, stack_depth)
    }

    /// One scope's variables. See [`context`] for the ids.
    pub fn context(&mut self, context_id: u32, stack_depth: u32) -> Result<Vec<Property>> {
        let id = self.next_transaction;
        let response = self.exchange(&protocol::context_get(id, context_id, stack_depth))?;
        if let Some(error) = response.error {
            bail!("could not read variables: {error}");
        }
        Ok(response.properties)
    }

    /// One value, addressed by its `full_name`.
    ///
    /// How a container truncated by `max_depth` is expanded: ask for the node the user
    /// opened rather than refetching the whole scope deeper.
    pub fn property(&mut self, full_name: &str, stack_depth: u32) -> Result<Option<Property>> {
        let id = self.next_transaction;
        let response = self.exchange(&protocol::property_get(id, full_name, stack_depth))?;
        if let Some(error) = response.error {
            bail!("could not read {full_name}: {error}");
        }
        Ok(response.properties.into_iter().next())
    }

    /// Detaches, letting the script finish on its own.
    ///
    /// `detach` rather than `stop`, so a web request completes and returns its page
    /// instead of dying mid-response with the browser still waiting.
    pub fn detach(&mut self) -> Result<()> {
        let id = self.next_transaction;
        // The reply may never come if the script has already ended, and that is a clean
        // outcome: the goal was to stop debugging, and it is stopped either way.
        if let Err(err) = self.exchange(&protocol::detach(id)) {
            tracing::debug!(%err, "the session had already ended when detaching");
        }
        self.status = Status::Stopped;
        let _ = self.writer.flush();
        Ok(())
    }

    /// Builds the stop from a continuation reply.
    ///
    /// Uses the position Xdebug volunteers in `<xdebug:message>` rather than issuing a
    /// `stack_get`: the reply already says where execution is, and a round trip per step
    /// is the difference between stepping that feels instant and stepping that does not.
    /// The caller fetches the full stack only when the stack panel is actually shown.
    fn stop_from(&self, response: Response) -> Stop {
        Stop {
            status: response.status.unwrap_or(self.status),
            position: response.position,
            error: response.error.map(|error| error.to_string()),
        }
    }
}

/// Where a continuation command left the script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stop {
    pub status: Status,
    /// `(file_uri, 1-based line)`, when the engine reported one.
    pub position: Option<(String, u32)>,
    pub error: Option<String>,
}

impl Stop {
    /// Whether execution is paused somewhere inspectable.
    pub fn is_paused(&self) -> bool {
        self.status == Status::Break
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Drives a real `Session` over a real TCP socket by playing the part of Xdebug.
    ///
    /// A loopback socket rather than an in-memory pipe because the connection direction is
    /// the thing most likely to be wrong: this crate must *listen* and PHP must dial in.
    /// A pipe would let a version that got that backwards pass.
    struct FakeEngine {
        stream: TcpStream,
    }

    impl FakeEngine {
        fn send(&mut self, xml: &str) {
            let mut packet = xml.len().to_string().into_bytes();
            packet.push(0);
            packet.extend(xml.as_bytes());
            packet.push(0);
            self.stream.write_all(&packet).unwrap();
            self.stream.flush().unwrap();
        }

        /// Reads one NUL-terminated command.
        fn recv(&mut self) -> String {
            let mut command = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                if self.stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                if byte[0] == 0 {
                    break;
                }
                command.push(byte[0]);
            }
            String::from_utf8(command).unwrap()
        }

        /// Answers the three `feature_set` calls the handshake always makes.
        fn accept_handshake(&mut self) {
            for _ in 0..3 {
                let command = self.recv();
                assert!(command.starts_with("feature_set"), "got {command:?}");
                let id = transaction_id(&command);
                self.send(&format!(
                    r#"<response xmlns="urn:debugger_protocol_v1" command="feature_set" transaction_id="{id}" success="1"></response>"#
                ));
            }
        }
    }

    fn transaction_id(command: &str) -> u32 {
        let mut parts = command.split_whitespace();
        while let Some(part) = parts.next() {
            if part == "-i" {
                return parts.next().unwrap().parse().unwrap();
            }
        }
        panic!("no transaction id in {command:?}");
    }

    const INIT: &str = r#"<init xmlns="urn:debugger_protocol_v1" fileuri="file:///srv/app/index.php" language="PHP" protocol_version="1.0" idekey="ellefuanti"><engine version="3.3.1"><![CDATA[Xdebug]]></engine></init>"#;

    /// Binds a listener, dials it as Xdebug would, and completes the handshake.
    fn connected() -> (Session, FakeEngine) {
        let listener = Listener::bind(0).unwrap();
        let port = listener.port().unwrap();

        let engine = std::thread::spawn(move || {
            let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut engine = FakeEngine { stream };
            engine.send(INIT);
            engine.accept_handshake();
            engine
        });

        let session = listener.accept(Duration::from_secs(5)).unwrap().unwrap();
        (session, engine.join().unwrap())
    }

    #[test]
    fn the_ide_listens_and_php_dials_in() {
        // The structural fact that separates this from LSP. If this crate ever grows a
        // `Command::spawn`, this test is the one that should stop it.
        let (session, _engine) = connected();
        assert_eq!(session.init().file_uri, "file:///srv/app/index.php");
        assert_eq!(session.init().engine_version, "3.3.1");
        assert_eq!(session.init().idekey, "ellefuanti");
        assert_eq!(session.status(), Status::Starting);
    }

    #[test]
    fn the_handshake_raises_the_limits_before_the_caller_gets_the_session() {
        // Ordering matters: features must be set while the engine is still in `starting`,
        // before any breakpoint or continuation command. `accept_handshake` asserts the
        // three commands arrive, and arriving before `accept` returns is what this pins.
        let listener = Listener::bind(0).unwrap();
        let port = listener.port().unwrap();

        let engine = std::thread::spawn(move || {
            let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut engine = FakeEngine { stream };
            engine.send(INIT);

            let mut features = Vec::new();
            for _ in 0..3 {
                let command = engine.recv();
                features.push(command.clone());
                let id = transaction_id(&command);
                engine.send(&format!(
                    r#"<response command="feature_set" transaction_id="{id}" success="1"></response>"#
                ));
            }
            features
        });

        let _session = listener.accept(Duration::from_secs(5)).unwrap().unwrap();
        let features = engine.join().unwrap();
        assert!(features.iter().any(|f| f.contains("max_depth")), "{features:?}");
        assert!(features.iter().any(|f| f.contains("max_children")), "{features:?}");
        assert!(features.iter().any(|f| f.contains("max_data")), "{features:?}");
    }

    #[test]
    fn a_breakpoint_round_trips_and_a_run_reports_where_it_stopped() {
        let (mut session, mut engine) = connected();

        let driver = std::thread::spawn(move || {
            let command = engine.recv();
            assert!(command.starts_with("breakpoint_set"), "{command}");
            assert!(command.contains("-t line"), "{command}");
            assert!(command.contains("-n 24"), "{command}");
            let id = transaction_id(&command);
            engine.send(&format!(
                r#"<response command="breakpoint_set" transaction_id="{id}" id="990001"></response>"#
            ));

            let command = engine.recv();
            assert_eq!(command, format!("run -i {}", transaction_id(&command)));
            let id = transaction_id(&command);
            engine.send(&format!(
                r#"<response xmlns:xdebug="https://xdebug.org/dbgp/xdebug" command="run" transaction_id="{id}" status="break" reason="ok"><xdebug:message filename="file:///srv/app/index.php" lineno="24"></xdebug:message></response>"#
            ));
            engine
        });

        let breakpoint = session.set_breakpoint("file:///srv/app/index.php", 24).unwrap();
        assert_eq!(breakpoint, "990001");

        let stop = session.run().unwrap();
        assert!(stop.is_paused());
        assert_eq!(stop.position, Some(("file:///srv/app/index.php".to_string(), 24)));
        assert_eq!(session.status(), Status::Break);
        assert!(session.is_alive());
        drop(driver.join().unwrap());
    }

    #[test]
    fn a_script_that_runs_to_completion_ends_the_session_rather_than_erroring() {
        // The normal end of every session. It must read as "finished", not as a fault.
        let (mut session, mut engine) = connected();

        let driver = std::thread::spawn(move || {
            let command = engine.recv();
            let id = transaction_id(&command);
            engine.send(&format!(
                r#"<response command="run" transaction_id="{id}" status="stopping" reason="ok"></response>"#
            ));
            engine
        });

        let stop = session.run().unwrap();
        assert_eq!(stop.status, Status::Stopping);
        assert!(!stop.is_paused());
        assert!(!session.is_alive(), "a finished script cannot be stepped");
        drop(driver.join().unwrap());
    }

    #[test]
    fn stepping_and_the_stack_and_the_locals_work_together() {
        // The end-to-end path the panel actually walks: step, ask where we are, ask what
        // is in scope.
        let (mut session, mut engine) = connected();

        let driver = std::thread::spawn(move || {
            let command = engine.recv();
            assert!(command.starts_with("step_into"), "{command}");
            let id = transaction_id(&command);
            engine.send(&format!(
                r#"<response xmlns:xdebug="https://xdebug.org/dbgp/xdebug" command="step_into" transaction_id="{id}" status="break" reason="ok"><xdebug:message filename="file:///srv/app/User.php" lineno="12"></xdebug:message></response>"#
            ));

            let command = engine.recv();
            assert!(command.starts_with("stack_get"), "{command}");
            let id = transaction_id(&command);
            engine.send(&format!(
                r#"<response command="stack_get" transaction_id="{id}"><stack where="App\Models\User-&gt;name" level="0" type="file" filename="file:///srv/app/User.php" lineno="12"></stack><stack where="{{main}}" level="1" type="file" filename="file:///srv/app/index.php" lineno="24"></stack></response>"#
            ));

            let command = engine.recv();
            assert!(command.starts_with("context_get"), "{command}");
            // The stack level must travel with it, or frame 1 shows frame 0's variables.
            assert!(command.contains("-d 1"), "{command}");
            let id = transaction_id(&command);
            engine.send(&format!(
                r#"<response command="context_get" transaction_id="{id}" context="0"><property name="$name" fullname="$name" type="string" encoding="base64"><![CDATA[UmljYXJkbw==]]></property></response>"#
            ));
            engine
        });

        let stop = session.step_into().unwrap();
        assert_eq!(stop.position.unwrap().1, 12);

        let stack = session.stack().unwrap();
        assert_eq!(stack.len(), 2);
        assert_eq!(stack[0].function, "App\\Models\\User->name");
        assert_eq!(stack[1].function, "{main}");

        let locals = session.locals(1).unwrap();
        assert_eq!(locals[0].name, "$name");
        assert_eq!(locals[0].value.as_deref(), Some("Ricardo"));
        drop(driver.join().unwrap());
    }

    #[test]
    fn a_reply_out_of_step_is_reported_rather_than_misattributed() {
        // The failure the transaction check exists for: an engine answering with a stale
        // id. Reading on would attribute this reply — and every later one — to the wrong
        // command, which is a debugger showing the wrong file's variables.
        let (mut session, mut engine) = connected();

        let driver = std::thread::spawn(move || {
            let command = engine.recv();
            let real = transaction_id(&command);
            engine.send(&format!(
                r#"<response command="run" transaction_id="{}" status="break"></response>"#,
                real + 7
            ));
            engine
        });

        let err = session.run().unwrap_err().to_string();
        assert!(err.contains("out of step"), "{err}");
        drop(driver.join().unwrap());
    }

    #[test]
    fn a_failed_breakpoint_is_an_error_not_a_dead_session() {
        let (mut session, mut engine) = connected();

        let driver = std::thread::spawn(move || {
            let command = engine.recv();
            let id = transaction_id(&command);
            engine.send(&format!(
                r#"<response command="breakpoint_set" transaction_id="{id}"><error code="200"><message><![CDATA[breakpoint could not be set]]></message></error></response>"#
            ));
            engine
        });

        let err = session.set_breakpoint("file:///srv/app/missing.php", 3).unwrap_err().to_string();
        assert!(err.contains("could not set a breakpoint"), "{err}");
        // The session itself is untouched: one rejected command is not a lost connection.
        assert!(session.is_alive());
        drop(driver.join().unwrap());
    }

    #[test]
    fn a_peer_that_hangs_up_without_speaking_is_not_a_session() {
        // A port scanner, or another IDE probing 9003. It must not produce an error dialog.
        let listener = Listener::bind(0).unwrap();
        let port = listener.port().unwrap();

        std::thread::spawn(move || {
            let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            drop(stream);
        });

        assert!(listener.accept(Duration::from_secs(5)).unwrap().is_none());
    }

    #[test]
    fn accept_times_out_without_failing_when_nobody_connects() {
        // The common case by far: the listener is up and the user has not loaded a page.
        let listener = Listener::bind(0).unwrap();
        assert!(listener.accept(Duration::from_millis(150)).unwrap().is_none());
    }

    #[test]
    fn a_second_listener_on_the_same_port_reports_the_conflict_usefully() {
        // The single commonest setup failure: PhpStorm is already listening on 9003. The
        // message has to say so, because the OS error alone does not.
        let first = Listener::bind(0).unwrap();
        let port = first.port().unwrap();
        let err = match Listener::bind(port) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("binding an already-bound port should have failed"),
        };
        assert!(err.contains("already listening"), "{err}");
    }

    #[test]
    fn the_listener_outlives_a_session_and_accepts_the_next_request() {
        // Every page load is a new session. A listener that served one and stopped would
        // debug the first request and silently ignore the rest.
        let listener = Listener::bind(0).unwrap();
        let port = listener.port().unwrap();

        for _ in 0..2 {
            let engine = std::thread::spawn(move || {
                let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
                let mut engine = FakeEngine { stream };
                engine.send(INIT);
                engine.accept_handshake();
                engine
            });
            let session = listener.accept(Duration::from_secs(5)).unwrap();
            assert!(session.is_some());
            drop(engine.join().unwrap());
        }
    }

    #[test]
    fn detaching_lets_the_script_finish() {
        let (mut session, mut engine) = connected();

        let driver = std::thread::spawn(move || {
            let command = engine.recv();
            assert!(command.starts_with("detach"), "{command}");
            let id = transaction_id(&command);
            engine.send(&format!(
                r#"<response command="detach" transaction_id="{id}" status="stopping" reason="ok"></response>"#
            ));
            engine
        });

        session.detach().unwrap();
        assert_eq!(session.status(), Status::Stopped);
        assert!(!session.is_alive());
        drop(driver.join().unwrap());
    }

    #[test]
    fn expanding_a_truncated_container_asks_for_the_node_by_full_name() {
        let (mut session, mut engine) = connected();

        let driver = std::thread::spawn(move || {
            let command = engine.recv();
            assert!(command.starts_with("property_get"), "{command}");
            assert!(command.contains("-n $user"), "{command}");
            let id = transaction_id(&command);
            engine.send(&format!(
                r#"<response command="property_get" transaction_id="{id}"><property name="$user" fullname="$user" type="array" children="1" numchildren="1"><property name="id" fullname="$user['id']" type="int"><![CDATA[7]]></property></property></response>"#
            ));
            engine
        });

        let property = session.property("$user", 0).unwrap().unwrap();
        assert_eq!(property.children[0].value.as_deref(), Some("7"));
        drop(driver.join().unwrap());
    }
}
