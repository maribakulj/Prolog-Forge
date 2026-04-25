//! Transport abstraction — the client owns a pair of `(reader, writer)`
//! trait objects so tests can plug in a loopback pipe while production
//! code targets a subprocess's stdin/stdout.
//!
//! The traits are deliberately narrow: each side is blocking `Read` /
//! `Write`. The LSP rename flow is request/response with at most a
//! handful of notifications interleaved — no need for async, select, or
//! mpsc plumbing at this layer. The client reads and discards
//! non-response messages inline.

use std::io::{Read, Write};

/// A blocking, byte-level reader of the LSP input stream. We re-export
/// `Read` under a trait name so the client constructor signature reads
/// domain-first (`LspReader` says what this *is*, not just that it is a
/// `Read`).
pub trait LspReader: Read + Send {}
impl<T: Read + Send> LspReader for T {}

pub trait LspWriter: Write + Send {}
impl<T: Write + Send> LspWriter for T {}
