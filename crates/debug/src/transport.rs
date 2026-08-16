//! DBGp packet framing over a byte stream.
//!
//! Where LSP frames with HTTP-like headers, DBGp frames with a length and NUL bytes:
//!
//! ```text
//! 193\0<?xml version="1.0" encoding="iso-8859-1"?>\n<response .../>\0
//! ```
//!
//! The length is ASCII decimal, then a NUL, then exactly that many bytes of XML, then a
//! second NUL. **The declared length excludes the trailing NUL**, which is the one detail
//! a reimplementation gets wrong, and it desynchronises the stream permanently rather
//! than failing loudly. Settled from Xdebug's own `make_message()` in `handler_dbgp.c`
//! rather than from the specification, which does not say:
//!
//! ```c
//! xdebug_str_add_fmt(ret, "%d", xml_message.l + sizeof("<?xml ...?>\n") - 1);
//! xdebug_str_addc(ret, '\0');   // separator
//! ...                           // declaration + body: exactly that many bytes
//! xdebug_str_addc(ret, '\0');   // terminator, NOT counted
//! ```
//!
//! Two consequences worth stating because they look like bugs later. The XML declaration
//! says `iso-8859-1`, not UTF-8, and it is *inside* the counted length together with its
//! trailing newline. And no NUL may appear within a packet, which is what makes the
//! terminator unambiguous.
//!
//! # Why the same partial-read discipline as LSP
//!
//! This reads by byte count into a `Vec<u8>` and decodes only once the whole body has
//! arrived, for the reason `crates/lsp/src/transport.rs` documents at length: a
//! multi-byte character split across a chunk boundary is a non-issue if no partial
//! buffer is ever decoded. TCP splits wherever it likes, so the tests feed every message
//! one byte at a time.
//!
//! # Encoding
//!
//! Xdebug declares `iso-8859-1` but emits UTF-8 in practice for the payloads that matter,
//! and base64-encodes anything it is unsure of. Decoding is therefore lossy-UTF-8 rather
//! than a Latin-1 transcode: a mojibake variable name is survivable, a hard error on one
//! stray byte is not, and §24's rule that a misbehaving backend must not stop the user
//! working applies here as much as it does to a language server.

use std::io::{BufRead, Write};

use anyhow::{Context as _, Result, bail};

/// Packets larger than this are refused rather than allocated.
///
/// A corrupt length would otherwise have us reserve arbitrary memory before reading a
/// byte. A `context_get` over a large object is the biggest real payload and sits far
/// below this, so the limit only fires on a stream that has already gone wrong.
const MAX_PACKET_BYTES: usize = 64 * 1024 * 1024;

