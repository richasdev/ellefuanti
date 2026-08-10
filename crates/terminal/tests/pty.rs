//! End-to-end tests against a real PTY and a real shell.
//!
//! **No sleeps.** A test that sleeps 200ms and then asserts on shell output is a test that
//! passes on this machine and fails on a loaded CI box. Everything here polls a condition
//! with a deadline instead: the fast path returns as soon as the output lands, and the
//! failure path reports what the screen actually held rather than timing out blind.

use std::path::Path;
use std::time::{Duration, Instant};

use elle_terminal::{Session, SessionId, SessionStatus, TerminalManager};

/// Upper bound for any single wait. Generous, because it is only ever reached on failure —
/// a passing assertion returns in milliseconds.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Poll interval. Short enough to keep the tests fast, long enough not to spin a core.
const POLL: Duration = Duration::from_millis(5);

/// A shell with no rc files, so the prompt and output do not depend on the developer's
/// dotfiles. `--norc` matters: a `.bashrc` printing a banner would appear on the grid.
fn test_shell() -> &'static str {
    "/bin/sh"
}

/// Polls `condition` until it returns true or the deadline passes.
fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// Polls the screen until it satisfies `predicate`, then returns it.
///
/// Panics with the full screen contents on timeout, which is the difference between
/// debugging this in one run and in five.
fn wait_for_screen(
    session: &Session,
    what: &str,
    mut predicate: impl FnMut(&str) -> bool,
) -> String {
    let mut last = String::new();
    let found = wait_until(|| {
        last = session.snapshot().to_string_trimmed();
        predicate(&last)
    });
    assert!(found, "timed out waiting for {what}; screen held:\n{last}");
    last
}

/// Polls until `needle` appears at least `count` times.
///
/// The count is the whole point. A PTY echoes what is typed, so `echo marker` puts
/// "marker" on screen *as input* long before the shell has run anything. Waiting for a
/// single occurrence returns immediately and asserts nothing about execution; waiting for
/// two is what distinguishes "the shell echoed my keystrokes" from "the shell ran it".
fn wait_for_occurrences(session: &Session, needle: &str, count: usize) -> String {
    wait_for_screen(session, &format!("{count}x {needle:?}"), |screen| {
        screen.matches(needle).count() >= count
    })
}

/// Polls until `needle` appears at all. Only correct for text the shell *outputs* that
/// cannot also appear as an echo of the input — never for a marker inside a command.
fn wait_for_text(session: &Session, needle: &str) -> String {
    wait_for_screen(session, &format!("{needle:?}"), |screen| screen.contains(needle))
}

/// Spawns a session, retrying while the OS is temporarily out of PTYs.
///
/// `cargo test` runs these in parallel and every one of them allocates a real PTY, which
/// on macOS is a finite global resource: once the harness has enough tests in flight,
/// `openpty` starts failing with `Os { code: -6 }`. That is the machine being saturated,
/// not the code under test being wrong — the same test passes every time in isolation —
/// so retrying on that failure is what makes this suite honest rather than flaky.
///
/// ponytail: a retry loop, not a global semaphore limiting concurrent PTYs across tests.
/// A semaphore would serialise the suite to eliminate the contention entirely; the retry
/// keeps the tests parallel and simply waits out a transient limit. Swap it if the retries
/// start dominating the runtime.
fn spawn(rows: u16, cols: u16) -> Session {
    spawn_in(SessionId(1), None, rows, cols)
}

/// [`spawn`] with an explicit id and cwd, for the tests that need one.
///
/// Every direct `Session::spawn` in this file must go through here. Two of them did not,
/// and those two — the cwd test and the fd-leak check — were exactly the ones reported as
/// flaky under parallel load (#43): they took the raw call, so a transient `openpty`
/// failure was a test failure rather than a retry.
fn spawn_in(id: SessionId, cwd: Option<&Path>, rows: u16, cols: u16) -> Session {
    let deadline = Instant::now() + TIMEOUT;
    let mut last_error = None;

    while Instant::now() < deadline {
        match Session::spawn(id, cwd, Some(test_shell()), rows, cols) {
            Ok(session) => return session,
            Err(err) => {
                last_error = Some(format!("{err:#}"));
                std::thread::sleep(POLL);
            }
        }
    }

    panic!(
        "could not open a pty within {TIMEOUT:?}; last error: {}",
        last_error.unwrap_or_else(|| "none recorded".into())
    );
}

