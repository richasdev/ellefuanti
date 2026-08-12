//! Running a suite: spawn, read output as it arrives, and stop when asked.
//!
//! # No polling
//!
//! [`run`] blocks on `read_line` and hands each line to a callback the moment it arrives.
//! There is no timer and no sleep, so a run that is not happening costs nothing at all —
//! which is what keeps the idle footprint the perf gate measures unchanged by this crate
//! (#79, #93).
//!
//! # Cancellation
//!
//! Dropping a gpui `Task` stops the caller *awaiting* this function, but the child process
//! would keep running and the blocking read would keep it alive (ADR-0007). So cancellation
//! here is two things, both required: a [`CancelFlag`] checked between lines, and a `kill`
//! on the child. The flag alone would wait for the current line, and a suite blocked on a
//! database connection never produces one.

use std::io::{BufRead, BufReader};
use std::process::{Command as StdCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

/// How long a cancellation can take to be noticed.
///
/// The watcher below waits this long between checks of the flag. Small enough that
/// cancelling feels immediate, and large enough that the thread is asleep essentially all
/// of the time — and it only exists while a suite is running, so an idle editor has no such
/// thread at all (#79, #93).
const CANCEL_LATENCY: Duration = Duration::from_millis(50);

use crate::detect::Command;
use crate::teamcity::Event;

/// Cooperative cancellation for a run in progress.
///
/// The same shape as `elle_workspace::CancelFlag`, and deliberately not that type: this
/// crate does not depend on `elle-workspace`, and a test runner that pulled in the file
/// indexer to borrow an `AtomicBool` would be a dependency edge §24 does not want.
#[derive(Clone, Default, Debug)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the run to stop. Safe from another thread, and idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Kills the runner and everything it spawned.
///
/// The child leads its own process group (`process_group(0)` at spawn), so the negative
/// pid addresses the group — `sh` *and* the suite under it. `Child::kill` follows as a
/// fallback for the group already being gone, and because it is what updates the
/// `Child`'s own bookkeeping.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    // SAFETY: plain syscall with no memory to manage; a stale pid is answered with
    // ESRCH, which is ignored like every other way the tree can already be dead.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
}

/// How a run ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The runner exited on its own. The code is its verdict on the suite: 0 means every
    /// test passed, and anything else means it did not — but *which* tests failed comes
    /// from the events, never from this number.
    Exited { code: Option<i32> },
    /// Someone asked the run to stop, and it did.
    Cancelled,
}

