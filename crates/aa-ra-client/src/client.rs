//! The client: spawn rust-analyzer, run the initialize handshake, send
//! `textDocument/rename`, and tear down cleanly.
//!
//! State machine:
//!
//! ```text
//!     (new) ──► Initialized ──► (rename)* ──► ShuttingDown ──► (drop)
//! ```
//!
//! We keep everything synchronous: each method owns the stdin/stdout
//! until it returns. Notifications that arrive between a request and
//! its response (`$/progress`, `window/logMessage`, diagnostics) are
//! drained inline and discarded — this client only cares about the
//! response to its own request.

use std::io;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;

use crate::framing::{read_message, write_message};
use crate::transport::{LspReader, LspWriter};
use crate::types::*;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("rust-analyzer not available: {0}")]
    NotAvailable(String),
    #[error("lsp error response: {0}")]
    LspError(String),
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("unexpected protocol state: {0}")]
    Protocol(String),
}

pub struct RenameRequest<'a> {
    /// Absolute path of the file containing the declaration site.
    pub file: &'a Path,
    /// 0-indexed line of any occurrence of the symbol.
    pub line: u32,
    /// 0-indexed character offset of any occurrence of the symbol.
    pub character: u32,
    pub new_name: &'a str,
}

/// The client.
///
/// `transport` is any pair of blocking (reader, writer); in production
/// it wraps a child process's stdio. `_child` is held to keep the
/// subprocess alive for the client's lifetime and to reap it on drop.
pub struct Client {
    reader: Box<dyn LspReader>,
    writer: Box<dyn LspWriter>,
    next_id: AtomicU64,
    timeout: Duration,
    _child: Option<ChildGuard>,
}

