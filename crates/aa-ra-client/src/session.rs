//! Persistent rust-analyzer session.
//!
//! A single `Session` owns a tempdir mirror of a workspace and a live
//! rust-analyzer subprocess against it. Successive renames on the same
//! workspace reuse both: the initial `cargo metadata` + indexing cost
//! is paid once per session, and per-file sync between calls uses
//! `textDocument/didChange` with the full new text (no incremental
//! edit reconstruction).
//!
//! # Invalidation
//!
//! A `Session` is tied to the workspace it was spawned against. If
//! callers need to switch to a different root they drop the session
//! and spawn a new one. The pool in `aa-core` handles that policy —
//! this module does not.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::client::{Client, ClientError};
use crate::types::WorkspaceEdit;

/// Bookkeeping for a single file the session has synchronised with the
/// server. `version` is the LSP document version we last advertised;
/// `content` is the exact bytes we last sent. A new rename call compares
/// the incoming text against `content` to decide between a no-op, a
/// `didChange`, or (on the first sighting) a `didOpen`.
struct FileState {
    version: i32,
    content: String,
}

pub struct Session {
    /// Absolute path to the session's shadow directory on disk. Every
    /// workspace-relative path the caller hands in is resolved against
    /// this root.
    root: PathBuf,
    /// Kept to tie the tempdir's lifetime to the Session.
    _tmp: tempfile::TempDir,
    client: Client,
    known: BTreeMap<String, FileState>,
}

impl Session {
    /// Materialise `initial_files` to a fresh tempdir and spawn
    /// rust-analyzer against it. The initial mirror is synchronous;
    /// the `initialize` handshake waits for RA's response but does
    /// not block on indexing — the first rename will do that.
    pub fn spawn(
        initial_files: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let tmp = tempfile::tempdir().map_err(ClientError::Io)?;
        let root = tmp.path().join("project");
        fs::create_dir_all(&root).map_err(ClientError::Io)?;

        // Write every file before spawning RA so its initial index sees
        // the full workspace.
        for (rel, content) in initial_files {
            let dest = root.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(ClientError::Io)?;
            }
            let mut f = fs::File::create(&dest).map_err(ClientError::Io)?;
            f.write_all(content.as_bytes()).map_err(ClientError::Io)?;
        }

