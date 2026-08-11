//! Terminal sessions: a real PTY, ANSI parsing, and a grid the UI can render.
//!
//! No gpui (ADR-0004) and no async runtime (ADR-0007). Every entry point is a blocking
//! call; the app crate wraps the slow ones in `cx.background_spawn`. Reading the PTY is
//! the exception that cannot be a blocking call the UI makes — it never ends — so each
//! session owns a reader thread and the UI polls [`Session::snapshot`] instead.
//!
//! ANSI interpretation is delegated to `alacritty_terminal` rather than hand-written. A
//! VT parser is a large, adversarial subsystem (DEC private modes, charset shifts, OSC
//! strings, wide-character and reflow rules) where being 95% correct still means a broken
//! `vim` and a corrupted `artisan tinker`. What this crate owns is the part specific to
//! this app: session lifecycle, teardown without leaks, key encoding, and the plain-data
//! snapshot that keeps gpui out of the domain layer.

mod grid;
mod keys;
mod link;
mod manager;
mod selection;
mod session;

pub use grid::{Cell, CellColor, CursorPos, GridSnapshot};
pub use keys::{Key, Modifiers, TermFlags, encode, encode_paste};
pub use link::{Link, link_at};
pub use manager::TerminalManager;
pub use selection::{
    GridGeometry, Selection, SelectionMode, SelectionPoint, cell_at, select_all, selected_columns,
    selected_text,
};
pub use session::{SCROLLBACK_LINES, Session, SessionId, SessionStatus};
