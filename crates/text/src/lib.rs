//! Text storage for the editor: a rope-backed buffer with undo/redo.
//!
//! No UI, no filesystem, no syntax: this crate is pure text so it can be tested at
//! full speed without a window (ADR-0003).
//!
//! # Offsets
//!
//! **Every public offset in this crate is a byte offset.** Ropey natively indexes chars,
//! while tree-sitter and gpui's text layout both want bytes; picking one unit at the API
//! boundary and converting once, internally, is what keeps `é` from corrupting an edit.
//!
//! Offsets are clamped into the document and snapped **down** to the nearest UTF-8 char
//! boundary. Snapping rather than panicking is deliberate: a click or a column
//! calculation legitimately lands mid-codepoint, and the editor should place the cursor
//! at the start of that character instead of crashing.

mod buffer;
mod edit;

pub use buffer::{Buffer, Point, Version};
pub use edit::Edit;
