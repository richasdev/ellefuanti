//! Request/response correlation over a framed byte stream.
//!
//! A `Connection` owns the writer and a reader thread. The reader thread's only job is
//! to pull frames off the stream and route them: responses go to whichever caller is
//! waiting on that id, everything else goes onto a queue for the caller to drain.
//!
//! # Why a thread rather than an executor
//!
//! ADR-0007: this crate is blocking and executor-agnostic, like `elle-workspace`. The
//! reader thread exists because a blocking read must live *somewhere*, and a dedicated
//! thread is the smallest thing that works without importing a runtime. The app wraps
//! the blocking calls in `cx.background_spawn`; this crate does not know or care.
//!
//! # Cancellation
//!
//! §22 requires that fast typing not queue a completion per keystroke. Cancellation
//! here is two things at once, and both are needed:
//!
//! 1. `$/cancelRequest` tells the server to stop working. It is advisory — the
//!    specification lets a server finish and reply normally.
//! 2. The pending entry is dropped locally, so the reply is discarded whenever it
//!    arrives and the caller stops waiting immediately.
//!
//! Relying on the notification alone would leave the UI waiting on a server that
//! ignores it.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

use crate::jsonrpc::{
    Incoming, Notification, Request, RequestId, Response, ResponseError, error_codes,
};
use crate::transport::{read_message, write_message};

/// The outcome of a request.
#[derive(Debug)]
pub enum RequestOutcome {
    Ok(Value),
    /// The server replied with an error.
    Failed(ResponseError),
    /// Cancelled locally, or acknowledged as cancelled by the server. Not a fault.
    Cancelled,
}

/// State shared between the caller and the reader thread.
struct Shared {
    /// Requests awaiting a reply, keyed by id. Removing an entry cancels the wait.
    pending: Mutex<HashMap<RequestId, Option<Result<Value, ResponseError>>>>,
    /// Server-initiated traffic the caller has not drained yet.
    inbox: Mutex<Vec<ServerMessage>>,
    /// Signalled whenever the reader thread stores a reply or the stream ends.
    arrived: Condvar,
    /// Set once the reader thread stops, for any reason.
    closed: AtomicBool,
    /// Why the stream ended, if it ended badly. Turns "waiting forever" into a
    /// reportable failure.
    failure: Mutex<Option<String>>,
}

impl Shared {
    /// Wakes every waiter. Called when the stream dies so nobody blocks on a reply
    /// that can no longer arrive — the difference between a crashed server being a
    /// reported error and being a frozen editor (§24).
    fn close(&self, reason: Option<String>) {
        if let Some(reason) = reason {
            *self.failure.lock().unwrap() = Some(reason);
        }
        self.closed.store(true, Ordering::SeqCst);

        // Waiters block on one of two mutexes — `pending` for a reply, `inbox` for
        // server-pushed traffic — and both must be woken. Taking each lock before
        // notifying closes the window where a waiter has checked `closed` but has not
        // yet started waiting; missing that wakeup would leave it blocked on a
        // connection that is already dead.
        {
            let _pending = self.pending.lock().unwrap();
            self.arrived.notify_all();
        }
        {
            let _inbox = self.inbox.lock().unwrap();
            self.arrived.notify_all();
        }
    }
}

/// Something the server sent that was not a reply to us.
#[derive(Debug)]
pub enum ServerMessage {
    Notification { method: String, params: Value },
    Request { id: RequestId, method: String, params: Value },
}

