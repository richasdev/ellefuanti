//! One terminal: a PTY, a shell running inside it, and the parsed screen.
//!
//! Threading, because it is the whole design. A [`Session`] owns a reader thread that
//! blocks on the PTY master and feeds bytes into alacritty's parser. The UI never touches
//! that thread: it calls [`Session::snapshot`], which takes the terminal lock for as long
//! as it takes to copy the visible cells, and releases it before rendering. So a shell
//! producing output at full speed slows the reader thread, not the frame.
//!
//! Everything public here is blocking and executor-agnostic (ADR-0007). `snapshot` is
//! bounded work on a lock the reader holds only in short bursts, so the app calls it
//! directly from render; `spawn` does real work and is wrapped in `background_spawn`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, StdSyncHandler};
use anyhow::{Context as _, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::grid::{Cell, CellColor, CursorPos, GridSnapshot};

/// Scrollback depth, in lines.
///
/// ponytail: a fixed depth, not a setting. 10k lines is alacritty's own default and costs
/// a few MB per session at 80 columns. Read it from a settings crate once one exists.
pub const SCROLLBACK_LINES: usize = 10_000;

/// How much the reader thread pulls from the PTY per syscall.
const READ_CHUNK: usize = 65_536;

/// Identifies a session within a [`crate::TerminalManager`].
///
/// A generational id rather than a `Vec` index: closing session 0 must not silently
/// redirect a pending write to whatever session slid into slot 0.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SessionId(pub u64);

/// Why a session is no longer usable.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SessionStatus {
    Running,
    /// The shell exited on its own (the user typed `exit`, or it crashed).
    Exited { code: Option<i32> },
    /// The PTY or the reader failed. The message is shown in the panel.
    Failed(String),
}

impl SessionStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, SessionStatus::Running)
    }
}

/// State shared with the reader thread.
///
/// Split out from `Session` so the thread holds only what it needs: it must not keep the
/// `Session` alive, or dropping a session could never join its own thread.
struct Shared {
    term: Mutex<Term<VoidListener>>,
    status: Mutex<SessionStatus>,
    /// Set on drop so the reader stops parsing and exits rather than racing teardown.
    closing: AtomicBool,
    /// Bumped on every applied read, so the UI can tell "nothing changed" from "changed
    /// back to the same thing" without diffing an entire grid every frame.
    generation: Mutex<u64>,
}

/// A live terminal session.
pub struct Session {
    id: SessionId,
    title: String,
    cwd: PathBuf,
    shared: Arc<Shared>,
    /// Writing end of the PTY. `Option` only so `Drop` can take it: dropping the writer
    /// sends EOF to the shell, which is what makes it exit promptly on close.
    writer: Option<Box<dyn Write + Send>>,
    master: Box<dyn MasterPty + Send>,
    /// Kills the child without waiting on the `Child`, which the reader may be blocked in.
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    reader_thread: Option<JoinHandle<()>>,
    child_thread: Option<JoinHandle<()>>,
    size: PtySize,
}

impl Session {
    /// Spawns a shell in a new PTY.
    ///
    /// `shell` is the program to run; `None` uses the user's login shell. Blocking: forks
    /// a process and opens a device, so call it off the UI thread.
    pub fn spawn(
        id: SessionId,
        cwd: Option<&Path>,
        shell: Option<&str>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        // Zero in either axis makes the kernel reject the winsize, and alacritty indexes
        // a zero-column grid out of bounds. Clamp rather than propagate: a panel laid out
        // at zero height during the first frame is normal, not an error worth surfacing.
        let size = PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        };

        let pty = native_pty_system()
            .openpty(size)
            .context("opening a pty")?;

        let mut command = match shell {
            Some(shell) => CommandBuilder::new(shell),
            // Reads $SHELL, falling back to the passwd entry — the login shell, not /bin/sh.
            None => CommandBuilder::new_default_prog(),
        };

        let cwd = resolve_cwd(cwd);
        command.cwd(&cwd);

        // Without TERM a shell assumes a dumb terminal and stops emitting colour at all.
        // xterm-256color is the safe lowest common denominator that still gets colour.
        command.env("TERM", "xterm-256color");

        let child = pty
            .slave
            .spawn_command(command)
            .context("spawning the shell")?;

        // The slave fd must be closed in this process, or the master never sees EOF when
        // the shell exits and the reader thread blocks forever.
        drop(pty.slave);

        let killer = child.clone_killer();

        let reader = pty
            .master
            .try_clone_reader()
            .context("cloning the pty reader")?;
        let writer = pty
            .master
            .take_writer()
            .context("taking the pty writer")?;

