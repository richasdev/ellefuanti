//! A PHP step debugger, speaking DBGp to Xdebug.
//!
//! # This is not DAP, and the issue title is wrong about that
//!
//! Issue #30 says "Xdebug debugger via DAP", and the lead it gives — that the JSON-RPC
//! transport from #19 might be reusable — is worth stating plainly as *not* what
//! happened, because the reasoning is the useful part.
//!
//! **Xdebug does not speak DAP.** It implements only DBGp, and that was checked against
//! Xdebug's own source rather than inferred: `src/debugger/` contains exactly one
//! protocol handler, `handler_dbgp.c`, and no DAP handler exists in any 3.x including
//! 3.5. What speaks DAP in a VS Code or Zed setup is `xdebug/vscode-php-debug`, a
//! separate TypeScript adapter that translates DAP to DBGp and back. Bundling it would
//! mean shipping Node and an npm package to talk to a debugger we can dial directly.
//!
//! So the choice was DBGp directly, and the LSP transport turned out not to be reusable
//! for reasons that go deeper than message format. Both are request/response over a
//! socket, and there the resemblance stops:
//!
//! | | LSP (`crates/lsp`) | DBGp (here) |
//! |---|---|---|
//! | Framing | `Content-Length` headers, JSON body | `length\0xml\0` |
//! | Payload | JSON | XML |
//! | Who connects | we spawn a child process | **PHP dials into a port we listen on** |
//! | Concurrency | replies interleave; needs a pending map | strictly serial |
//! | Lifetime | one server per project | one session **per request** |
//!
//! The third row is the one that decides it. `elle_lsp::Connection` is built around
//! owning a child's stdin and stdout and correlating out-of-order replies with a pending
//! map, a reader thread and two condition variables. None of that machinery has anything
//! to answer here: there is no child, and Xdebug's command loop reads one command and
//! answers it before reading the next, so a transaction id is a *check* rather than a
//! routing key. Reusing that `Connection` would have meant keeping its whole correlation
//! apparatus to solve a problem this protocol does not have.
//!
//! What *was* reused is the part worth reusing: the shape. [`transport`] is deliberately
//! the same design as `crates/lsp/src/transport.rs` — read by declared byte count into a
//! `Vec<u8>`, decode only once whole, test it one byte at a time — because the partial-read
//! failure modes are identical even though the framing is not.
//!
//! # No dependencies at all, and the measurement that forced it
//!
//! This crate depends on `anyhow` and `tracing` and nothing else — no XML crate, despite
//! parsing XML. That was not the first plan, and the reason it changed is worth recording
//! because the reasoning that failed is the reasoning this codebase uses elsewhere.
//!
//! `roxmltree` is already in `Cargo.lock`, reached through `gpui -> resvg -> usvg`. By the
//! argument that justifies `regex` and `serde_json` in `crates/app` — already compiled for
//! every build, so declaring it costs nothing — it looked free. **Measured, the release
//! binary went 18.91 MB to 19.09 MB, past the 19 MB gate.**
//!
//! A crate being in the dependency *graph* is not the same as its code being in the
//! *binary*. usvg calls only the part of `roxmltree` that SVG needs and the linker was
//! dropping the rest; using the full document API pulled it all in. The `regex` precedent
//! holds only where the existing user already exercises the whole crate, and does not
//! generalise to "anything in the lock file is free". [`xml`] is the replacement: ~200
//! lines that parse exactly what Xdebug emits and reject everything else.
//!
//! # No gpui, no runtime
//!
//! Plain blocking Rust (ADR-0004, ADR-0007), so the protocol and the session state machine
//! are testable without a window and without a live PHP process. Every test in this crate
//! runs against captured Xdebug packets or a fake engine on a loopback socket; none needs
//! PHP installed.
//!
//! # Scope
//!
//! Line breakpoints, stepping, the call stack and variable inspection. Deliberately absent,
//! and each for a stated reason rather than for lack of time:
//!
//! - **Watch expressions** need `eval`, which runs arbitrary code in the debugged request.
//!   That is a real capability with a real blast radius and deserves its own decision.
//! - **Conditional breakpoints** are a small addition to [`protocol::breakpoint_set_line`]
//!   (`-t conditional` plus a base64 expression) and were left out to keep the surface
//!   that ships fully tested.
//! - **Path mapping** beyond identity. A local project's paths are the paths PHP sees; a
//!   containerised one's are not, and mapping them well means a settings surface and a
//!   round-trip UI for getting it wrong. [`uri_to_path`] handles the local case honestly
//!   and does not pretend to handle the other.
//!
//! # Example
//!
//! ```no_run
//! use elle_debug::{Listener, DEFAULT_PORT};
//! use std::time::Duration;
//!
//! // We listen; PHP connects to us when a request runs with Xdebug enabled.
//! let listener = Listener::bind(DEFAULT_PORT)?;
//! if let Some(mut session) = listener.accept(Duration::from_secs(30))? {
//!     session.set_breakpoint("file:///srv/app/index.php", 24)?;
//!     let stop = session.run()?;
//!     if stop.is_paused() {
//!         for frame in session.stack()? {
//!             println!("{} at {}:{}", frame.function, frame.file_uri, frame.line);
//!         }
//!         for local in session.locals(0)? {
//!             println!("{} = {:?}", local.name, local.value);
//!         }
//!     }
//!     session.detach()?;
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```

mod breakpoints;
pub mod protocol;
mod session;
mod transport;
mod xml;

pub use breakpoints::{Breakpoint, BreakpointStore};
pub use protocol::{Init, Property, ProtocolError, StackFrame, Status, context};
pub use session::{DEFAULT_PORT, Listener, READ_TIMEOUT, Session, Stop};

use std::path::{Path, PathBuf};

/// Converts a filesystem path to the `file://` URI DBGp expects.
///
/// Percent-encoding is applied to the characters that would otherwise change how a URI
/// parses. Xdebug is lenient about receiving a raw path, but the URIs it *sends* are
/// encoded, and a breakpoint set under one spelling of a path will not match a stop
/// reported under the other.
pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            // RFC 3986's unreserved set, plus `/` as the path separator. Spelled as three
            // separate ranges rather than the tempting `b'A'..=b'z'`, which also admits
            // `[ \ ] ^ ` ` — the six ASCII characters that sit between `Z` and `a`, and
            // exactly the ones a URI must not carry unencoded.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char);
            }
            other => uri.push_str(&format!("%{other:02X}")),
        }
    }
    uri
}

