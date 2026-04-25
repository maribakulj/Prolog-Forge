//! Persistent rust-analyzer session pool.
//!
//! Keyed by a caller-chosen `session_key` (typically the workspace's
//! canonical root path). The first typed-rename request for a given
//! key spawns an `RaSession` and mirrors the incoming file map; later
//! requests reuse that session's long-lived `rust-analyzer` child and
//! only re-sync files that have changed. The cost of the initial
//! cargo-metadata + indexing is amortised across every call, so
//! back-to-back `patch.preview` + `patch.apply` on the same typed op
//! only pay it once.
//!
//! # Concurrency
//!
//! The pool holds its sessions behind an outer `RwLock`
//! (`HashMap<key, Arc<Mutex<Session>>>`) so concurrent rename requests
//! on *different* workspaces go through without contention; concurrent
//! rename requests on the *same* workspace serialise at the session
//! `Mutex`. Serialisation is intentional — `rust-analyzer` is not
//! reentrant on a single stdin/stdout, and LSP request/response
//! correlation already assumes a single in-flight request per
//! transport.
//!
//! # Lifecycle
//!
//! Sessions live until the pool drops. `Core::drop` cascades through
//! the pool, which drops each Arc'd session; `Session::drop` reaps the
//! `rust-analyzer` child process. There is no idle timeout today — it
//! is a Phase 1.14 item once we have a real production workload to
//! tune against.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use aa_ra_client::{ClientError, DocumentUri, Session, TextEdit, WorkspaceEdit};

use aa_patch::{TypedRenameError, TypedRenameRequest, TypedRenameResolver};

/// Pool of reusable `rust-analyzer` sessions.
pub struct RaSessionPool {
    sessions: RwLock<HashMap<String, Arc<Mutex<Session>>>>,
    /// Timeout applied to individual LSP requests. The session's
    /// indexing is not bounded by this (indexing is implicit —
    /// `rust-analyzer` blocks the first request until ready); only
    /// per-request RPCs time out.
    timeout: Duration,
}

impl RaSessionPool {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            timeout: Duration::from_secs(120),
        }
    }

    /// Total count of live sessions. Used by tests and by a future
    /// `memory.stats` surface.
    pub fn len(&self) -> usize {
        self.sessions.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolve a typed rename, spawning a session for `session_key`
    /// on the first request or reusing an existing one. The session
    /// key is caller-provided; aa-core passes the workspace's
    /// canonical root so two workspaces never share an RA.
    fn resolve_inner(
        &self,
        session_key: &str,
        req: TypedRenameRequest<'_>,
    ) -> Result<BTreeMap<String, String>, TypedRenameError> {
        // Acquire (or create) the session arc. The outer lock is held
        // only long enough to look up / insert; the per-session
        // Mutex serialises the actual LSP round-trip.
        let handle = {
            let map = self.sessions.read().unwrap();
            map.get(session_key).cloned()
        };
        let session = match handle {
            Some(h) => h,
            None => {
                // Drop the read lock before acquiring the write lock
                // to avoid upgrade deadlocks.
                let mut map = self.sessions.write().unwrap();
                if let Some(existing) = map.get(session_key) {
                    existing.clone()
                } else {
                    let s = match Session::spawn(req.files, self.timeout) {
                        Ok(s) => s,
                        Err(ClientError::NotAvailable(msg)) => {
                            return Err(TypedRenameError::Unavailable(msg));
                        }
                        Err(e) => return Err(TypedRenameError::Client(e.to_string())),
                    };
                    let arc = Arc::new(Mutex::new(s));
                    map.insert(session_key.to_string(), arc.clone());
                    arc
                }
            }
        };

        // Run the rename on the serialised session. We copy out the
        // workspace's root path up-front so the post-rename
        // URI-to-relative remapping doesn't need to hold the session
        // lock.
        let edit: WorkspaceEdit;
        let root_path;
        {
            let mut s = session.lock().unwrap();
            edit = s
                .sync_and_rename(
                    req.files,
                    req.decl_file,
                    req.decl_line,
                    req.decl_character,
                    req.new_name,
                )
                .map_err(|e| TypedRenameError::Client(e.to_string()))?;
            root_path = s.root().to_path_buf();
        }

        // Apply RA's `WorkspaceEdit` to the in-memory file map. Shape
        // mirrors the one-shot path so the pool and the one-shot
        // resolvers are interchangeable from the caller's point of
        // view.
        let mut out = req.files.clone();
        apply_workspace_edit(&mut out, &edit, &root_path)?;

        // Keep the session's internal view in sync with the map we're
        // about to return. Without this, the *next* call's `sync`
        // would report "this file changed since last time" and
        // re-send a didChange for the freshly-edited file, which is
        // harmless but wasteful. `ack_applied_edits` is a no-op write
        // when the session's shadow already matches.
        {
            let mut s = session.lock().unwrap();
            let _ = s.ack_applied_edits(&out);
        }

        Ok(out)
    }
}