impl Client {
    /// Spawn `rust-analyzer` against `workspace_root`. Returns
    /// `ClientError::NotAvailable` if the binary cannot be launched —
    /// callers typically fall back to the syntactic rename path in that
    /// case.
    pub fn spawn(workspace_root: &Path, timeout: Duration) -> Result<Self, ClientError> {
        let mut child = match Command::new("rust-analyzer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(workspace_root)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Err(ClientError::NotAvailable(e.to_string())),
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClientError::Protocol("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClientError::Protocol("no stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ClientError::Protocol("no stderr".into()))?;

        let guard = ChildGuard {
            child,
            _stderr: stderr,
        };
        let mut client = Client::with_transport(Box::new(stdout), Box::new(stdin), timeout);
        client._child = Some(guard);
        client.initialize(workspace_root)?;
        Ok(client)
    }

    /// Build a client over an arbitrary transport pair. Exposed for
    /// in-process mock tests; production code goes through [`spawn`].
    pub fn with_transport(
        reader: Box<dyn LspReader>,
        writer: Box<dyn LspWriter>,
        timeout: Duration,
    ) -> Self {
        Self {
            reader,
            writer,
            next_id: AtomicU64::new(1),
            timeout,
            _child: None,
        }
    }

    /// Variant of [`Client::with_transport`] for tests: runs the
    /// initialize handshake against the peer before returning. Real
    /// callers go through [`Client::spawn`] which already does this.
    pub fn with_transport_initialized(
        reader: Box<dyn LspReader>,
        writer: Box<dyn LspWriter>,
        workspace_root: &Path,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let mut client = Self::with_transport(reader, writer, timeout);
        client.initialize(workspace_root)?;
        Ok(client)
    }

    fn initialize(&mut self, root: &Path) -> Result<(), ClientError> {
        let uri = DocumentUri::from_path(root);
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: uri.clone(),
            workspace_folders: vec![WorkspaceFolder {
                uri,
                name: root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "root".into()),
            }],
            capabilities: ClientCapabilities {
                workspace: WorkspaceCaps {
                    workspace_edit: WorkspaceEditCaps {
                        document_changes: false,
                    },
                },
                text_document: TextDocumentCaps {
                    rename: RenameCaps {
                        prepare_support: false,
                    },
                    synchronization: SyncCaps { did_save: false },
                },
            },
            client_info: ClientInfo {
                name: "aa-ra-client",
                version: env!("CARGO_PKG_VERSION"),
            },
        };
        // Narrow the handshake-EOF case to `NotAvailable` so the
        // patching pipeline can degrade gracefully. Typical triggers:
        // the rustup proxy binary exists at `~/.cargo/bin/rust-analyzer`
        // but the underlying component is not installed, so the process
        // spawns *and then immediately exits* with an error message on
        // stderr. From our side we see a closed stdout on the first
        // read, which `framing::read_message` reports as an
        // `UnexpectedEof`.
        match self.request("initialize", &params) {
            Ok(_) => {}
            Err(ClientError::Io(ref e))
                if e.kind() == io::ErrorKind::UnexpectedEof
                    || e.kind() == io::ErrorKind::BrokenPipe =>
            {
                return Err(ClientError::NotAvailable(format!(
                    "rust-analyzer exited before completing the handshake: {e}"
                )));
            }
            Err(other) => return Err(other),
        }
        self.notify("initialized", &serde_json::json!({}))?;
        Ok(())
    }

    /// Ask the server to rename the symbol at `(file, line, character)`
    /// to `new_name`. Returns the server's `WorkspaceEdit`.
    pub fn rename(&mut self, req: RenameRequest<'_>) -> Result<WorkspaceEdit, ClientError> {
        // Open the file first — rust-analyzer won't rename a file it
        // hasn't been notified about.
        let uri = DocumentUri::from_path(req.file);
        let text = std::fs::read_to_string(req.file)?;
        self.notify(
            "textDocument/didOpen",
            &DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "rust",
                    version: 1,
                    text,
                },
            },
        )?;

        let params = RenameParams {
            text_document: TextDocumentIdentifier { uri },
            position: Position {
                line: req.line,
                character: req.character,
            },
            new_name: req.new_name,
        };
        let result = self.request("textDocument/rename", &params)?;
        // LSP spec: `textDocument/rename` returns `WorkspaceEdit |
        // null`. `null` means the server cannot perform the rename
        // (typically: workspace not yet indexed, or the position
        // doesn't resolve to a renameable symbol). Surface it as an
        // empty `WorkspaceEdit` so the wire type stays non-Optional
        // for prod typed-rename callers — they already treat empty
        // edits as a no-op via `flatten().is_empty()` — and so the
        // e2e retry helper can poll for the rename to start producing
        // edits.
        if result.is_null() {
            return Ok(WorkspaceEdit::default());
        }
        let edit: WorkspaceEdit = serde_json::from_value(result)?;
        Ok(edit)
    }

    /// Send a `textDocument/didOpen` notification with the given text.
    /// Unlike [`Client::rename`], this does not block on a response —
    /// `didOpen` is a one-way LSP notification. Used by [`Session`] when
    /// it first sees a file across a call boundary.
    pub fn did_open(&mut self, file: &Path, text: &str, version: i32) -> Result<(), ClientError> {
        let uri = DocumentUri::from_path(file);
        self.notify(
            "textDocument/didOpen",
            &DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: "rust",
                    version,
                    text: text.to_string(),
                },
            },
        )
    }

    /// Send a `textDocument/didChange` notification with the full new
    /// text. LSP accepts both incremental and full-document change
    /// events; we always use the full variant because [`Session`]
    /// already owns the complete new content. Server implementations
    /// (rust-analyzer included) handle the full form unconditionally.
    pub fn did_change(&mut self, file: &Path, text: &str, version: i32) -> Result<(), ClientError> {
        let uri = DocumentUri::from_path(file);
        self.notify(
            "textDocument/didChange",
            &DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri, version },
                content_changes: vec![TextDocumentContentChangeEvent {
                    text: text.to_string(),
                }],
            },
        )
    }

    /// Ask for a rename at `(file, line, character)` on a file the
    /// caller has already synchronised via [`Client::did_open`] or
    /// [`Client::did_change`]. Unlike [`Client::rename`], we do **not**
    /// re-read the file from disk or re-send `didOpen` — a
    /// persistent-session caller is responsible for keeping the server
    /// in sync with its in-memory view.
    pub fn rename_at(
        &mut self,
        file: &Path,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<WorkspaceEdit, ClientError> {
        let uri = DocumentUri::from_path(file);
        let params = RenameParams {
            text_document: TextDocumentIdentifier { uri },
            position: Position { line, character },
            new_name,
        };
        let result = self.request("textDocument/rename", &params)?;
        // LSP spec: `textDocument/rename` returns `WorkspaceEdit |
        // null`. `null` means the server cannot perform the rename
        // (typically: workspace not yet indexed, or the position
        // doesn't resolve to a renameable symbol). Surface it as an
        // empty `WorkspaceEdit` so the wire type stays non-Optional
        // for prod typed-rename callers — they already treat empty
        // edits as a no-op via `flatten().is_empty()` — and so the
        // e2e retry helper can poll for the rename to start producing
        // edits.
        if result.is_null() {
            return Ok(WorkspaceEdit::default());
        }
        let edit: WorkspaceEdit = serde_json::from_value(result)?;
        Ok(edit)
    }

    /// Best-effort shutdown. Sends `shutdown` + `exit`, then drops.
    pub fn shutdown(mut self) -> Result<(), ClientError> {
        let _ = self.request("shutdown", &serde_json::json!(null));
        let _ = self.notify("exit", &serde_json::json!(null));
        Ok(())
    }

    // --- low-level send/receive --------------------------------------------

    fn request<P: serde::Serialize>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let body = serde_json::to_vec(&req)?;
        write_message(&mut self.writer, &body)?;

        // Drain non-response messages (notifications / unrelated
        // responses) until we get the matching id — or timeout.
        let deadline = Instant::now() + self.timeout;
        loop {
            if Instant::now() >= deadline {
                return Err(ClientError::Timeout(self.timeout));
            }
            let body = read_message(&mut self.reader)?;
            let msg: JsonRpcResponse = serde_json::from_slice(&body)?;
            // Notifications and server-initiated requests carry
            // `method`; responses don't.
            if msg.method.is_some() {
                continue;
            }
            let msg_id = match &msg.id {
                Some(v) => v,
                None => continue,
            };
            if msg_id.as_u64() != Some(id) {
                // Out-of-order response — unusual for LSP since
                // requests are serialized, but we skip to stay robust.
                continue;
            }
            if let Some(err) = msg.error {
                return Err(ClientError::LspError(err.to_string()));
            }
            return Ok(msg.result.unwrap_or(Value::Null));
        }
    }

    fn notify<P: serde::Serialize>(&mut self, method: &str, params: &P) -> Result<(), ClientError> {
        let n = JsonRpcNotification {
            jsonrpc: "2.0",
            method,
            params,
        };
        let body = serde_json::to_vec(&n)?;
        write_message(&mut self.writer, &body)?;
        Ok(())
    }
}