        let term = Term::new(
            Config { scrolling_history: SCROLLBACK_LINES, ..Config::default() },
            &TermDimensions { columns: size.cols as usize, screen_lines: size.rows as usize },
            VoidListener,
        );

        let shared = Arc::new(Shared {
            term: Mutex::new(term),
            status: Mutex::new(SessionStatus::Running),
            closing: AtomicBool::new(false),
            generation: Mutex::new(0),
        });

        let reader_thread = spawn_reader(Arc::clone(&shared), reader);
        let child_thread = spawn_child_waiter(Arc::clone(&shared), child);

        Ok(Self {
            id,
            title: default_title(&cwd),
            cwd,
            shared,
            writer: Some(writer),
            master: pty.master,
            killer,
            reader_thread: Some(reader_thread),
            child_thread: Some(child_thread),
            size,
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn size(&self) -> (u16, u16) {
        (self.size.rows, self.size.cols)
    }

    pub fn status(&self) -> SessionStatus {
        self.shared.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Counter of applied reads. The UI repaints only when this moves.
    pub fn generation(&self) -> u64 {
        *self.shared.generation.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sends bytes to the shell's stdin.
    ///
    /// Errors are returned rather than panicking: writing to a shell that just exited is
    /// an ordinary race, not a bug (§24).
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            anyhow::bail!("terminal is closed");
        };
        writer.write_all(bytes).context("writing to the pty")?;
        writer.flush().context("flushing the pty")?;
        Ok(())
    }

    /// Convenience for the common case of forwarding typed text.
    pub fn write_str(&mut self, text: &str) -> Result<()> {
        self.write(text.as_bytes())
    }

    /// Resizes both the kernel's winsize and the parsed grid.
    ///
    /// Both must move together: telling the shell the width changed while the grid still
    /// holds the old column count reflows every subsequent line at the wrong width.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.size.rows && cols == self.size.cols {
            return Ok(());
        }

        self.size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
        // The kernel first: it is the side that can fail, and a failed resize should not
        // leave the grid describing a width the shell was never told about.
        self.master.resize(self.size).context("resizing the pty")?;

        self.with_term(|term| {
            term.resize(TermDimensions { columns: cols as usize, screen_lines: rows as usize });
        });
        Ok(())
    }

    /// Copies the visible screen out from under the lock.
    pub fn snapshot(&self) -> GridSnapshot {
        let mut term = self.shared.term.lock().unwrap_or_else(|e| e.into_inner());
        snapshot_of(&mut term)
    }

    /// Scrolls the viewport by `delta` lines; positive scrolls up into history.
    pub fn scroll(&mut self, delta: i32) {
        use alacritty_terminal::grid::Scroll;
        self.with_term(|term| term.scroll_display(Scroll::Delta(delta)));
    }

    /// Pins the viewport back to the live bottom. Called on input, matching every other
    /// terminal: typing while scrolled back should show what you are typing.
    pub fn scroll_to_bottom(&mut self) {
        use alacritty_terminal::grid::Scroll;
        self.with_term(|term| term.scroll_display(Scroll::Bottom));
    }

    fn with_term<R>(&self, f: impl FnOnce(&mut Term<VoidListener>) -> R) -> R {
        let mut term = self.shared.term.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut term)
    }
}

impl Drop for Session {
    /// Teardown order matters, and each step is load-bearing:
    ///
    /// 1. Flag `closing` so the reader stops applying reads.
    /// 2. Drop the writer, sending EOF — a well-behaved shell exits on its own.
    /// 3. `kill` (SIGHUP) so a shell ignoring EOF still goes away.
    /// 4. Join the *reader*, which owns a reference to the parsed terminal and so must be
    ///    gone before it is dropped. That join is fast: killing the child closes the
    ///    master, and the blocking read returns EOF immediately.
    ///
    /// The child-waiter is deliberately **not** joined. It sits in `waitpid`, and a shell
    /// takes as long as it likes to act on SIGHUP — measured at ~600ms for `/bin/sh` on
    /// macOS, which is a visibly frozen window if a tab close waits for it. It touches
    /// only the `Arc<Shared>`, never the terminal grid, so letting it outlive the
    /// `Session` is safe: it reaps the child (no zombie) and exits on its own. The `Arc`
    /// keeps `Shared` alive exactly as long as the thread needs it.
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        drop(self.writer.take());
        let _ = self.killer.kill();

        // The reader unblocks when the master sees EOF, which the kill guarantees.
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        // Dropping the handle detaches; the thread still runs to completion and reaps.
        drop(self.child_thread.take());
    }
}