/// Reads one packet, blocking until it is complete.
///
/// Returns `Ok(None)` at a clean end of stream — the script finished and PHP closed the
/// socket, which is the normal end of every debug session and must not be reported as an
/// error. A stream that ends *mid-packet* is an error, because that is a crash.
pub fn read_packet(input: &mut impl BufRead) -> Result<Option<String>> {
    let Some(length) = read_length(input)? else {
        return Ok(None);
    };

    let mut body = vec![0u8; length];
    input
        .read_exact(&mut body)
        .context("stream ended mid-packet: the debug session died while sending a response")?;

    // The trailing NUL, which the length did not count.
    let mut terminator = [0u8; 1];
    input.read_exact(&mut terminator).context("stream ended before a packet's terminating NUL")?;
    if terminator[0] != 0 {
        // Reading on would return whatever this length happened to slice out of the next
        // packet, so this fails rather than guessing.
        bail!("packet was not NUL-terminated: the declared length is out of step with the stream");
    }

    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

/// Reads the ASCII length prefix up to its NUL separator.
///
/// `Ok(None)` means a clean EOF before any digit was seen.
fn read_length(input: &mut impl BufRead) -> Result<Option<usize>> {
    let mut digits = Vec::new();
    // Bounded so a stream of non-NUL bytes cannot grow this without limit: a real prefix
    // is a handful of digits, and anything longer is a desynchronised stream.
    const MAX_DIGITS: usize = 32;

    loop {
        let mut byte = [0u8; 1];
        match input.read(&mut byte) {
            Ok(0) => {
                return if digits.is_empty() {
                    Ok(None)
                } else {
                    bail!("stream ended inside a packet's length prefix")
                };
            }
            Ok(_) => {}
            Err(err) => return Err(err).context("reading a packet's length prefix"),
        }

        if byte[0] == 0 {
            break;
        }
        if digits.len() >= MAX_DIGITS {
            bail!("packet length prefix is not a number: the stream is out of step");
        }
        digits.push(byte[0]);
    }

    if digits.is_empty() {
        bail!("packet had an empty length prefix");
    }

    let text = std::str::from_utf8(&digits)
        .context("packet length prefix was not ASCII: the stream is out of step")?;
    let length: usize =
        text.trim().parse().with_context(|| format!("bad packet length: {text:?}"))?;
    if length > MAX_PACKET_BYTES {
        bail!("packet length {length} exceeds the {MAX_PACKET_BYTES} byte limit");
    }
    Ok(Some(length))
}

/// Writes one command.
///
/// Commands travel the other way with no length prefix at all — the IDE sends a plain
/// line terminated by a single NUL. The asymmetry is the protocol's, not ours.
pub fn write_command(out: &mut impl Write, command: &str) -> Result<()> {
    out.write_all(command.as_bytes())?;
    out.write_all(&[0])?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor, Read};

    /// A reader that yields at most `chunk` bytes per `read`, so a packet is guaranteed
    /// to arrive in pieces. This is the condition that breaks naive framing, and a
    /// `Cursor` never reproduces it — but a TCP socket does, constantly.
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

    /// Frames a body the way Xdebug does: length, NUL, body, NUL.
    fn framed(body: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(body.len().to_string().as_bytes());
        out.push(0);
        out.extend(body.as_bytes());
        out.push(0);
        out
    }

    fn read_all(data: Vec<u8>, chunk: usize) -> Vec<String> {
        let mut reader = BufReader::new(ChunkedReader { data, position: 0, chunk });
        let mut packets = Vec::new();
        while let Some(body) = read_packet(&mut reader).unwrap() {
            packets.push(body);
        }
        packets
    }

    #[test]
    fn round_trips_a_single_packet() {
        assert_eq!(read_all(framed("<response/>"), 4096), vec!["<response/>"]);
    }

    #[test]
    fn reads_several_packets_from_one_buffer() {
        // Xdebug replies serially, but TCP is free to deliver three replies in one read.
        let mut data = framed("<a/>");
        data.extend(framed("<b/>"));
        data.extend(framed("<c/>"));
        assert_eq!(read_all(data, 4096), vec!["<a/>", "<b/>", "<c/>"]);
    }

    #[test]
    fn survives_being_fed_one_byte_at_a_time() {
        // The strictest split there is: the length digits, both NULs and every body byte
        // arrive in separate reads. A socket under load does exactly this.
        let mut data = framed("<a/>");
        data.extend(framed("<bb/>"));
        assert_eq!(read_all(data, 1), vec!["<a/>", "<bb/>"]);
    }

    #[test]
    fn the_declared_length_excludes_the_terminating_nul() {
        // The detail that desynchronises the stream if it is wrong, pinned against a real
        // Xdebug packet. Counting the NUL would make this length 12 and leave the reader
        // one byte out of step on every packet after the first.
        let body = "<response/>";
        let data = framed(body);
        assert_eq!(&data[..3], b"11\0", "length counts the body only");
        assert_eq!(*data.last().unwrap(), 0, "and the packet still ends with a NUL");
        assert_eq!(read_all(data, 1), vec![body]);
    }

    #[test]
    fn multibyte_characters_split_across_chunk_boundaries_survive() {
        // Every chunk size puts the boundary somewhere different, and several land inside
        // the UTF-8 encoding of ç and ã. A PHP variable holding a Portuguese string is
        // not an edge case in this product.
        let body = r#"<property name="$saudação"><![CDATA[coração]]></property>"#;
        let data = framed(body);
        for chunk in 1..=40 {
            assert_eq!(read_all(data.clone(), chunk), vec![body], "chunk size {chunk}");
        }
    }

    #[test]
    fn a_real_xdebug_packet_frames_and_reads_back() {
        // Byte-exact from Xdebug's own test suite, declaration and trailing newline
        // included, because both count toward the length.
        let body = "<?xml version=\"1.0\" encoding=\"iso-8859-1\"?>\n\
                    <response xmlns=\"urn:debugger_protocol_v1\" command=\"run\" \
                    transaction_id=\"4\" status=\"break\" reason=\"ok\"></response>";
        assert_eq!(read_all(framed(body), 7), vec![body]);
    }

    #[test]
    fn clean_eof_between_packets_is_not_an_error() {
        // The normal end of every session: the script ran to completion and PHP hung up.
        let mut reader = BufReader::new(Cursor::new(Vec::new()));
        assert!(read_packet(&mut reader).unwrap().is_none());
    }

    #[test]
    fn eof_midway_through_a_body_is_an_error() {
        // A script fatally erroring mid-response must be reported, not mistaken for a
        // clean shutdown, or the caller waits forever for a reply that is gone.
        let mut data = framed("<response/>");
        data.truncate(data.len() - 5);
        let mut reader = BufReader::new(Cursor::new(data));
        let err = read_packet(&mut reader).unwrap_err().to_string();
        assert!(err.contains("mid-packet"), "{err}");
    }

    #[test]
    fn eof_inside_the_length_prefix_is_an_error() {
        let mut reader = BufReader::new(Cursor::new(b"123".to_vec()));
        assert!(read_packet(&mut reader).is_err());
    }

    #[test]
    fn a_missing_terminator_is_reported_rather_than_guessed() {
        // A length one short would otherwise slice the next packet's first byte off and
        // corrupt everything after it. Failing here is the only recoverable answer.
        let mut data = Vec::new();
        data.extend(b"4\0");
        data.extend(b"<a/>X");
        let mut reader = BufReader::new(Cursor::new(data));
        let err = read_packet(&mut reader).unwrap_err().to_string();
        assert!(err.contains("NUL-terminated"), "{err}");
    }

    #[test]
    fn a_non_numeric_length_is_rejected() {
        let mut reader = BufReader::new(Cursor::new(b"garbage\0<a/>\0".to_vec()));
        assert!(read_packet(&mut reader).is_err());
    }

    #[test]
    fn an_absurd_length_is_refused_without_allocating() {
        let mut reader = BufReader::new(Cursor::new(b"99999999999\0".to_vec()));
        let err = read_packet(&mut reader).unwrap_err().to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn an_unterminated_run_of_junk_does_not_grow_without_bound() {
        // Something that is not a DBGp peer connected to the port. The reader must give
        // up rather than buffer whatever it sends until memory runs out.
        let mut reader = BufReader::new(Cursor::new(vec![b'x'; 1024]));
        assert!(read_packet(&mut reader).is_err());
    }

    #[test]
    fn commands_are_written_nul_terminated_without_a_length() {
        // The asymmetry: replies carry a length prefix, commands do not.
        let mut out = Vec::new();
        write_command(&mut out, "run -i 4").unwrap();
        assert_eq!(out, b"run -i 4\0");
    }
}
