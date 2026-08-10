//! A generic Language Server Protocol client.
//!
//! # What this crate is, and what it deliberately is not
//!
//! It is an LSP client. It is **not** a PHP integration, and it is not an Intelephense
//! integration. That distinction is the whole point (RISKS.md #2): Intelephense is
//! proprietary and licence-gated, so the moment PHP intelligence starts depending on
//! one of its quirks, the ability to swap in phpactor or a first-party engine is gone
//! on paper only.
//!
//! The discipline that keeps this real is mechanical: a backend is described entirely
//! by a [`ServerConfig`] value — a command, arguments, environment, and an opaque
//! `initializationOptions` blob. There is no `enum Backend`, no per-server branch, and
//! no PHP-specific behaviour anywhere in this crate. Even the completion trigger
//! characters are read from the server's own capabilities rather than hardcoded, since
//! `$` and `->` are PHP's answer and not this crate's business. A test in
//! `tests/substitutability.rs` asserts the absence of backend names in the source, so
//! the rule fails a build rather than eroding on a deadline.
//!
//! # No gpui, no tokio
//!
//! Plain Rust, so it stays testable without a window (ADR-0004), and blocking rather
//! than async, so it does not drag in a second runtime (ADR-0007). Every call here
//! blocks the calling thread; the app wraps them in `cx.background_spawn` exactly as it
//! already does for `elle-workspace`. The reader thread this crate does spawn is an
//! implementation detail of "a blocking read has to live somewhere", not a runtime.
//!
//! # Offsets
//!
//! This codebase counts **bytes**; LSP counts **UTF-16 code units** by default. Every
//! conversion goes through [`offset`], which is tested against Portuguese and
//! astral-plane fixtures rather than trusted — see that module's documentation for why
//! an ASCII test proves nothing here.
//!
//! # Fault isolation
//!
//! §24: a server that crashes, hangs, or was never installed must not stop the user
//! editing. Nothing here panics on server behaviour, every wait is bounded by a
//! timeout, a dead connection wakes its waiters with an error instead of blocking
//! forever, and an unparseable frame is logged and skipped rather than ending the
//! session.
//!
//! # Example
//!
//! ```no_run
//! use elle_lsp::{Client, ServerConfig};
//!
//! let config = ServerConfig::new("intelephense", "intelephense", "/srv/app")
//!     .with_args(["--stdio"])
//!     .with_language_ids(["php"]);
//!
//! // Blocking: the caller decides which executor this runs on.
//! let mut client = Client::start(&config)?;
//! let uri = elle_lsp::path_to_uri(std::path::Path::new("/srv/app/routes/web.php"))?;
//! client.did_open(uri.clone(), "php", "<?php\n")?;
//! let items = client.completion(&uri, 6)?;
//! client.stop()?;
//! # Ok::<(), anyhow::Error>(())
//! ```

mod client;
mod config;
mod connection;
mod document;
pub mod jsonrpc;
pub mod offset;
mod process;
mod transport;

pub use client::{
    Capabilities, Client, INITIALIZE_TIMEOUT, REQUEST_TIMEOUT, SHUTDOWN_TIMEOUT, ServerEvent,
};
pub use config::ServerConfig;
pub use connection::{Connection, RequestOutcome, ServerMessage};
pub use document::{SyncKind, TrackedDocument};
pub use jsonrpc::{RequestId, ResponseError};
pub use offset::{LineIndex, OffsetEncoding};
pub use process::{path_to_uri, uri_to_path};

// Re-exported so callers do not need their own `lsp-types` dependency pinned to a
// matching version — the protocol types are part of this crate's API surface.
pub use lsp_types;

/// Re-exported for the same reason as `lsp_types`: [`Client::poll_response`] and
/// [`Client::await_response`] are generic over it, so a caller writing that bound would
/// otherwise need its own `serde` dependency pinned to a matching version for a trait it
/// never names in its own types.
pub use serde::de::DeserializeOwned;