/// Runs `command`, calling `on_event` for every line of output as it arrives.
///
/// Blocking: the caller decides which executor runs it (ADR-0007), and in the app that is
/// `cx.background_spawn`.
///
/// Errors only when the run could not be *started* — a missing binary, a root that is not a
/// directory. A suite with failing tests is a successful run of the runner, and comes back
/// as `Ok(Outcome::Exited { code: Some(1) })` with the failures in the events. Conflating
/// the two would make "your tests failed" and "we could not run your tests" the same
/// message, and they need different reactions from the user.
pub fn run(
    command: &Command,
    cancel: &CancelFlag,
    mut on_event: impl FnMut(Event),
) -> Result<Outcome> {
    if !command.root.is_dir() {
        bail!("project root {} is not a directory", command.root.display());
    }
    // Checked before spawning so a missing runner is a clear message rather than an
    // OS-level "No such file or directory" (§24: not having a test framework installed is
    // an ordinary, recoverable situation).
    if !command.program.is_file() {
        bail!("{} is not installed", command.program.display());
    }

    let mut spawned = StdCommand::new(&command.program);
    spawned
        .args(&command.args)
        .current_dir(&command.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Merged into stdout rather than dropped: a runner that dies on a PHP fatal error
        // prints it here, and that text is the only explanation the user will get. It
        // reaches them as `Event::Unparsed`, which is exactly what that variant is for.
        .stderr(Stdio::piped());
    // The runner leads its own process group, so cancellation can kill the whole tree.
    // A test runner is never just one process — `sh` runs `pest` runs `php` — and
    // killing only the direct child leaves the suite running *and* holding the stdout
    // and stderr pipes open, which keeps the reader threads blocked for as long as the
    // orphans live. That was a real 30-second "cancel" on CI, won locally only by the
    // kill racing the shell's fork.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        spawned.process_group(0);
    }
    let mut child = spawned
        .spawn()
        .with_context(|| format!("could not start {}", command.program.display()))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // stderr on its own thread: a child that fills the stderr pipe while we read stdout
    // would block forever on a pipe nobody drains, and that is a hang, not a slow test run.
    let errors = std::thread::Builder::new()
        .name("elle-test-runner-stderr".into())
        .spawn(move || {
            let mut collected = Vec::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                collected.push(line);
            }
            collected
        })
        .context("spawning the stderr reader")?;

    // A thread that watches the flag and kills the child, while the reader is blocked.
    //
    // Checking the flag between lines is not enough on its own, and the test that proves it
    // is `a_cancelled_run_stops_the_child_rather_than_waiting_for_it`: a suite that prints
    // one line and then blocks — on a slow test, a database connection, a `sleep` — leaves
    // `read_line` parked with no line to return, so the loop never comes back around to
    // look at the flag. Cancelling would then take as long as the suite, which is not
    // cancelling at all. That test failed for exactly this reason before this thread
    // existed, and took the full 30 seconds it was written to catch.
    //
    // Killing the child is also what *unblocks* the read: the write end of the pipe closes
    // and `read_line` returns 0. So this thread is both halves of ADR-0007's cancellation —
    // stopping the work, and releasing whoever was waiting on it.
    //
    // `Child` is shared rather than moved because both this thread and the code after the
    // read loop need it: one to kill, the other to reap and read the exit status.
    let child = Arc::new(Mutex::new(child));
    let watcher_child = Arc::clone(&child);
    let watcher_cancel = cancel.clone();
    let (done_sender, done_receiver) = std::sync::mpsc::channel::<()>();
    let watcher = std::thread::Builder::new()
        .name("elle-test-runner-cancel".into())
        .spawn(move || {
            // Wakes when the run finishes (the sender is dropped or sends) or when the
            // wait times out, whichever comes first. The timeout is what bounds how long a
            // cancel takes to be noticed; it is not a poll of the *output*, which still
            // arrives by blocking read. A thread that exists only while a suite is running
            // costs nothing at idle, which is what the footprint gate measures.
            loop {
                match done_receiver.recv_timeout(CANCEL_LATENCY) {
                    // The run ended on its own.
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if watcher_cancel.is_cancelled() {
                            if let Ok(mut child) = watcher_child.lock() {
                                kill_tree(&mut child);
                            }
                            return true;
                        }
                    }
                }
            }
        })
        .context("spawning the cancellation watcher")?;

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        if cancel.is_cancelled() {
            break;
        }
        line.clear();
        // Blocks until the runner produces a line. No timer, no sleep: this is the whole
        // of "read output as it arrives".
        match reader.read_line(&mut line) {
            // EOF. Either the runner finished, or the watcher killed it under us.
            Ok(0) => break,
            Ok(_) => {
                if let Some(event) = Event::parse(&line) {
                    on_event(event);
                }
            }
            // The read failed, which is also what a killed child looks like from inside a
            // blocking read.
            Err(_) => break,
        }
    }

    // Stop the watcher and find out whether it was the one that ended the run.
    let _ = done_sender.send(());
    let killed_by_watcher = watcher.join().unwrap_or(false);

    if cancel.is_cancelled() || killed_by_watcher {
        // The watcher may have killed it already; doing it again is harmless, and it is
        // what handles a run cancelled before the watcher noticed.
        let mut child = child.lock().expect("the child mutex");
        kill_tree(&mut child);
        let _ = child.wait();
        let _ = errors.join();
        return Ok(Outcome::Cancelled);
    }

    let status = {
        let mut child = child.lock().expect("the child mutex");
        child.wait().context("waiting for the test runner")?
    };
    // Whatever the runner said on stderr reaches the user verbatim. Joined after `wait` so
    // the pipe is closed and the thread has finished.
    if let Ok(collected) = errors.join() {
        for line in collected {
            if !line.trim().is_empty() {
                on_event(Event::Unparsed { line });
            }
        }
    }

    Ok(Outcome::Exited { code: status.code() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// §24. Asking to run tests in a folder with no runner is an error message, never a
    /// panic and never a hang.
    #[test]
    fn running_without_an_installed_binary_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let command = Command {
            program: dir.path().join("vendor/bin/pest"),
            args: Vec::new(),
            root: dir.path().to_path_buf(),
        };

        let error = run(&command, &CancelFlag::new(), |_| {}).expect_err("must not run");
        assert!(error.to_string().contains("not installed"), "{error:#}");
    }

    #[test]
    fn a_root_that_is_not_a_directory_is_an_error() {
        let command = Command {
            program: PathBuf::from("/bin/echo"),
            args: Vec::new(),
            root: PathBuf::from("/definitely/not/here"),
        };

        assert!(run(&command, &CancelFlag::new(), |_| {}).is_err());
    }

    /// Output reaches the caller line by line, and the exit code comes back as the runner
    /// gave it. `/bin/sh` stands in for a test runner so this test needs no PHP.
    #[test]
    fn output_is_streamed_and_the_exit_code_is_reported() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let command = Command {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_string(),
                "echo \"##teamcity[testStarted name='a' flowId='1']\"; \
                 echo \"##teamcity[testFinished name='a' duration='3' flowId='1']\"; exit 1"
                    .to_string(),
            ],
            root: dir.path().to_path_buf(),
        };

        let mut events = Vec::new();
        let outcome = run(&command, &CancelFlag::new(), |event| events.push(event)).expect("a run");

        assert_eq!(outcome, Outcome::Exited { code: Some(1) });
        assert_eq!(
            events,
            vec![
                Event::Started { name: "a".to_string() },
                Event::Finished { name: "a".to_string(), duration_ms: Some(3) },
            ]
        );
    }

    /// A runner that dies before emitting anything parseable still tells the user
    /// something. The text is preserved rather than swallowed.
    #[test]
    fn a_runner_that_only_prints_an_error_reports_it_verbatim() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let command = Command {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "echo 'PHP Fatal error: boom' >&2; exit 255".to_string()],
            root: dir.path().to_path_buf(),
        };

        let mut events = Vec::new();
        let outcome = run(&command, &CancelFlag::new(), |event| events.push(event)).expect("a run");

        assert_eq!(outcome, Outcome::Exited { code: Some(255) });
        assert_eq!(events, vec![Event::Unparsed { line: "PHP Fatal error: boom".to_string() }]);
    }

    /// A cancelled run stops, and says it was cancelled rather than reporting an exit code
    /// that would look like a verdict on the suite.
    ///
    /// The child here prints one line and then sleeps for 30 seconds — the shape of a real
    /// suite that is blocked on a slow test or a database. If cancelling only set a flag
    /// the read loop checks *between* lines, this would take the full 30 seconds, because
    /// `read_line` is parked with no line coming. It did exactly that before the watcher
    /// thread existed. The elapsed-time assertion is the whole point of the test.
    #[test]
    fn a_cancelled_run_stops_the_child_rather_than_waiting_for_it() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let command = Command {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_string(),
                // The sleep goes into the background *first*, then the line that triggers
                // the cancel, then `wait` keeps sh alive. This guarantees the grandchild
                // exists before the kill — the shape that made the old direct-child kill
                // fail on CI: `sleep` survived `sh` and held the output pipes open for
                // the full 30 s. With `sleep` last, the kill only won by racing sh's fork.
                "sleep 30 & echo \"##teamcity[testStarted name='a' flowId='1']\"; wait".to_string(),
            ],
            root: dir.path().to_path_buf(),
        };

        let cancel = CancelFlag::new();
        let started = std::time::Instant::now();
        let outcome = run(&command, &cancel, |event| {
            // Cancel as soon as the run proves it is alive, from inside the read loop.
            if matches!(event, Event::Started { .. }) {
                cancel.cancel();
            }
        })
        .expect("a run");

        assert_eq!(outcome, Outcome::Cancelled);
        // Generous next to a 30s sleep and tight enough that a regression to
        // "check the flag between lines" fails here rather than merely running slowly.
        assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
    }

    /// Cancelling before the run starts stops it without spawning work that is already
    /// obsolete.
    #[test]
    fn a_run_cancelled_before_it_starts_produces_no_events() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let command = Command {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "echo hello; sleep 30".to_string()],
            root: dir.path().to_path_buf(),
        };

        let cancel = CancelFlag::new();
        cancel.cancel();

        let mut events = Vec::new();
        let outcome = run(&command, &cancel, |event| events.push(event)).expect("a run");

        assert_eq!(outcome, Outcome::Cancelled);
        assert!(events.is_empty(), "{events:?}");
    }
}