/// RAII guard that kills the child process when the client is dropped
/// without a clean [`Client::shutdown`]. Without this a panicking caller
/// would leak rust-analyzer subprocesses across tests.
#[allow(dead_code)]
struct ChildGuard {
    child: Child,
    _stderr: ChildStderr,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Workaround: `ChildStdin`/`ChildStdout` implement Read/Write but are
// not Clone. The client holds boxed trait objects so neither the
// subprocess-owning path nor the mock path needs to know the concrete
// type.
#[allow(dead_code)]
fn _assert_bounds() {
    fn assert_send<T: Send>() {}
    assert_send::<ChildStdin>();
    assert_send::<ChildStdout>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockServer;

    #[test]
    fn initialize_shutdown_round_trip() {
        let root = std::env::temp_dir();
        let server = MockServer::new().with_rename_response(WorkspaceEdit::default());
        let (reader, writer, _handle) = server.spawn();
        let client =
            Client::with_transport_initialized(reader, writer, &root, Duration::from_secs(5))
                .expect("initialize");
        // Just shutting down should be clean.
        drop(client.shutdown());
    }

    #[test]
    fn rename_round_trip_via_mock() {
        let tmp = std::env::temp_dir().join(format!("aa-ra-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("lib.rs");
        std::fs::write(&file, "fn add() {}\nfn main() { add(); }\n").unwrap();

        let mut canned = WorkspaceEdit::default();
        let uri = DocumentUri::from_path(&file);
        canned.changes.insert(
            uri.clone(),
            vec![
                TextEdit {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 3,
                        },
                        end: Position {
                            line: 0,
                            character: 6,
                        },
                    },
                    new_text: "sum".into(),
                },
                TextEdit {
                    range: Range {
                        start: Position {
                            line: 1,
                            character: 13,
                        },
                        end: Position {
                            line: 1,
                            character: 16,
                        },
                    },
                    new_text: "sum".into(),
                },
            ],
        );
        let server = MockServer::new().with_rename_response(canned.clone());
        let (reader, writer, _handle) = server.spawn();
        let mut client =
            Client::with_transport_initialized(reader, writer, &tmp, Duration::from_secs(5))
                .expect("initialize");
        let got = client
            .rename(RenameRequest {
                file: &file,
                line: 0,
                character: 3,
                new_name: "sum",
            })
            .expect("rename");
        assert_eq!(got.changes.len(), 1);
        let edits = got.changes.get(&uri).expect("edits for file");
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].new_text, "sum");
        assert_eq!(edits[1].new_text, "sum");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn real_rust_analyzer_rename_when_available() {
        // This test shells out to the real `rust-analyzer` binary if one
        // is on PATH. By default it self-skips when the tool is missing
        // — useful on dev hosts that don't carry RA. The dedicated
        // `rust-analyzer-e2e` CI job sets `AA_REQUIRE_REAL_RA=1` to
        // turn the skip into a hard failure: that's how we close the
        // "RA tested only via in-process mock" gap noted in
        // docs/architecture.md.
        let ra_available = std::process::Command::new("rust-analyzer")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ra_available {
            if std::env::var("AA_REQUIRE_REAL_RA").is_ok() {
                panic!(
                    "AA_REQUIRE_REAL_RA is set but rust-analyzer is not on PATH; \
                     install it before running this test (CI installs it via \
                     `rustup component add rust-analyzer`)"
                );
            }
            eprintln!("rust-analyzer not available; skipping real-binary test");
            return;
        }

        // A minimal crate fixture.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"ra_rename_fixture\"\nversion = \"0.0.0\"\n\
             edition = \"2021\"\n[lib]\npath = \"src/lib.rs\"\n[workspace]\n",
        )
        .unwrap();
        let lib = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
                   pub fn double(x: i32) -> i32 { add(x, x) }\n";
        std::fs::write(root.join("src/lib.rs"), lib).unwrap();

