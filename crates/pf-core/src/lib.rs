//! The Prolog Forge Core: session lifecycle + dispatch of typed API methods.
//!
//! This crate is transport-agnostic. It accepts JSON-RPC `Request` values and
//! returns `Response` values. The daemon wraps it with stdio framing; a test
//! can call it directly; a future gRPC transport would reuse it unchanged.

pub mod apply;
pub mod handlers;
pub mod index;
pub mod lower;
pub mod session;

pub use session::Core;

use pf_protocol::{Request, Response, RpcError};
use serde_json::Value;

/// Dispatch one JSON-RPC request and produce a response. Notifications
/// (requests without an id) are dispatched for their side effects and yield
/// `None`.
pub fn dispatch(core: &Core, req: Request) -> Option<Response> {
    let id = req.id.clone();
    let params = req.params.unwrap_or(Value::Null);
    let result: Result<Value, RpcError> = handlers::route(core, &req.method, params);
    let id = id?;
    Some(match result {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(id, e),
    })
}