/// Drains the PTY into the parser until EOF.
fn spawn_reader(shared: Arc<Shared>, mut reader: Box<dyn Read + Send>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("elle-terminal-reader".into())
        .spawn(move || {
            // The type parameter is the synchronised-update timeout strategy; the default
            // is only a default on the struct, so `new()` cannot infer it here.
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            let mut buffer = vec![0u8; READ_CHUNK];

            loop {
                if shared.closing.load(Ordering::SeqCst) {
                    return;
                }

                match reader.read(&mut buffer) {
                    // EOF: the shell closed its end. Not an error.
                    Ok(0) => return,
                    Ok(n) => {
                        if shared.closing.load(Ordering::SeqCst) {
                            return;
                        }
                        // Scoped so the lock is released before the next blocking read;
                        // holding it across `read` would stall every frame on a quiet PTY.
                        {
                            let mut term =
                                shared.term.lock().unwrap_or_else(|e| e.into_inner());
                            parser.advance(&mut *term, &buffer[..n]);
                        }
                        let mut generation =
                            shared.generation.lock().unwrap_or_else(|e| e.into_inner());
                        *generation += 1;
                    }
                    // A closing PTY reports EIO on some platforms rather than EOF.
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        if !shared.closing.load(Ordering::SeqCst) {
                            tracing::debug!("terminal reader stopped: {err}");
                        }
                        return;
                    }
                }
            }
        })
        // A thread that cannot spawn is a resource exhaustion the app cannot recover
        // from here; the session is still constructed, so the panel reports it.
        .expect("spawning a terminal reader thread")
}

/// Waits for the shell so it is reaped rather than left a zombie, and records the status.
fn spawn_child_waiter(
    shared: Arc<Shared>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("elle-terminal-child".into())
        .spawn(move || {
            let status = match child.wait() {
                Ok(status) => {
                    let code = status.exit_code();
                    SessionStatus::Exited { code: Some(code as i32) }
                }
                Err(err) => SessionStatus::Failed(format!("waiting for the shell: {err}")),
            };

            // A session closed by the user is not a session that "exited" from the
            // panel's point of view; leave the status alone so teardown reads cleanly.
            if shared.closing.load(Ordering::SeqCst) {
                return;
            }
            *shared.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
        })
        .expect("spawning a terminal child-waiter thread")
}

/// Copies the visible cells out of a `Term`.
fn snapshot_of(term: &mut Term<VoidListener>) -> GridSnapshot {
    let grid = term.grid();
    let columns = grid.columns();
    let screen_lines = grid.screen_lines();
    let display_offset = grid.display_offset();
    let history_size = grid.history_size();

    let mut lines = Vec::with_capacity(screen_lines);
    for line in 0..screen_lines {
        let mut row = Vec::with_capacity(columns);
        for column in 0..columns {
            // display_offset shifts the window up into history; negative lines are
            // scrollback, which is exactly how alacritty indexes it.
            let point = Point::new(Line(line as i32 - display_offset as i32), Column(column));
            let cell = &grid[point];
            let flags = cell.flags;

            row.push(Cell {
                c: cell.c,
                fg: convert_color(cell.fg, flags.contains(Flags::BOLD)),
                bg: convert_color(cell.bg, false),
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                wide_spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
            });
        }
        lines.push(row);
    }

    // A scrolled-back viewport has no live cursor on screen to draw.
    let cursor = if display_offset == 0 {
        let point = term.grid().cursor.point;
        Some(CursorPos { line: point.line.0.max(0) as usize, column: point.column.0 })
    } else {
        None
    };

    GridSnapshot { lines, columns, cursor, history_size, display_offset }
}