/// `TerminalManager::open_with_shell` with the same PTY-exhaustion retry as [`spawn`].
///
/// The manager allocates a PTY too, so it hits the same finite global limit under a
/// parallel test run. An earlier fix covered only the direct `Session::spawn` path, which
/// left the tests that open several sessions at once — the ones most likely to exhaust the
/// table — still able to fail spuriously.
fn open_session(manager: &mut TerminalManager) -> SessionId {
    let deadline = Instant::now() + TIMEOUT;
    let mut last_error = None;

    while Instant::now() < deadline {
        match manager.open_with_shell(Some(test_shell())) {
            Ok(id) => return id,
            Err(err) => {
                last_error = Some(format!("{err:#}"));
                std::thread::sleep(POLL);
            }
        }
    }

    panic!(
        "could not open a session within {TIMEOUT:?}; last error: {}",
        last_error.unwrap_or_else(|| "none recorded".into())
    );
}

#[test]
fn spawns_a_shell_and_reports_it_running() {
    let session = spawn(24, 80);
    assert_eq!(session.status(), SessionStatus::Running);
    assert_eq!(session.size(), (24, 80));

    // The grid exists and has the requested shape before any output arrives.
    let snapshot = session.snapshot();
    assert_eq!(snapshot.lines.len(), 24);
    assert_eq!(snapshot.columns, 80);
}

#[test]
fn writes_a_command_and_reads_its_output() {
    let mut session = spawn(24, 80);

    // A distinctive string so a prompt or an echo of the command itself cannot be
    // mistaken for the output being asserted on.
    session.write_str("echo elle-marker-4711\n").unwrap();

    // Twice: once as the PTY's echo of the keystrokes, once as the command's own output.
    // Waiting for both is what proves the shell *executed* rather than merely echoed.
    let screen = wait_for_occurrences(&session, "elle-marker-4711", 2);
    assert!(screen.matches("elle-marker-4711").count() >= 2, "screen:\n{screen}");
}

#[test]
fn parses_ansi_colour_into_cell_attributes() {
    let mut session = spawn(24, 80);

    // printf, not echo -e: /bin/sh's echo does not portably interpret escapes.
    session.write_str("printf '\\033[31;1mREDBOLD\\033[0m\\n'\n").unwrap();

    // The command text itself contains "REDBOLD", so wait for a *styled* cell rather than
    // for the word — the echoed input is unstyled and would match too early.
    let mut found = None;
    let ok = wait_until(|| {
        let snapshot = session.snapshot();
        found = snapshot.lines.iter().flatten().find(|cell| cell.c == 'R' && cell.bold).copied();
        found.is_some()
    });
    assert!(
        ok,
        "timed out waiting for a bold R; screen held:\n{}",
        session.snapshot().to_string_trimmed()
    );
    let cell = found.unwrap();

    assert!(cell.bold, "SGR 1 must set bold");
    // Bold red promotes to the bright slot (9), which is the historical behaviour.
    assert_eq!(cell.fg, elle_terminal::CellColor::Ansi(9));
}

#[test]
fn the_generation_counter_moves_only_when_output_arrives() {
    let mut session = spawn(24, 80);

    // Wait for the shell to settle, so the prompt's own output is already counted.
    session.write_str("echo settled\n").unwrap();
    wait_for_occurrences(&session, "settled", 2);

    let before = session.generation();
    session.write_str("echo second\n").unwrap();
    wait_for_occurrences(&session, "second", 2);

    assert!(session.generation() > before, "a read must bump the generation");
}

#[test]
fn resize_updates_both_the_grid_and_the_shell() {
    let mut session = spawn(24, 80);

    session.resize(30, 100).unwrap();
    assert_eq!(session.size(), (30, 100));

    let snapshot = session.snapshot();
    assert_eq!(snapshot.columns, 100);
    assert_eq!(snapshot.lines.len(), 30);

    // The kernel must have been told too, or the shell keeps wrapping at 80. `stty size`
    // reads the winsize straight from the tty, so this asserts the syscall landed rather
    // than just our own bookkeeping.
    session.write_str("stty size\n").unwrap();
    wait_for_text(&session, "30 100");
}

