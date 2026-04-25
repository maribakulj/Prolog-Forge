//! LSP Content-Length framing.
//!
//! ```text
//! Content-Length: N\r\n\r\n<N bytes of UTF-8 JSON>
//! ```
//!
//! Other headers (e.g. `Content-Type`) are tolerated and ignored, per
//! the LSP spec. We keep the implementation intentionally byte-level —
//! no buffered reader state, no `BufRead` trait object — so it composes
//! cleanly with both the child process's stdout and a loopback pipe in
//! tests.

use std::io::{self, Read, Write};

/// Write a single framed message to `w`. The body is serialized by the
/// caller — we only add the envelope.
pub fn write_message<W: Write>(w: &mut W, body: &[u8]) -> io::Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Read exactly one framed message from `r`. Returns the body bytes with
/// the envelope stripped. `UnexpectedEof` on a half-framed stream.
pub fn read_message<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut header_buf: Vec<u8> = Vec::with_capacity(64);
    // Read byte-by-byte until the \r\n\r\n separator. LSP headers are ASCII;
    // we never need to worry about multi-byte boundaries.
    loop {
        let mut b = [0u8; 1];
        let n = r.read(&mut b)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF before header separator",
            ));
        }
        header_buf.push(b[0]);
        if header_buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if header_buf.len() > 8192 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LSP header exceeded 8 KiB",
            ));
        }
    }
    let header_text = std::str::from_utf8(&header_buf).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("non-UTF-8 header: {e}"))
    })?;
    let mut content_length: Option<usize> = None;
    for line in header_text.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed LSP header"))?;
        if key.eq_ignore_ascii_case("Content-Length") {
            content_length = Some(value.trim().parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad Content-Length: {e}"),
                )
            })?);
        }
        // All other headers are ignored per spec.
    }
    let length = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;

    let mut body = vec![0u8; length];
    r.read_exact(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip_ascii_body() {
        let body = b"{\"jsonrpc\":\"2.0\"}";
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, body).unwrap();
        let mut r = Cursor::new(buf);
        let got = read_message(&mut r).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn multiple_messages_chained() {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, b"first").unwrap();
        write_message(&mut buf, b"second").unwrap();
        let mut r = Cursor::new(buf);
        assert_eq!(read_message(&mut r).unwrap(), b"first");
        assert_eq!(read_message(&mut r).unwrap(), b"second");
    }

    #[test]
    fn tolerates_unknown_header() {
        let envelope =
            b"Content-Length: 5\r\nContent-Type: application/vscode-jsonrpc\r\n\r\nhello";
        let mut r = Cursor::new(envelope);
        assert_eq!(read_message(&mut r).unwrap(), b"hello");
    }

    #[test]
    fn utf8_body() {
        let body = "résumé: {\"ok\": \"café\"}".as_bytes();
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, body).unwrap();
        let mut r = Cursor::new(buf);
        assert_eq!(read_message(&mut r).unwrap(), body);
    }

    #[test]
    fn rejects_missing_content_length() {
        let envelope = b"X-Other: foo\r\n\r\nhello";
        let mut r = Cursor::new(envelope);
        let err = read_message(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_malformed_header() {
        let envelope = b"Content-Length 5\r\n\r\nhello";
        let mut r = Cursor::new(envelope);
        let err = read_message(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