/// Maps alacritty's colour model onto the UI's.
///
/// Bold text historically selects the bright variant of a named colour; terminals still
/// do it and shells still rely on it for readable prompts.
fn convert_color(color: Color, bold: bool) -> CellColor {
    match color {
        Color::Named(named) => convert_named(named, bold),
        Color::Spec(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => match index {
            // The first sixteen entries are the ANSI slots the theme owns.
            0..=15 => CellColor::Ansi(index),
            _ => {
                let (r, g, b) = xterm_256_to_rgb(index);
                CellColor::Rgb(r, g, b)
            }
        },
    }
}

fn convert_named(named: NamedColor, bold: bool) -> CellColor {
    use NamedColor::*;
    match named {
        Foreground => CellColor::Foreground,
        Background => CellColor::Background,
        // The cursor's own colour is the theme's business, not the program's.
        Cursor => CellColor::Foreground,

        Black => CellColor::Ansi(if bold { 8 } else { 0 }),
        Red => CellColor::Ansi(if bold { 9 } else { 1 }),
        Green => CellColor::Ansi(if bold { 10 } else { 2 }),
        Yellow => CellColor::Ansi(if bold { 11 } else { 3 }),
        Blue => CellColor::Ansi(if bold { 12 } else { 4 }),
        Magenta => CellColor::Ansi(if bold { 13 } else { 5 }),
        Cyan => CellColor::Ansi(if bold { 14 } else { 6 }),
        White => CellColor::Ansi(if bold { 15 } else { 7 }),

        BrightBlack => CellColor::Ansi(8),
        BrightRed => CellColor::Ansi(9),
        BrightGreen => CellColor::Ansi(10),
        BrightYellow => CellColor::Ansi(11),
        BrightBlue => CellColor::Ansi(12),
        BrightMagenta => CellColor::Ansi(13),
        BrightCyan => CellColor::Ansi(14),
        BrightWhite => CellColor::Ansi(15),
        BrightForeground => CellColor::Foreground,

        // Dim variants render as their normal slot.
        // ponytail: a real dim needs an alpha or a darkened blend against the background,
        // which the theme owns; map them properly when the theme grows a dim ramp.
        DimBlack => CellColor::Ansi(0),
        DimRed => CellColor::Ansi(1),
        DimGreen => CellColor::Ansi(2),
        DimYellow => CellColor::Ansi(3),
        DimBlue => CellColor::Ansi(4),
        DimMagenta => CellColor::Ansi(5),
        DimCyan => CellColor::Ansi(6),
        DimWhite => CellColor::Ansi(7),
        DimForeground => CellColor::Foreground,
    }
}

/// The xterm 256-colour cube and greyscale ramp, for indices 16..=255.
fn xterm_256_to_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        // 6x6x6 colour cube. The levels are xterm's, not an even 51-step ramp.
        16..=231 => {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let i = index as usize - 16;
            (LEVELS[(i / 36) % 6], LEVELS[(i / 6) % 6], LEVELS[i % 6])
        }
        // 24-step greyscale.
        232..=255 => {
            let level = 8 + (index as u16 - 232) * 10;
            let level = level.min(255) as u8;
            (level, level, level)
        }
        // 0..=15 are handled by the caller; returning black is unreachable in practice.
        _ => (0, 0, 0),
    }
}

/// Picks a starting directory, falling back until something exists.
///
/// A cwd that does not exist makes the spawn fail outright, so this never hands one over:
/// the workspace root if given, else the process cwd, else `/`.
fn resolve_cwd(requested: Option<&Path>) -> PathBuf {
    if let Some(path) = requested {
        if path.is_dir() {
            return path.to_path_buf();
        }
        // A file (the open document, say) means the user wants its folder.
        if let Some(parent) = path.parent() {
            if parent.is_dir() {
                return parent.to_path_buf();
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// Tab label: the directory's own name, which is what identifies it at a glance.
fn default_title(cwd: &Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.display().to_string())
}

/// A `Dimensions` we own, so construction does not depend on alacritty's `term::test`
/// helper module — that is test scaffolding and free to change between releases.
struct TermDimensions {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xterm_cube_matches_known_entries() {
        // Spot-checks against the published xterm palette.
        assert_eq!(xterm_256_to_rgb(16), (0, 0, 0));
        assert_eq!(xterm_256_to_rgb(21), (0, 0, 255));
        assert_eq!(xterm_256_to_rgb(196), (255, 0, 0));
        assert_eq!(xterm_256_to_rgb(231), (255, 255, 255));
        assert_eq!(xterm_256_to_rgb(232), (8, 8, 8));
        assert_eq!(xterm_256_to_rgb(255), (238, 238, 238));
    }

    #[test]
    fn bold_promotes_named_colours_to_their_bright_slot() {
        assert_eq!(convert_named(NamedColor::Red, false), CellColor::Ansi(1));
        assert_eq!(convert_named(NamedColor::Red, true), CellColor::Ansi(9));
        // Already-bright slots do not double-promote.
        assert_eq!(convert_named(NamedColor::BrightRed, true), CellColor::Ansi(9));
    }

    #[test]
    fn indexed_colours_below_sixteen_stay_symbolic() {
        // The theme owns these, so they must not be flattened to RGB here.
        assert_eq!(convert_color(Color::Indexed(3), false), CellColor::Ansi(3));
        assert_eq!(convert_color(Color::Indexed(200), false), CellColor::Rgb(255, 0, 215));
    }

    #[test]
    fn resolve_cwd_falls_back_to_the_parent_of_a_file() {
        let dir = std::env::temp_dir();
        let file = dir.join("elle-terminal-cwd-probe");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(resolve_cwd(Some(&file)), dir);
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn resolve_cwd_ignores_a_path_that_does_not_exist() {
        let missing = Path::new("/nonexistent-elle-terminal-dir/deeper");
        // Must not hand a bogus cwd to spawn, which would fail the whole session.
        assert!(resolve_cwd(Some(missing)).is_dir());
    }
}
