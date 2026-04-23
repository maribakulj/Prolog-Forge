//! JSON-RPC 2.0 types, LSP-style Content-Length framing, and the public API
//! contract of the Prolog Forge Core.
//!
//! This crate is shared between the daemon (server) and every adapter
//! (client). Breaking changes here are breaking changes to the whole product.

pub mod api;
pub mod message;
pub mod transport;

pub use api::*;
pub use message::*;
pub use transport::*;

/// Current protocol version. Semver: MAJOR breaks wire compat, MINOR is
/// additive (new methods / new optional fields).
pub const PROTOCOL_VERSION: &str = "0.11.0";