#[test]
fn resize_to_zero_is_clamped_rather_than_rejected() {
    let mut session = spawn(24, 80);
    // A panel laid out at zero height on the first frame is normal; it must not error or
    // produce a zero-column grid, which would index out of bounds.
    session.resize(0, 0).unwrap();

    let (rows, cols) = session.size();
    assert!(rows >= 1 && cols >= 1, "size clamped to at least 1x1, got {rows}x{cols}");
    assert!(!session.snapshot().lines.is_empty());
}

#[test]
fn scrollback_retains_lines_that_scrolled_off() {
    let mut session = spawn(10, 80);

    // seq writes far more lines than the 10-row viewport, so early lines must have gone
    // into history rather than being discarded. Waiting on history_size rather than on
    // the text: "200" appears in the echoed command before a single line has scrolled.
    session.write_str("seq 1 200\n").unwrap();
    let filled = wait_until(|| session.snapshot().history_size > 0);
    assert!(
        filled,
        "lines that scrolled off must be in scrollback; screen held:\n{}",
        session.snapshot().to_string_trimmed()
    );

    let live = session.snapshot();
    assert!(live.history_size > 0, "lines that scrolled off must be in scrollback");
    assert_eq!(live.display_offset, 0, "the viewport starts pinned to the bottom");

    // Scroll up and confirm we see earlier output that is no longer on the live screen.
    session.scroll(50);
    let scrolled = session.snapshot();
    assert!(scrolled.display_offset > 0, "scrolling up must move the viewport");
    assert!(scrolled.cursor.is_none(), "a scrolled-back viewport has no live cursor to draw");
    assert_ne!(scrolled.to_string_trimmed(), live.to_string_trimmed());

    session.scroll_to_bottom();
    assert_eq!(session.snapshot().display_offset, 0);
}

#[test]
fn multiple_sessions_are_independent() {
    let mut manager = TerminalManager::new();
    let first = open_session(&mut manager);
    let second = open_session(&mut manager);
    let third = open_session(&mut manager);
    assert_eq!(manager.len(), 3);

    // Each gets a marker only it should ever show.
    manager.get_mut(first).unwrap().write_str("echo only-in-first\n").unwrap();
    manager.get_mut(second).unwrap().write_str("echo only-in-second\n").unwrap();

    wait_for_occurrences(manager.get_mut(first).unwrap(), "only-in-first", 2);
    wait_for_occurrences(manager.get_mut(second).unwrap(), "only-in-second", 2);

    let first_screen = manager.get_mut(first).unwrap().snapshot().to_string_trimmed();
    let second_screen = manager.get_mut(second).unwrap().snapshot().to_string_trimmed();
    let third_screen = manager.get_mut(third).unwrap().snapshot().to_string_trimmed();

    // The point of the test: no crosstalk between the grids.
    assert!(!first_screen.contains("only-in-second"));
    assert!(!second_screen.contains("only-in-first"));
    assert!(!third_screen.contains("only-in-first"));
    assert!(!third_screen.contains("only-in-second"));
}

#[test]
fn sessions_resize_independently_of_each_other() {
    let mut manager = TerminalManager::new();
    let first = open_session(&mut manager);
    let second = open_session(&mut manager);

    manager.resize_all(20, 60);
    for session in manager.sessions() {
        assert_eq!(session.size(), (20, 60));
    }

    // Then one alone, to prove resize is per-session and not global state.
    manager.get_mut(first).unwrap().resize(40, 120).unwrap();
    assert_eq!(manager.get_mut(first).unwrap().size(), (40, 120));
    assert_eq!(manager.get_mut(second).unwrap().size(), (20, 60));
}

#[test]
fn an_exiting_shell_is_reported_not_fatal() {
    let mut session = spawn(24, 80);
    session.write_str("exit 7\n").unwrap();

    // The child-waiter thread records the status; poll for it rather than sleeping.
    let observed = wait_until(|| !session.status().is_running());
    assert!(observed, "the shell's exit must be observed, got {:?}", session.status());

    match session.status() {
        SessionStatus::Exited { code } => {
            // Some shells report the code, some report a signal; either way it must not
            // still claim to be running. The code is asserted when it is available.
            if let Some(code) = code {
                assert_eq!(code, 7, "the shell's own exit code should be preserved");
            }
        }
        other => panic!("expected Exited, got {other:?}"),
    }

    // §24: the session is dead but the object is still safe to use. None of this panics.
    let _ = session.snapshot();
    let _ = session.write_str("echo after-exit\n");
    let _ = session.resize(30, 90);
}

