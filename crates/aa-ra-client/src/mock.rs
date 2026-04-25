//! In-process mock LSP server.
//!
//! Lets the client tests run without a real `rust-analyzer` binary on
//! PATH. We spawn a thread that reads framed JSON-RPC messages from a
//! loopback pipe, responds to the small subset the client actually
//! sends (`initialize`, `initialized`, `textDocument/didOpen`,
//! `textDocument/rename`, `shutdown`, `exit`), and exits.
//!
//! The API is: configure canned responses with builder methods, call
//! `.spawn()` to get `(reader, writer)` transport pair for the client
//! plus a join handle so tests can assert the server terminated
//! cleanly.

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use serde_json::{json, Value};

use crate::framing::{read_message, write_message};
use crate::transport::{LspReader, LspWriter};
use crate::types::WorkspaceEdit;

#[derive(Default)]
pub struct MockServer {
    rename_response: Option<WorkspaceEdit>,
}

impl MockServer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_rename_response(mut self, edit: WorkspaceEdit) -> Self {
        self.rename_response = Some(edit);
        self
    }

    /// Spawn the server thread. Returns the *client-side* reader/writer
    /// pair (so the client reads what the server writes, and vice
    /// versa) plus a handle you can join to check for panics.
    pub fn spawn(self) -> (Box<dyn LspReader>, Box<dyn LspWriter>, JoinHandle<()>) {
        // Client writes → server reads; server writes → client reads.
        let (cs_tx, cs_rx) = mpsc::channel::<u8>(); // client → server
        let (sc_tx, sc_rx) = mpsc::channel::<u8>(); // server → client

        let server_read = ChannelReader { rx: cs_rx };
        let server_write = ChannelWriter { tx: sc_tx };

        let rename_response = self.rename_response;
        let handle = std::thread::Builder::new()
            .name("aa-ra-mock".into())
            .spawn(move || run(server_read, server_write, rename_response))
            .expect("spawn mock server thread");

        let client_read: Box<dyn LspReader> = Box::new(ChannelReader { rx: sc_rx });
        let client_write: Box<dyn LspWriter> = Box::new(ChannelWriter { tx: cs_tx });
        (client_read, client_write, handle)
    }
}

fn run<R: Read, W: Write>(mut reader: R, mut writer: W, rename: Option<WorkspaceEdit>) {
    loop {
        let body = match read_message(&mut reader) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(_) => break,
        };
        let msg: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        match (method, id) {
            ("initialize", Some(id)) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": {
                            "renameProvider": true,
                            "textDocumentSync": 1
                        },
                        "serverInfo": { "name": "aa-ra-mock", "version": "0" }
                    }
                });
                if write_message(&mut writer, resp.to_string().as_bytes()).is_err() {
                    break;
                }
            }
            ("textDocument/rename", Some(id)) => {
                let result = rename.clone().unwrap_or_default();
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": serde_json::to_value(&result).unwrap()
                });
                if write_message(&mut writer, resp.to_string().as_bytes()).is_err() {
                    break;
                }
            }
            ("shutdown", Some(id)) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": null
                });
                let _ = write_message(&mut writer, resp.to_string().as_bytes());
            }
            ("exit", _) => break,
            // notifications: nothing to reply
            (_, _) => {}
        }
    }
}

struct ChannelReader {
    rx: Receiver<u8>,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        // Wait for at least one byte.
        match self.rx.recv() {
            Ok(b) => {
                out[0] = b;
            }
            Err(_) => return Ok(0), // channel closed → EOF
        }
        let mut n = 1;
        // Drain any additional buffered bytes non-blockingly up to `out`
        // capacity. This matters for `read_exact` performance but not
        // for correctness — the loop would still work with single-byte
        // reads.
        while n < out.len() {
            match self.rx.try_recv() {
                Ok(b) => {
                    out[n] = b;
                    n += 1;
                }
                Err(_) => break,
            }
        }
        Ok(n)
    }
}

struct ChannelWriter {
    tx: Sender<u8>,
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &b in buf {
            self.tx
                .send(b)
                .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Unused field in `MockServer` when all builder setters aren't used;
// silence the warning without suppressing it crate-wide.
#[allow(dead_code)]
fn _unused(_: &MockServer) {}
