//! AYE-AYE daemon — a stdio JSON-RPC 2.0 server.
//!
//! The daemon reads LSP-style Content-Length framed messages from stdin,
//! dispatches them through the Core, and writes framed responses to stdout.
//! Notifications (requests without `id`) are accepted but produce no reply.
//! Logs and tracing are written to stderr, never to stdout, to avoid
//! corrupting the protocol stream.

use std::io::{self, BufReader, StderrLock, Write};

use aa_core::{dispatch, Core};
use aa_protocol::{read_frame, write_frame, FramingError, Request, Response, RpcError};
use anyhow::Result;
use tracing::{debug, error, info};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        "aye-aye daemon starting"
    );

    let core = Core::new();
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(f) => f,
            Err(FramingError::Closed) => {
                info!("stdin closed; exiting");
                return Ok(());
            }
            Err(e) => {
                error!(error = %e, "framing error");
                return Err(e.into());
            }
        };
        let parsed: Result<Request, _> = serde_json::from_slice(&frame);
        let response = match parsed {
            Ok(req) => {
                let method = req.method.clone();
                let is_shutdown = method == aa_protocol::METHOD_SHUTDOWN;
                debug!(method = %method, "request");
                let resp = dispatch(&core, req);
                if is_shutdown {
                    if let Some(r) = resp.as_ref() {
                        write_response(&mut writer, r)?;
                    }
                    info!("shutdown requested");
                    return Ok(());
                }
                resp
            }
            Err(e) => {
                error!(error = %e, "parse error");
                Some(Response::err(
                    aa_protocol::Id::Null,
                    RpcError::parse_error(e.to_string()),
                ))
            }
        };
        if let Some(r) = response {
            write_response(&mut writer, &r)?;
        }
    }
}

fn write_response<W: Write>(w: &mut W, r: &Response) -> Result<()> {
    let bytes = serde_json::to_vec(r)?;
    write_frame(w, &bytes)?;
    Ok(())
}

// Ensure the type is used; otherwise dead-code warning.
#[allow(dead_code)]
fn _stderr_hint(_: StderrLock<'_>) {}