#[test]
fn writing_to_a_dead_shell_returns_an_error_rather_than_panicking() {
    let mut session = spawn(24, 80);
    session.write_str("exit 0\n").unwrap();
    assert!(wait_until(|| !session.status().is_running()));

    // The write may succeed (the buffer accepts it) or fail (EPIPE); both are fine.
    // What must never happen is a panic taking the app down with it.
    for _ in 0..64 {
        let _ = session.write_str("echo x\n");
    }
}

#[test]
fn a_pty_that_cannot_spawn_reports_an_error() {
    let result =
        Session::spawn(SessionId(1), None, Some("/nonexistent/definitely-not-a-shell"), 24, 80);

    // Matched rather than `expect_err`, which would require `Session: Debug` and so a
    // derive that exposes the crate's internals purely to satisfy a test.
    let Err(err) = result else {
        panic!("spawning a missing program must fail");
    };
    // The message reaches the panel, so it must name what went wrong.
    let message = format!("{err:#}");
    assert!(!message.is_empty(), "the error must carry a message for the panel");
}

#[test]
fn a_session_starts_in_the_requested_directory() {
    let dir = std::env::temp_dir().canonicalize().unwrap();
    let mut session = spawn_in(SessionId(1), Some(&dir), 24, 80);

    assert_eq!(session.cwd().canonicalize().unwrap(), dir);

    // And the shell agrees, which is the part that actually matters to the user.
    session.write_str("pwd\n").unwrap();

    let expected = dir.to_string_lossy().to_string();
    let resolved = resolve_symlink(&dir);
    // Waiting for the path itself, not for "/": the prompt alone can contain a slash.
    let screen = wait_for_screen(&session, "pwd output", |screen| {
        screen.contains(&expected) || screen.contains(&resolved)
    });
    assert!(
        screen.contains(&expected) || screen.contains(&resolved),
        "pwd should report {expected}; screen:\n{screen}"
    );
}