        let mut client = Client::spawn(root, Duration::from_secs(60)).expect("spawn rust-analyzer");

        // rust-analyzer needs to discover + index the workspace
        // before it can resolve `add` to a symbol. The four
        // indexing-not-ready signals it can return are documented
        // in `crate::retry`; both the test and the production
        // callers (`OneShotResolver`, `Session::rename`) now share
        // the same `retry_rename_until_indexed` helper. 180s is
        // sized for a cold-cache GitHub runner where `cargo
        // metadata` + proc-macro server warmup can take
        // significantly longer than on a developer host with a
        // warm `~/.cargo`.
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
        let outcome = crate::retry::retry_rename_until_indexed(
            &mut client,
            &root.join("src/lib.rs"),
            0,
            7,
            "sum",
            deadline,
            crate::retry::DEFAULT_POLL_INTERVAL,
        );
        for a in &outcome.attempts {
            eprintln!("attempt {}: {}", a.attempt, a.reason);
        }
        let edit = outcome.edit.expect("rename (after indexing)");
        // `flatten()` merges `changes` and `documentChanges` —
        // RA 1.95+ ignores our `documentChanges: false` capability
        // and always returns the newer form, so reading `changes`
        // directly would silently see no edits.
        let by_uri = edit.flatten();
        assert!(!by_uri.is_empty(), "RA returned no edits");
        let total: usize = by_uri.values().map(|v| v.len()).sum();
        assert!(
            total >= 2,
            "expected at least 2 edits (def + 1 caller), got {total}"
        );
        client.shutdown().ok();
    }
}