/// Converts a `file://` URI back to a path.
///
/// Returns `None` for anything that is not a local file URI, rather than guessing. A
/// remote or `eval`'d frame has no local path, and inventing one would open the wrong
/// file in the editor — the stack panel's most confusing possible failure.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///srv/app` has an empty authority; `file://srv/app` would name a host. Both
    // spellings appear in the wild — Xdebug's own test suite emits the second — and on a
    // single machine both mean the same local path.
    let rest = rest.strip_prefix('/').map_or(rest, |stripped| stripped);

    let mut decoded = Vec::with_capacity(rest.len());
    let mut bytes = rest.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next()?;
            let low = bytes.next()?;
            let pair = [high, low];
            let text = std::str::from_utf8(&pair).ok()?;
            decoded.push(u8::from_str_radix(text, 16).ok()?);
        } else {
            decoded.push(byte);
        }
    }

    let text = String::from_utf8(decoded).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(PathBuf::from(format!("/{text}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_round_trips_through_a_uri() {
        let path = Path::new("/srv/app/routes/web.php");
        assert_eq!(path_to_uri(path), "file:///srv/app/routes/web.php");
        assert_eq!(uri_to_path("file:///srv/app/routes/web.php").unwrap(), path);
    }

    #[test]
    fn a_path_with_spaces_survives_the_round_trip() {
        // "Meus Projetos" is an ordinary macOS directory name, and an unencoded space in a
        // URI is the classic way a breakpoint silently fails to bind.
        let path = Path::new("/Users/ricardo/Meus Projetos/app.php");
        let uri = path_to_uri(path);
        assert!(!uri.contains(' '), "{uri}");
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn an_accented_path_survives_the_round_trip() {
        // Percent-encoding is per byte, so a multi-byte character becomes several escapes
        // and must be reassembled before the UTF-8 decode.
        let path = Path::new("/srv/aplicação/coração.php");
        let uri = path_to_uri(path);
        assert!(uri.is_ascii(), "a URI must be ASCII: {uri}");
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn the_two_authority_spellings_both_mean_the_local_path() {
        // Xdebug's own test suite emits `file://name.inc` with no empty authority, while a
        // real session sends `file:///srv/...`. Both have to land on the same place.
        assert_eq!(
            uri_to_path("file:///srv/app/index.php").unwrap(),
            Path::new("/srv/app/index.php")
        );
        assert_eq!(
            uri_to_path("file://srv/app/index.php").unwrap(),
            Path::new("/srv/app/index.php")
        );
    }

    #[test]
    fn a_non_file_uri_yields_nothing_rather_than_a_wrong_path() {
        // An `eval`'d frame or a stream wrapper has no local file. Guessing one would open
        // an unrelated file in the editor and claim execution is stopped there.
        assert!(uri_to_path("http://example.com/index.php").is_none());
        assert!(uri_to_path("xdebug://debug-eval").is_none());
        assert!(uri_to_path("file://").is_none());
        assert!(uri_to_path("").is_none());
        // Truncated escape.
        assert!(uri_to_path("file:///srv/a%2").is_none());
    }
}
