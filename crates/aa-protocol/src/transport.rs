//! LSP-style Content-Length framing for JSON-RPC over a byte stream
//! (stdio or socket).
//!
//! Frame format:
//!   Content-Length: <N>\r\n
//!   \r\n
//!   <N bytes of UTF-8 JSON>
//!
//! Additional headers (e.g. Content-Type) are tolerated but ignored.

use std::io::{BufRead, Write};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed header: {0}")]
    BadHeader(String),
    #[error("missing Content-Length header")]
    MissingContentLength,
    #[error("invalid utf-8 in frame body")]
    Utf8,
    #[error("connection closed")]
    Closed,
}

/// Read one framed message from `reader`. Returns the raw JSON bytes.
pub fn read_frame<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, FramingError> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(FramingError::Closed);
        }
        if line == "\r\n" || line == "\n" {
            break; // end of headers
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| FramingError::BadHeader(line.trim().to_string()))?;
        if name.eq_ignore_ascii_case("content-length") {
            let v: usize = value
                .trim()
                .parse()
                .map_err(|_| FramingError::BadHeader(line.trim().to_string()))?;
            content_length = Some(v);
        }
    }

    let len = content_length.ok_or(FramingError::MissingContentLength)?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write one framed message to `writer`.
pub fn write_frame<W: Write>(writer: &mut W, body: &[u8]) -> Result<(), FramingError> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"{\"hello\":1}").unwrap();
        let mut c = Cursor::new(buf);
        let out = read_frame(&mut c).unwrap();
        assert_eq!(out, b"{\"hello\":1}");
    }

    #[test]
    fn lf_only_header_terminator() {
        let frame = b"Content-Length: 2\n\n{}";
        let mut c = Cursor::new(frame.to_vec());
        let out = read_frame(&mut c).unwrap();
        assert_eq!(out, b"{}");
    }
}
