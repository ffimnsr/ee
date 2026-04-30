// Copyright 2024 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Transport abstractions for the RPC layer.
//!
//! Provides [`ReadTransport`] and [`WriteTransport`] traits together with
//! two built-in framing strategies:
//!
//! - **Newline-delimited** ([`NewlineReader`] / [`NewlineWriter`]) –
//!   the original xi wire format; each JSON message is terminated by a
//!   single `\n` byte.
//! - **Content-Length framed** ([`ContentLengthReader`] /
//!   [`ContentLengthWriter`]) – LSP-compatible framing; each message is
//!   preceded by a `Content-Length: <N>\r\n\r\n` header.

use std::io::{self, BufRead, Write};

// ── Core traits ────────────────────────────────────────────────────

/// Reads complete framed messages from an underlying byte stream.
pub trait ReadTransport {
    /// Appends the next message body to `buf` and returns the number of
    /// bytes appended.  Returns `Ok(0)` to signal EOF / clean disconnect.
    fn read_message(&mut self, buf: &mut String) -> io::Result<usize>;
}

/// Writes complete framed messages to an underlying byte stream.
///
/// Implementations are responsible for applying framing and flushing
/// the writer after each message.
pub trait WriteTransport: Send + 'static {
    /// Encodes `data` with appropriate framing, writes it to the
    /// underlying stream, and flushes.
    fn write_message(&mut self, data: &[u8]) -> io::Result<()>;
}

// ── Newline-delimited framing ──────────────────────────────────────

/// Reads newline-terminated messages from any [`BufRead`].
pub struct NewlineReader<R>(pub R);

impl<R> NewlineReader<R> {
    pub fn new(inner: R) -> Self {
        NewlineReader(inner)
    }
}

impl<R: BufRead> ReadTransport for NewlineReader<R> {
    fn read_message(&mut self, buf: &mut String) -> io::Result<usize> {
        self.0.read_line(buf)
    }
}

/// Writes newline-terminated messages to any [`Write`], flushing after
/// each message.
pub struct NewlineWriter<W>(W);

impl<W: Write + Send + 'static> NewlineWriter<W> {
    pub fn new(inner: W) -> Self {
        NewlineWriter(inner)
    }

    /// Unwraps this `NewlineWriter`, returning the underlying writer.
    pub fn into_inner(self) -> W {
        self.0
    }
}

impl<W: Write + Send + 'static> WriteTransport for NewlineWriter<W> {
    fn write_message(&mut self, data: &[u8]) -> io::Result<()> {
        // Build the complete frame in one allocation so that Write
        // implementors that treat each write call as a discrete message
        // (e.g. channel-backed test writers) receive the whole frame
        // atomically.
        let mut frame = Vec::with_capacity(data.len() + 1);
        frame.extend_from_slice(data);
        frame.push(b'\n');
        self.0.write_all(&frame)?;
        self.0.flush()
    }
}

// ── Content-Length framing (LSP-style) ────────────────────────────

/// Reads `Content-Length`-framed messages from any [`BufRead`].
///
/// Expected wire format:
///
/// ```text
/// Content-Length: <N>\r\n
/// \r\n
/// <N bytes of JSON>
/// ```
pub struct ContentLengthReader<R>(pub R);

impl<R> ContentLengthReader<R> {
    pub fn new(inner: R) -> Self {
        ContentLengthReader(inner)
    }
}

impl<R: BufRead> ReadTransport for ContentLengthReader<R> {
    fn read_message(&mut self, buf: &mut String) -> io::Result<usize> {
        // Read the header line.
        let mut header = String::new();
        let n = self.0.read_line(&mut header)?;
        if n == 0 {
            return Ok(0); // EOF
        }

        let length: usize = header
            .trim_end()
            .strip_prefix("Content-Length: ")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid Content-Length header: {:?}", header.trim_end()),
                )
            })?;

        // Consume the blank separator line (\r\n or \n).
        let mut sep = String::new();
        self.0.read_line(&mut sep)?;

        // Read exactly `length` bytes.
        let mut body = vec![0u8; length];
        self.0.read_exact(&mut body)?;

        let s = std::str::from_utf8(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        buf.push_str(s);
        Ok(length)
    }
}

/// Writes `Content-Length`-framed messages to any [`Write`], flushing
/// after each message.
pub struct ContentLengthWriter<W>(W);

impl<W: Write + Send + 'static> ContentLengthWriter<W> {
    pub fn new(inner: W) -> Self {
        ContentLengthWriter(inner)
    }

    /// Unwraps this `ContentLengthWriter`, returning the underlying writer.
    pub fn into_inner(self) -> W {
        self.0
    }
}

impl<W: Write + Send + 'static> WriteTransport for ContentLengthWriter<W> {
    fn write_message(&mut self, data: &[u8]) -> io::Result<()> {
        write!(self.0, "Content-Length: {}\r\n\r\n", data.len())?;
        self.0.write_all(data)?;
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn newline_roundtrip() {
        let mut writer = NewlineWriter::new(Cursor::new(Vec::<u8>::new()));
        writer.write_message(b"{\"method\":\"ping\"}").unwrap();
        let out = writer.into_inner().into_inner();

        let mut reader = NewlineReader::new(Cursor::new(out));
        let mut buf = String::new();
        let n = reader.read_message(&mut buf).unwrap();
        assert!(n > 0);
        assert_eq!(buf.trim_end(), "{\"method\":\"ping\"}");
    }

    #[test]
    fn content_length_roundtrip() {
        let msg = b"{\"method\":\"ping\"}";
        let mut writer = ContentLengthWriter::new(Cursor::new(Vec::<u8>::new()));
        writer.write_message(msg).unwrap();
        let out = writer.into_inner().into_inner();

        let expected_header = format!("Content-Length: {}\r\n\r\n", msg.len());
        assert!(out.starts_with(expected_header.as_bytes()));

        let mut reader = ContentLengthReader::new(Cursor::new(out));
        let mut buf = String::new();
        let n = reader.read_message(&mut buf).unwrap();
        assert_eq!(n, msg.len());
        assert_eq!(buf.as_bytes(), msg);
    }

    #[test]
    fn newline_reader_eof_returns_zero() {
        let mut reader = NewlineReader::new(Cursor::new(b"".as_ref()));
        let mut buf = String::new();
        let n = reader.read_message(&mut buf).unwrap();
        assert_eq!(n, 0);
    }
}
