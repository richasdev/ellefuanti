//! JSON-RPC 2.0 framing over a byte stream.
//!
//! LSP frames every message as HTTP-like headers, a blank line, then exactly
//! `Content-Length` **bytes** of JSON:
//!
//! ```text
//! Content-Length: 42\r\n
//! Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n
//! \r\n
//! {"jsonrpc":"2.0",...}
//! ```
//!
//! Hand-rolled rather than taken from `async-lsp`, which is a tower/tokio framework —
//! ADR-0007 rules out a second runtime, and this crate must stay blocking and
//! executor-agnostic like every other domain crate. The framing itself is small; what
//! it needs is not cleverness but tests, because the failure modes are all about
//! *partial* data:
//!
//! - a header split across two reads
//! - a body split across two reads
//! - a multi-byte UTF-8 character split across a chunk boundary
//! - several messages arriving in one read
//!
//! Reading by `Content-Length` bytes into a `Vec<u8>` and decoding UTF-8 only once the
//! whole body has arrived is what makes the third case a non-issue: we never decode a
//! partial buffer, so `ç` can straddle any boundary it likes.

use std::io::{BufRead, Write};

use anyhow::{Context as _, Result, bail};

/// Bodies larger than this are refused rather than allocated.
///
/// A corrupted or hostile `Content-Length` would otherwise have us reserve arbitrary
/// memory before reading a byte. Real LSP payloads are far below this — a full
/// `didOpen` of a large file being the biggest — so the limit only ever fires on a
/// stream that has already gone wrong.
const MAX_MESSAGE_BYTES: usize = 128 * 1024 * 1024;

/// Writes one framed message.
pub fn write_message(out: &mut impl Write, body: &[u8]) -> Result<()> {
    // Only Content-Length is sent: Content-Type is optional in the specification and
    // every server ignores it.
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(body)?;
    out.flush()?;
    Ok(())
}

/// Reads one framed message, blocking until it is complete.
///
/// Returns `Ok(None)` at a clean end of stream — the server exited between messages,
/// which is normal after `exit` and must not be reported as an error. A stream that
/// ends *mid-message* is an error, because that is a crash.
pub fn read_message(input: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let Some(length) = read_headers(input)? else {
        return Ok(None);
    };

    let mut body = vec![0u8; length];
    // `read_exact` is the whole answer to split bodies: it loops until it has every
    // byte or the stream dies.
    input
        .read_exact(&mut body)
        .context("stream ended mid-message: the server died while sending a response")?;
    Ok(Some(body))
}

