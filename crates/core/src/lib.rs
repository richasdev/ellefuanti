//! Presentation-free core: command metadata registry and shared error types.
//!
//! This crate must never depend on gpui (ADR-0004). Everything here is plain Rust
//! so it stays testable without a window and reusable from headless tools.

pub mod command;

pub use command::{BUILTIN_COMMANDS, Command, CommandId, CommandRegistry};