/// A live JSON-RPC connection to a server.
pub struct Connection {
    writer: Mutex<Box<dyn Write + Send>>,
    shared: Arc<Shared>,
    next_id: AtomicI64,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Connection {
    /// Starts a connection over an arbitrary reader/writer pair.
    ///
    /// Generic over the streams rather than tied to `Child`'s pipes, so the tests can
    /// drive a mock server over in-memory pipes without spawning anything. That is what
    /// keeps the suite from requiring Intelephense to be installed.
    pub fn new(
        input: impl Read + Send + 'static,
        output: impl Write + Send + 'static,
        name: String,
    ) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            inbox: Mutex::new(Vec::new()),
            arrived: Condvar::new(),
            closed: AtomicBool::new(false),
            failure: Mutex::new(None),
        });

        let reader_shared = Arc::clone(&shared);
        let reader = std::thread::Builder::new()
            .name(format!("lsp-reader-{name}"))
            .spawn(move || read_loop(input, &reader_shared))
            .ok();

        Self {
            writer: Mutex::new(Box::new(output)),
            shared,
            next_id: AtomicI64::new(1),
            reader: Mutex::new(reader),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// Why the connection ended, if it ended badly.
    pub fn failure(&self) -> Option<String> {
        self.shared.failure.lock().unwrap().clone()
    }

    /// Sends a notification. Fire and forget: there is no reply to wait for.
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        let body = serde_json::to_vec(&Notification::new(method, params))?;
        self.write(&body)
    }

    /// Replies to a server-initiated request.
    pub fn respond(&self, response: Response) -> Result<()> {
        let body = serde_json::to_vec(&response)?;
        self.write(&body)
    }

    /// Sends a request and registers it as pending, without waiting.
    ///
    /// Split from [`Connection::wait`] so a caller can hold the id and cancel it later
    /// — the completion-per-keystroke case.
    pub fn send(&self, method: &str, params: Value) -> Result<RequestId> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));

        // Registered *before* the write, so a reply that arrives while we are still in
        // `write` has somewhere to land instead of being dropped as unknown.
        self.shared.pending.lock().unwrap().insert(id.clone(), None);

        let body = serde_json::to_vec(&Request::new(id.clone(), method, params))?;
        if let Err(err) = self.write(&body) {
            self.shared.pending.lock().unwrap().remove(&id);
            return Err(err);
        }
        Ok(id)
    }

    /// Blocks until `id` is answered, cancelled, or `timeout` elapses.
    ///
    /// A timeout is not optional. A server that hangs — Intelephense indexing a large
    /// vendor tree, or a process wedged on a lock — must not be able to block a caller
    /// forever, because §24 says a misbehaving server may not stop the user editing.
    pub fn wait(&self, id: &RequestId, timeout: Duration) -> Result<RequestOutcome> {
        let deadline = Instant::now() + timeout;
        let mut pending = self.shared.pending.lock().unwrap();

        loop {
            match pending.get(id) {
                // Reply landed.
                Some(Some(_)) => {
                    let result = pending.remove(id).flatten().expect("just matched Some(Some)");
                    return Ok(match result {
                        Ok(value) => RequestOutcome::Ok(value),
                        // A server acknowledging our cancellation is the expected
                        // outcome, not a fault to show the user.
                        Err(error)
                            if error.code == error_codes::REQUEST_CANCELLED
                                || error.code == error_codes::CONTENT_MODIFIED =>
                        {
                            RequestOutcome::Cancelled
                        }
                        Err(error) => RequestOutcome::Failed(error),
                    });
                }
                // Someone cancelled it out from under us.
                None => return Ok(RequestOutcome::Cancelled),
                Some(None) => {}
            }

            if self.shared.closed.load(Ordering::SeqCst) {
                pending.remove(id);
                let reason = self
                    .shared
                    .failure
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "the language server exited".to_string());
                bail!("{reason}");
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                pending.remove(id);
                bail!("the language server did not answer within {timeout:?}");
            }

            let (guard, _) = self.shared.arrived.wait_timeout(pending, remaining).unwrap();
            pending = guard;
        }
    }

    /// Convenience: send and wait.
    pub fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<RequestOutcome> {
        let id = self.send(method, params)?;
        self.wait(&id, timeout)
    }

    /// Cancels a pending request.
    ///
    /// Drops the local entry first so the caller stops waiting even if the server
    /// ignores the notification, then asks the server to stop working. A failure to
    /// send is deliberately swallowed: the local cancellation already succeeded, and a
    /// dead connection is reported by whatever the caller does next.
    pub fn cancel(&self, id: &RequestId) {
        {
            // The notify happens *while the lock is held*. Releasing it first would let
            // a waiter re-check its condition and start waiting in the gap, missing the
            // wakeup and then blocking until its timeout — a cancel that appears to
            // hang, which is the exact symptom cancellation exists to prevent.
            let mut pending = self.shared.pending.lock().unwrap();
            if pending.remove(id).is_none() {
                // Already answered or already cancelled; sending `$/cancelRequest` for
                // an unknown id just makes noise in the server's log.
                return;
            }
            self.shared.arrived.notify_all();
        }

        if let Err(err) = self.notify("$/cancelRequest", serde_json::json!({ "id": id })) {
            tracing::debug!(?id, %err, "could not send $/cancelRequest; cancelled locally anyway");
        }
    }

    /// Takes everything the server has sent that was not a reply.
    pub fn drain_inbox(&self) -> Vec<ServerMessage> {
        std::mem::take(&mut *self.shared.inbox.lock().unwrap())
    }

    /// Blocks until at least one server message is available, or the timeout elapses.
    ///
    /// The wait holds the `inbox` lock rather than `pending`, because the inbox is the
    /// condition being waited on. Waiting on a different mutex than the one guarding the
    /// data would make every wakeup a race: the reader thread could push a message in
    /// the window between the check and the wait, and this would sleep through it.
    pub fn wait_for_inbox(&self, timeout: Duration) -> Vec<ServerMessage> {
        let deadline = Instant::now() + timeout;
        let mut inbox = self.shared.inbox.lock().unwrap();

        loop {
            if !inbox.is_empty() {
                return std::mem::take(&mut *inbox);
            }
            // A closed connection will never deliver anything more, so waiting for the
            // full timeout would just be a stall.
            if self.shared.closed.load(Ordering::SeqCst) {
                return Vec::new();
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Vec::new();
            }

            let (guard, _) = self.shared.arrived.wait_timeout(inbox, remaining).unwrap();
            inbox = guard;
        }
    }

    fn write(&self, body: &[u8]) -> Result<()> {
        if self.is_closed() {
            return Err(anyhow!(
                self.failure().unwrap_or_else(|| "the language server exited".to_string())
            ));
        }
        let mut writer = self.writer.lock().unwrap();
        write_message(&mut *writer, body)
    }

    /// Drops the writer, closing the server's stdin.
    ///
    /// This is what makes a server exit after `exit`: most read stdin to EOF.
    pub fn close_writer(&self) {
        let mut writer = self.writer.lock().unwrap();
        *writer = Box::new(std::io::sink());
    }

    /// Waits for the reader thread to finish.
    pub fn join_reader(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.is_closed() {
                if let Some(handle) = self.reader.lock().unwrap().take() {
                    let _ = handle.join();
                }
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

/// Pulls frames off the stream until it ends, routing each one.
fn read_loop(input: impl Read, shared: &Arc<Shared>) {
    let mut reader = BufReader::new(input);

    loop {
        match read_message(&mut reader) {
            Ok(Some(body)) => {
                let Some(message) = Incoming::parse(&body) else {
                    // One unparseable frame must not end LSP for the session (§24).
                    tracing::warn!("ignoring an unparseable message from the language server");
                    continue;
                };

                match message {
                    Incoming::Response { id, result } => {
                        let mut pending = shared.pending.lock().unwrap();
                        // `get_mut` rather than `insert`: if the entry is gone the
                        // request was cancelled, and resurrecting it would leak an
                        // entry nobody will ever collect.
                        if let Some(slot) = pending.get_mut(&id) {
                            *slot = Some(result);
                            shared.arrived.notify_all();
                        } else {
                            tracing::trace!(?id, "discarding a reply to a cancelled request");
                        }
                    }
                    // Both arms notify while still holding the inbox lock: a waiter that
                    // re-checked and started waiting in the gap would otherwise sleep
                    // through the very message it was waiting for.
                    Incoming::Notification { method, params } => {
                        let mut inbox = shared.inbox.lock().unwrap();
                        inbox.push(ServerMessage::Notification { method, params });
                        shared.arrived.notify_all();
                    }
                    Incoming::Request { id, method, params } => {
                        let mut inbox = shared.inbox.lock().unwrap();
                        inbox.push(ServerMessage::Request { id, method, params });
                        shared.arrived.notify_all();
                    }
                }
            }
            // Clean end of stream: the server exited between messages.
            Ok(None) => {
                shared.close(None);
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "the language server connection failed");
                shared.close(Some(format!("the language server connection failed: {err}")));
                return;
            }
        }
    }
}