        let client = Client::spawn(&root, timeout)?;
        let known = initial_files
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    FileState {
                        version: 0,
                        content: v.clone(),
                    },
                )
            })
            .collect();

        Ok(Self {
            root,
            _tmp: tmp,
            client,
            known,
        })
    }

    /// Absolute path of the session's shadow workspace. Useful for
    /// rewriting `WorkspaceEdit` URIs back to workspace-relative keys.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sync `files` into the session (write disk + `didChange` for
    /// anything that changed, `didOpen` for anything new) and then
    /// send a rename request at `(decl_file, line, character)`. Returns
    /// the raw `WorkspaceEdit` RA produced; the caller remaps it back
    /// to workspace-relative paths.
    pub fn sync_and_rename(
        &mut self,
        files: &BTreeMap<String, String>,
        decl_file: &str,
        decl_line: u32,
        decl_character: u32,
        new_name: &str,
    ) -> Result<WorkspaceEdit, ClientError> {
        self.sync(files)?;

        let decl_path = self.root.join(decl_file);
        self.client
            .rename_at(&decl_path, decl_line, decl_character, new_name)
    }

    /// Public for the pool's use when it wants to apply a `WorkspaceEdit`
    /// to its in-memory view *and* have the on-disk shadow follow
    /// along. (Without this the on-disk state drifts from RA's view
    /// after an edit the caller applies in-memory.) The pool calls
    /// [`Session::ack_applied_edits`] immediately after a successful
    /// rename so the session's `known` map reflects reality.
    pub fn ack_applied_edits(
        &mut self,
        files: &BTreeMap<String, String>,
    ) -> Result<(), ClientError> {
        // Same primitive the next rename would run — walk the caller's
        // current map, write anything that differs, bump the version,
        // and notify the server.
        self.sync(files)
    }

    fn sync(&mut self, files: &BTreeMap<String, String>) -> Result<(), ClientError> {
        for (rel, content) in files {
            let path = self.root.join(rel);
            match self.known.get_mut(rel) {
                Some(state) if state.content == *content => {
                    // No-op: server already has this content at this version.
                }
                Some(state) => {
                    // Update disk + notify didChange.
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).map_err(ClientError::Io)?;
                    }
                    fs::write(&path, content).map_err(ClientError::Io)?;
                    state.version += 1;
                    state.content = content.clone();
                    self.client.did_change(&path, content, state.version)?;
                }
                None => {
                    // First sighting: write + didOpen.
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).map_err(ClientError::Io)?;
                    }
                    fs::write(&path, content).map_err(ClientError::Io)?;
                    self.client.did_open(&path, content, 1)?;
                    self.known.insert(
                        rel.clone(),
                        FileState {
                            version: 1,
                            content: content.clone(),
                        },
                    );
                }
            }
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best effort: shutdown the client on the way out. `Client`'s
        // own `Drop` kills the child if the handshake never gets sent,
        // so a panic here is not a leak.
        // We can't call `self.client.shutdown()` because shutdown takes
        // `self` by value and we only have `&mut Client`. The child
        // guard inside Client will still reap the process on drop.
        let _ = &self.client;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockServer;
    use crate::types::{Position, Range, TextEdit};
    use std::collections::HashMap;

    fn mock_session_pair(
        rename_edits: WorkspaceEdit,
    ) -> (PathBuf, Session, std::thread::JoinHandle<()>) {
        // Build a session whose Client is wired to an in-process mock
        // rather than a real `rust-analyzer` subprocess. The session's
        // tempdir still holds the shadow files; only the Client's
        // transport is swapped out.
        let server = MockServer::new().with_rename_response(rename_edits);
        let (reader, writer, handle) = server.spawn();

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        fs::create_dir_all(&root).unwrap();

        let client =
            Client::with_transport_initialized(reader, writer, &root, Duration::from_secs(5))
                .expect("mock initialize");
        let session = Session {
            root: root.clone(),
            _tmp: tmp,
            client,
            known: BTreeMap::new(),
        };
        (root, session, handle)
    }

    #[test]
    fn sync_opens_then_changes_across_calls() {
        let edits = WorkspaceEdit {
            changes: HashMap::new(),
        };
        let (root, mut session, _h) = mock_session_pair(edits);

        let mut files = BTreeMap::new();
        files.insert("src/lib.rs".into(), "fn a(){}\n".into());
        // First sync: didOpen only.
        session.sync(&files).unwrap();
        assert_eq!(session.known["src/lib.rs"].version, 1);
        assert!(root.join("src/lib.rs").exists());

        // Second sync with same content: no-op (version unchanged).
        session.sync(&files).unwrap();
        assert_eq!(session.known["src/lib.rs"].version, 1);

        // Third sync with new content: didChange + version bump.
        files.insert("src/lib.rs".into(), "fn a(){ 1 }\n".into());
        session.sync(&files).unwrap();
        assert_eq!(session.known["src/lib.rs"].version, 2);
        let on_disk = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert_eq!(on_disk, "fn a(){ 1 }\n");
    }

    #[test]
    fn rename_returns_mock_workspace_edit() {
        let mut edits = WorkspaceEdit {
            changes: HashMap::new(),
        };
        let tmp_root = std::env::temp_dir().join("aa-session-test-root");
        let _ = fs::create_dir_all(&tmp_root);
        edits.changes.insert(
            crate::types::DocumentUri::from_path(&tmp_root.join("project").join("src/lib.rs")),
            vec![TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 3,
                    },
                    end: Position {
                        line: 0,
                        character: 4,
                    },
                },
                new_text: "b".into(),
            }],
        );
        let (_root, mut session, _h) = mock_session_pair(edits);
        let mut files = BTreeMap::new();
        files.insert("src/lib.rs".into(), "fn a(){}\n".into());
        let got = session
            .sync_and_rename(&files, "src/lib.rs", 0, 3, "b")
            .expect("rename");
        assert_eq!(got.changes.len(), 1);
    }
}