impl Default for RaSessionPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Adapter so the pool plugs into `aa-patch::apply_plan_with_resolver`.
/// The resolver's `resolve` signature has no `session_key` field —
/// aa-core extracts one from the caller's workspace root before
/// delegating here.
pub struct PooledResolver<'a> {
    pub pool: &'a RaSessionPool,
    pub session_key: String,
}

impl<'a> TypedRenameResolver for PooledResolver<'a> {
    fn resolve(
        &self,
        req: TypedRenameRequest<'_>,
    ) -> Result<BTreeMap<String, String>, TypedRenameError> {
        self.pool.resolve_inner(&self.session_key, req)
    }
}

// --- Duplicated from aa-patch::typed_rename to avoid cyclic
// --- deps: applying a WorkspaceEdit is a small pure function, and
// --- keeping it here lets aa-core own the full pool flow.

fn apply_workspace_edit(
    files: &mut BTreeMap<String, String>,
    edit: &WorkspaceEdit,
    root: &std::path::Path,
) -> Result<(), TypedRenameError> {
    for (uri, edits) in &edit.changes {
        let rel = uri_to_relative(uri, root)
            .ok_or_else(|| TypedRenameError::UnknownEditTarget(uri.0.clone()))?;
        let source = files
            .get(&rel)
            .cloned()
            .ok_or_else(|| TypedRenameError::UnknownEditTarget(rel.clone()))?;
        let new = apply_text_edits(&source, edits)?;
        files.insert(rel, new);
    }
    Ok(())
}

fn uri_to_relative(uri: &DocumentUri, root: &std::path::Path) -> Option<String> {
    let path_str = uri.0.strip_prefix("file://")?;
    let absolute = std::path::Path::new(path_str);
    let rel = absolute.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().into_owned())
}

fn apply_text_edits(source: &str, edits: &[TextEdit]) -> Result<String, TypedRenameError> {
    let line_starts = line_starts(source);
    let mut byte_edits: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for e in edits {
        let a = linecol_to_byte(
            &line_starts,
            source,
            e.range.start.line,
            e.range.start.character,
        )
        .ok_or_else(|| TypedRenameError::InvalidEdit(e.range, "start out of range".into()))?;
        let b = linecol_to_byte(
            &line_starts,
            source,
            e.range.end.line,
            e.range.end.character,
        )
        .ok_or_else(|| TypedRenameError::InvalidEdit(e.range, "end out of range".into()))?;
        if b < a {
            return Err(TypedRenameError::InvalidEdit(
                e.range,
                "end precedes start".into(),
            ));
        }
        byte_edits.push((a, b, &e.new_text));
    }
    byte_edits.sort_by_key(|(a, _, _)| *a);
    for pair in byte_edits.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(TypedRenameError::InvalidEdit(
                aa_ra_client::Range {
                    start: aa_ra_client::Position {
                        line: 0,
                        character: 0,
                    },
                    end: aa_ra_client::Position {
                        line: 0,
                        character: 0,
                    },
                },
                "overlapping edits".into(),
            ));
        }
    }
    let mut out = source.to_string();
    for (a, b, text) in byte_edits.into_iter().rev() {
        out.replace_range(a..b, text);
    }
    Ok(out)
}

