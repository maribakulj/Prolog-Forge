//! Minimal LSP client for rust-analyzer — Step 2 of the type-aware rename
//! ladder (`crates/pf-patch/src/rust_rename.rs` has the full map).
//!
//! rust-analyzer speaks LSP over stdio. This crate wraps it with the
//! narrowest API the patch pipeline needs:
//!
//! 1. spawn a child process against a workspace root,
//! 2. run the handshake (initialize / initialized),
//! 3. send `textDocument/rename` at a known declaration site,
//! 4. collect the resulting `WorkspaceEdit`,
//! 5. shut down cleanly.
//!
//! We deliberately do **not** reach for `tower-lsp` or `lsp-types` here.
//! Those crates pull in a much larger tree than we need for a single
//! request-response cycle; the LSP messages we touch fit on one page and
//! their JSON shape is stable.
//!
//! # Testability
//!
//! The client is written against a pair of traits ([`LspReader`] and
//! [`LspWriter`]) so an in-process mock LSP server ([`mock::MockServer`])
//! can stand in for the real rust-analyzer binary in unit tests. The CI
//! host used while this code was written does not carry a
//! `rust-analyzer` binary, so the full end-to-end verification happens
//! on any machine that does (`cargo test -p pf-ra-client --ignored`).

pub mod framing;
pub mod mock;
pub mod transport;
pub mod types;

mod client;

pub use client::{Client, ClientError, RenameRequest};
pub use types::{DocumentUri, Position, Range, TextEdit, WorkspaceEdit};
