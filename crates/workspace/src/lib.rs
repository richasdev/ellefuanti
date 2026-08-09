//! Workspace: the open folder, its file tree, and file IO.
//!
//! Every function here is synchronous and blocking *on purpose*. Blocking work belongs
//! to a background executor, and this crate does not know which one — the caller wraps
//! these calls in `cx.background_spawn` (ADR-0007). Keeping the async choice out of
//! here is what makes the tree testable without a runtime.

mod file_tree;
mod fs;

pub use file_tree::{Entry, EntryKind, FileTree};
pub use fs::{ReadFile, read_file, write_file};