fn line_starts(src: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

fn linecol_to_byte(line_starts: &[usize], src: &str, line: u32, character: u32) -> Option<usize> {
    let line = line as usize;
    let character = character as usize;
    if line >= line_starts.len() {
        return None;
    }
    let line_start = line_starts[line];
    let line_end = line_starts.get(line + 1).copied().unwrap_or(src.len());
    let line_text = &src[line_start..line_end];
    if line_text.is_ascii() {
        let off = line_start + character;
        if off > line_end {
            return None;
        }
        return Some(off);
    }
    let mut cu = 0usize;
    for (i, c) in line_text.char_indices() {
        if cu >= character {
            return Some(line_start + i);
        }
        cu += c.len_utf16();
    }
    if cu >= character {
        Some(line_end)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool must degrade gracefully when `rust-analyzer` is not
    /// available, returning `Unavailable` so the planner can emit a
    /// clear diagnostic and leave the shadow untouched — same
    /// contract as `OneShotResolver`.
    #[test]
    fn resolve_when_ra_missing_returns_unavailable() {
        // This test only makes sense on hosts where `rust-analyzer`
        // is *absent* — its whole point is to assert the pool emits
        // `TypedRenameError::Unavailable` on that path so the planner
        // can degrade to the syntactic fallback. On hosts with RA on
        // PATH, `resolve_inner` actually talks to RA and may return
        // any of {Ok, indexing-not-ready error, didOpen race error}
        // — none of which exercise the unavailable-degradation
        // contract. The dedicated `rust-analyzer-e2e` CI job covers
        // the available path end-to-end, so skipping here loses no
        // signal.
        let ra_available = std::process::Command::new("rust-analyzer")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ra_available {
            eprintln!("rust-analyzer is available; this test asserts the absent path — skipping");
            return;
        }
        let pool = RaSessionPool::new();
        let mut files = BTreeMap::new();
        files.insert(
            "Cargo.toml".into(),
            "[package]\nname=\"x\"\nversion=\"0.0.0\"\nedition=\"2021\"\n\
             [lib]\npath=\"src/lib.rs\"\n[workspace]\n"
                .into(),
        );
        files.insert("src/lib.rs".into(), "pub fn a() {}\n".into());
        let r = pool.resolve_inner(
            "workspace-key-1",
            TypedRenameRequest {
                files: &files,
                decl_file: "src/lib.rs",
                decl_line: 0,
                decl_character: 7,
                new_name: "b",
                timeout: Duration::from_secs(30),
            },
        );
        match r {
            Err(TypedRenameError::Unavailable(_)) => { /* expected */ }
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("RA was supposed to be unavailable but resolve succeeded"),
        }
        // `Session::spawn` fails before the pool stores the session,
        // so the pool should still be empty.
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn pool_caches_session_per_key() {
        // Without a real RA we can't test session reuse end-to-end,
        // but we can at least prove the key-based dispatch is wired:
        // two requests against the same key must go through the same
        // map slot (len == 1); two requests against different keys
        // must create distinct entries (when Session::spawn would
        // succeed — on this host the spawns fail, so both keys stay
        // empty). The invariant we can check unconditionally is:
        // `resolve_inner` on the same key twice must not double the
        // session count.
        let pool = RaSessionPool::new();
        let files: BTreeMap<String, String> = BTreeMap::new();
        let req = || TypedRenameRequest {
            files: &files,
            decl_file: "src/lib.rs",
            decl_line: 0,
            decl_character: 0,
            new_name: "x",
            timeout: Duration::from_secs(30),
        };
        let _ = pool.resolve_inner("a", req());
        let before = pool.len();
        let _ = pool.resolve_inner("a", req());
        let after = pool.len();
        assert_eq!(before, after, "same key must not grow the pool");
    }
}