/// Reads the header block, returning the declared body length.
///
/// `Ok(None)` means a clean EOF before any header was seen.
fn read_headers(input: &mut impl BufRead) -> Result<Option<usize>> {
    let mut content_length = None;
    let mut saw_any_header = false;

    loop {
        let mut line = String::new();
        // `read_line` handles a header split across reads for us: it accumulates until
        // it sees a newline.
        let read = input.read_line(&mut line).context("reading message header")?;

        if read == 0 {
            return if saw_any_header {
                bail!("stream ended inside a message header")
            } else {
                Ok(None)
            };
        }

        let line = line.trim_end_matches(['\r', '\n']);

        // The blank line terminates the header block.
        if line.is_empty() {
            if !saw_any_header {
                // A stray blank line before any header: tolerate it rather than
                // desynchronising the stream on a server that emits an extra newline.
                continue;
            }
            break;
        }

        saw_any_header = true;

        let Some((name, value)) = line.split_once(':') else {
            bail!("malformed LSP header: {line:?}");
        };

        // Header names are case-insensitive per RFC 7230, which the specification
        // inherits; servers do vary.
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            let length: usize =
                value.trim().parse().with_context(|| format!("bad Content-Length: {value:?}"))?;
            if length > MAX_MESSAGE_BYTES {
                bail!("Content-Length {length} exceeds the {MAX_MESSAGE_BYTES} byte limit");
            }
            content_length = Some(length);
        }
        // Other headers (Content-Type) are ignored deliberately.
    }

    match content_length {
        Some(length) => Ok(Some(length)),
        None => bail!("message header block had no Content-Length"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor, Read};

    /// A reader that yields at most `chunk` bytes per `read` call, so a message is
    /// guaranteed to arrive in pieces. This is the condition that breaks naive framing
    /// implementations, and a `Cursor` never reproduces it.
    struct ChunkedReader {
        data: Vec<u8>,
        position: usize,
        chunk: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let remaining = self.data.len() - self.position;
            let take = remaining.min(self.chunk).min(buf.len());
            buf[..take].copy_from_slice(&self.data[self.position..self.position + take]);
            self.position += take;
            Ok(take)
        }
    }

    fn framed(body: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_message(&mut out, body.as_bytes()).unwrap();
        out
    }

    fn read_all(data: Vec<u8>, chunk: usize) -> Vec<String> {
        let mut reader = BufReader::new(ChunkedReader { data, position: 0, chunk });
        let mut messages = Vec::new();
        while let Some(body) = read_message(&mut reader).unwrap() {
            messages.push(String::from_utf8(body).unwrap());
        }
        messages
    }

    #[test]
    fn writes_content_length_framing() {
        let out = framed(r#"{"a":1}"#);
        assert_eq!(String::from_utf8(out).unwrap(), "Content-Length: 7\r\n\r\n{\"a\":1}");
    }

    #[test]
    fn content_length_counts_bytes_not_characters() {
        // "ação" is 4 characters but 6 bytes. Framing by character count would
        // desynchronise the stream permanently on the first accented identifier.
        let body = r#"{"s":"ação"}"#;
        let out = framed(body);
        let header =
            String::from_utf8(out[..out.iter().position(|&b| b == b'\r').unwrap()].to_vec())
                .unwrap();
        assert_eq!(header, format!("Content-Length: {}", body.len()));
        assert_eq!(read_all(out, 1024), vec![body]);
    }

    #[test]
    fn round_trips_a_single_message() {
        assert_eq!(read_all(framed(r#"{"a":1}"#), 4096), vec![r#"{"a":1}"#]);
    }

    #[test]
    fn reads_several_messages_from_one_buffer() {
        let mut data = framed(r#"{"a":1}"#);
        data.extend(framed(r#"{"b":2}"#));
        data.extend(framed(r#"{"c":3}"#));
        assert_eq!(read_all(data, 4096), vec![r#"{"a":1}"#, r#"{"b":2}"#, r#"{"c":3}"#]);
    }

    #[test]
    fn survives_being_fed_one_byte_at_a_time() {
        // The strictest split there is: every header character, the blank line, and
        // every body byte arrive in a separate read.
        let mut data = framed(r#"{"a":1}"#);
        data.extend(framed(r#"{"b":2}"#));
        assert_eq!(read_all(data, 1), vec![r#"{"a":1}"#, r#"{"b":2}"#]);
    }

    #[test]
    fn multibyte_characters_split_across_chunk_boundaries_survive() {
        // Every chunk size from 1 upward puts a boundary in a different place; several
        // of them land *inside* the UTF-8 encoding of ç, ã and the emoji.
        let body = r#"{"msg":"ação, coração e 😀 não quebram"}"#;
        let data = framed(body);
        for chunk in 1..=40 {
            assert_eq!(read_all(data.clone(), chunk), vec![body], "chunk size {chunk}");
        }
    }

    #[test]
    fn clean_eof_between_messages_is_not_an_error() {
        let mut reader = BufReader::new(Cursor::new(Vec::new()));
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn eof_midway_through_a_body_is_an_error() {
        // A server that crashes mid-response must be reported, not silently treated as
        // a clean shutdown, or the client would wait forever for a reply that is gone.
        let mut data = framed(r#"{"a":1234567}"#);
        data.truncate(data.len() - 5);
        let mut reader = BufReader::new(Cursor::new(data));
        let err = read_message(&mut reader).unwrap_err().to_string();
        assert!(err.contains("mid-message"), "{err}");
    }

    #[test]
    fn eof_midway_through_headers_is_an_error() {
        let mut reader = BufReader::new(Cursor::new(b"Content-Length: 7\r\n".to_vec()));
        assert!(read_message(&mut reader).is_err());
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let data = b"content-length: 7\r\n\r\n{\"a\":1}".to_vec();
        assert_eq!(read_all(data, 3), vec![r#"{"a":1}"#]);
    }

    #[test]
    fn extra_headers_are_ignored() {
        let data =
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 7\r\n\r\n{\"a\":1}"
                .to_vec();
        assert_eq!(read_all(data, 5), vec![r#"{"a":1}"#]);
    }

    #[test]
    fn a_missing_content_length_is_rejected() {
        let mut reader = BufReader::new(Cursor::new(b"Content-Type: x\r\n\r\n{}".to_vec()));
        let err = read_message(&mut reader).unwrap_err().to_string();
        assert!(err.contains("no Content-Length"), "{err}");
    }

    #[test]
    fn a_malformed_header_is_rejected() {
        let mut reader = BufReader::new(Cursor::new(b"garbage-without-a-colon\r\n\r\n{}".to_vec()));
        assert!(read_message(&mut reader).is_err());
    }

    #[test]
    fn an_absurd_content_length_is_refused_without_allocating() {
        let mut reader =
            BufReader::new(Cursor::new(b"Content-Length: 99999999999\r\n\r\n".to_vec()));
        let err = read_message(&mut reader).unwrap_err().to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn an_empty_body_is_valid() {
        let data = b"Content-Length: 0\r\n\r\n".to_vec();
        assert_eq!(read_all(data, 1), vec![""]);
    }

    #[test]
    fn bare_newline_line_endings_are_accepted() {
        // The specification says \r\n, but tolerating \n costs one trim and avoids a
        // hard failure against a server that gets it wrong.
        let data = b"Content-Length: 7\n\n{\"a\":1}".to_vec();
        assert_eq!(read_all(data, 2), vec![r#"{"a":1}"#]);
    }
}