/// /tmp is a symlink to /private/tmp on macOS, so `pwd` may print either form.
fn resolve_symlink(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

#[test]
fn dropping_a_session_kills_the_shell_and_leaves_no_zombie() {
    let pid = {
        let mut session = spawn(24, 80);

        // $$ is the shell's own pid. Reading it back proves the process exists before the
        // drop, so the post-drop check is measuring a real transition.
        session.write_str("echo pid=$$\n").unwrap();

        // The literal "pid=$$" is echoed first; only the *expanded* line parses as a
        // number, so the parse itself is the wait condition.
        let mut pid = None;
        let ok = wait_until(|| {
            pid = read_pid(&session.snapshot().to_string_trimmed());
            pid.is_some()
        });
        assert!(
            ok,
            "could not read the shell pid; screen held:\n{}",
            session.snapshot().to_string_trimmed()
        );
        let pid = pid.unwrap();

        assert!(process_exists(pid), "the shell should be alive before the drop");
        pid
        // Drop runs here: it must kill the child and join both threads.
    };

    // The kill is asynchronous from this thread's point of view, so poll rather than
    // assuming the process is reaped the instant drop returns.
    let gone = wait_until(|| !process_exists(pid));
    assert!(gone, "the shell (pid {pid}) outlived its session");
}

/// Extracts the pid from an expanded `pid=NNN` line, ignoring the echoed `pid=$$`.
fn read_pid(screen: &str) -> Option<i32> {
    screen
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pid="))
        .find_map(|value| value.trim().parse().ok())
}

/// True while `pid` names a live *or* zombie process.
///
/// `kill(pid, 0)` is the portable liveness check. It still succeeds for a zombie, which is
/// what makes this a real leak test: a child that was killed but never reaped stays
/// visible here, so the assertion above fails unless `Drop` actually waited on it.
fn process_exists(pid: i32) -> bool {
    // ponytail: shells out to `kill -0` because a libc dependency in a domain crate would
    // be the first unsafe/platform dependency in this layer, and the architecture test
    // forbids `target_os` here. Swap for `libc::kill` if this ever runs on Windows, where
    // the whole check needs a different implementation anyway.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn closing_sessions_does_not_leak_file_descriptors() {
    // Each session holds a master fd plus a cloned reader fd. A leak shows up as the
    // process's open-fd count climbing across open/close cycles.
    //
    // Counting fds directly rather than spawning until something breaks: exhausting the
    // fd limit would also exhaust the *system-wide* pty device table, which is shared with
    // every other test running in parallel — that made this test fail intermittently in
    // whichever test happened to spawn next, rather than in the one at fault.

    // A few cycles first, so any one-off allocation (thread pool, lazy statics) has
    // already happened and does not read as a leak.
    for i in 0..3 {
        drop(spawn_for_leak_check(i));
    }

    let baseline = open_fd_count();

    for i in 3..13 {
        drop(spawn_for_leak_check(i));
    }

    // Wait for the count to settle rather than sampling immediately.
    //
    // `Drop` detaches the child-waiter thread (deliberately — joining it blocked the UI for
    // ~600 ms while `/bin/sh` died on SIGHUP), so descriptors come back on that thread's
    // schedule, not at the drop site. Sampling right away measures teardown in flight: this
    // passed 5/5 locally and failed on a slower CI runner at +6, which is a fact about
    // scheduling rather than about `Drop`.
    //
    // Polling for the settled value is what makes the test measure a *leak*. Simply widening
    // the threshold would have made CI green while blinding the test to a real regression —
    // a leak of two fds per session is +20, and this still catches that, because a genuine
    // leak never settles no matter how long we wait.
    let deadline = Instant::now() + TIMEOUT;
    let allowance = baseline + 4;
    let mut after = open_fd_count();
    while after > allowance && Instant::now() < deadline {
        std::thread::sleep(POLL);
        after = open_fd_count();
    }

    assert!(
        after <= allowance,
        "fd count settled at {after} from a baseline of {baseline} across 10 open/close \
         cycles, which means Drop is not returning the pty descriptors"
    );
}

fn spawn_for_leak_check(id: u64) -> Session {
    spawn_in(SessionId(id), None, 24, 80)
}

/// Number of file descriptors this process has open.
///
/// ponytail: shells out to `lsof` because counting fds portably needs either libc or
/// /dev/fd, and a libc dependency in a domain crate would be the first platform dependency
/// in this layer (the architecture test forbids `target_os` here). Reading /dev/fd would be
/// cheaper; `lsof -p` is the version that is obviously correct.
fn open_fd_count() -> usize {
    let pid = std::process::id();
    let output = std::process::Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .expect("lsof should be available on macOS and Linux");

    String::from_utf8_lossy(&output.stdout).lines().skip(1).count()
}

#[test]
fn closing_one_of_several_sessions_leaves_the_others_working() {
    let mut manager = TerminalManager::new();
    let first = open_session(&mut manager);
    let second = open_session(&mut manager);

    assert!(manager.close(first));
    assert_eq!(manager.len(), 1);

    // The survivor is still fully functional, not collaterally torn down.
    let session = manager.get_mut(second).unwrap();
    session.write_str("echo survivor\n").unwrap();
    wait_for_occurrences(session, "survivor", 2);
    assert!(session.status().is_running());
}

#[test]
fn a_session_survives_a_flood_of_output_without_blocking_the_reader() {
    let mut session = spawn(24, 80);

    // Far more output than the 64 KiB read chunk, to exercise the loop rather than a
    // single read, and to prove a busy PTY does not deadlock against snapshot's lock.
    session.write_str("seq 1 20000\n").unwrap();

    // Snapshot repeatedly while output is still streaming: this is what the UI does every
    // frame, and it must never block for long or deadlock against the reader thread.
    let deadline = Instant::now() + TIMEOUT;
    let mut saw_end = false;
    while Instant::now() < deadline {
        let snapshot = session.snapshot();
        assert_eq!(snapshot.lines.len(), 24, "the grid keeps its shape under load");
        // The final number on a line of its own — the echoed "seq 1 20000" contains
        // "20000" too, so a substring match would pass before any output arrived.
        if snapshot.to_text().iter().any(|line| line.trim() == "20000") {
            saw_end = true;
            break;
        }
    }

    assert!(saw_end, "the tail of a large output stream should arrive");
    assert!(session.status().is_running(), "a flood must not kill the session");
}

/// DECCKM, through the real parser rather than a hand-fed byte string.
///
/// The unit test in `keys.rs` proves the *encoder* switches to SS3; this proves the flag
/// actually reaches it. A program sets application cursor mode by emitting `ESC [ ? 1 h`,
/// which the shell here does with `printf` — so the path under test is PTY -> alacritty's
/// parser -> `Term::mode()` -> `Session::flags()`, which is where the bug lived.
#[test]
fn application_cursor_mode_reaches_the_key_encoder() {
    let mut session = spawn(24, 80);

    // Not set until something asks for it.
    assert!(!session.flags().application_cursor, "a fresh shell is in normal cursor mode");

    session.write_str("printf '\\033[?1h'; echo ready\n").unwrap();
    wait_for_occurrences(&session, "ready", 2);

    let flags = session.flags();
    assert!(flags.application_cursor, "DECCKM set by the program must be visible to the view");
    assert_eq!(
        elle_terminal::encode(&elle_terminal::Key::Up, elle_terminal::Modifiers::NONE, flags)
            .unwrap(),
        b"\x1bOA",
        "the arrow keys must follow the mode the program asked for"
    );

    // And it turns back off, so a program exiting does not leave every later arrow wrong.
    session.write_str("printf '\\033[?1l'; echo done\n").unwrap();
    wait_for_occurrences(&session, "done", 2);
    assert!(!session.flags().application_cursor);
}

/// The other half of `Term::mode()` this crate reads: bracketed paste.
#[test]
fn bracketed_paste_mode_reaches_the_paste_encoder() {
    let mut session = spawn(24, 80);
    assert!(!session.flags().bracketed_paste, "/bin/sh does not request it on its own");

    session.write_str("printf '\\033[?2004h'; echo armed\n").unwrap();
    wait_for_occurrences(&session, "armed", 2);

    let flags = session.flags();
    assert!(flags.bracketed_paste);
    // A two-line paste must arrive wrapped, or the shell runs the first line on its own.
    let bytes = elle_terminal::encode_paste("one\ntwo", flags);
    assert!(bytes.starts_with(b"\x1b[200~"));
    assert!(bytes.ends_with(b"\x1b[201~"));
}

/// Selection and copy against real shell output, which is the case the feature exists for.
///
/// The unit tests in `selection.rs` work on a hand-built snapshot; this proves the same
/// maths lines up with what alacritty actually puts in the grid — including where the
/// cursor and the prompt sit, which a synthetic snapshot cannot get wrong.
#[test]
fn a_selection_over_real_output_copies_exactly_that_text() {
    use elle_terminal::{GridGeometry, Selection, SelectionMode, SelectionPoint};

    let mut session = spawn(24, 80);
    // Portuguese, because a mid-codepoint selection is a panic in a debug build.
    session.write_str("printf 'ação-marker\\n'\n").unwrap();
    wait_for_occurrences(&session, "ação-marker", 2);

    let snapshot = session.snapshot();
    let geometry = GridGeometry::of(&snapshot);

    // Find the row the *output* landed on — the last one holding the marker, since the
    // echoed command line holds it too.
    let row = snapshot
        .to_text()
        .iter()
        .rposition(|line| line.contains("ação-marker"))
        .expect("the marker must be on screen");
    let column = snapshot.to_text()[row].find("ação-marker").unwrap();
    // `find` gives a byte offset; the grid is addressed in cells, and the two differ the
    // moment a multibyte character sits to the left. Counting chars is the correct one.
    let column = snapshot.to_text()[row][..column].chars().count();

    // Double-click anywhere inside it: `-` is not a word separator, so the whole token
    // comes out.
    let selection = Selection::new(
        SelectionPoint::new(geometry.top_line + row, column + 2),
        SelectionMode::Word,
    );
    assert_eq!(
        elle_terminal::selected_text(&selection, &snapshot, geometry),
        "ação-marker",
        "a double-click on real output must copy the whole token, bytes intact"
    );
}
