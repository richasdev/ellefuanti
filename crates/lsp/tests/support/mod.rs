//! Plumbing shared by the integration tests.
//!
//! Real OS pipes rather than in-memory channels, so the tests exercise the actual
//! framing path — partial reads included — and so the client under test is wired up
//! exactly as it would be against a spawned process.

use std::io::{BufRead, Write};

/// A crossed pair of pipes: what the client writes, the server reads, and vice versa.
pub struct Pipes {
    pub client_reader: os_pipe::PipeReader,
    pub client_writer: os_pipe::PipeWriter,
    pub server_reader: os_pipe::PipeReader,
    pub server_writer: os_pipe::PipeWriter,
}

impl Pipes {
    pub fn new() -> Self {
        // client → server
        let (server_reader, client_writer) = os_pipe::pipe().expect("pipe");
        // server → client
        let (client_reader, server_writer) = os_pipe::pipe().expect("pipe");
        Self { client_reader, client_writer, server_reader, server_writer }
    }
}

/// Writes one `Content-Length`-framed message.
///
/// Deliberately a separate implementation from the crate's own `write_message` rather
/// than a re-export: if the mock used the code under test to frame its replies, a
/// framing bug would cancel itself out and the tests would pass against a client that
/// could not talk to any real server.
pub fn write_frame(writer: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()
}

/// Reads one framed message. `Ok(None)` at end of stream.
pub fn read_frame(reader: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut length = None;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if length.is_none() {
                continue;
            }
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("Content-Length")
        {
            length = value.trim().parse().ok();
        }
    }

    let Some(length) = length else {
        return Ok(None);
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}
