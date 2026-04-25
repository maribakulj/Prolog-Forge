//! Scope-resolved rename via `rust-analyzer`.
//!
//! Mirrors the in-memory shadow file map to a temp directory, spawns
//! rust-analyzer against that directory, asks for a rename, applies the
//! returned `WorkspaceEdit` back onto the in-memory map, and returns
//! the new file contents.
//!
//! # Graceful degradation
//!
//! If `rust-analyzer` is not on `PATH` the function returns a dedicated
//! [`TypedRenameError::Unavailable`] error. The caller (preview layer)
//! turns that into a `PreviewError` diagnostic and leaves the shadow
//! untouched — the same pattern `CargoCheckStage` uses when `cargo` is
//! missing. We never crash an apply because the oracle is missing;
//! instead we surface the missing oracle explicitly so the verdict
//! stays honest.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use thiserror::Error;

use aa_ra_client::{Client, ClientError, DocumentUri, Range, RenameRequest, WorkspaceEdit};

#[derive(Debug, Error)]
pub enum TypedRenameError {
    #[error("rust-analyzer unavailable: {0}")]
    Unavailable(String),
    #[error("rust-analyzer client error: {0}")]
    Client(String),
    #[error("I/O error staging shadow workspace: {0}")]
    Io(String),
    #[error("declaration file `{0}` not in shadow workspace")]
    MissingDeclFile(String),
    #[error("rust-analyzer returned edits for unknown file `{0}`")]
    UnknownEditTarget(String),
    #[error("rename would produce invalid text at {0:?}: {1}")]
    InvalidEdit(Range, String),
}

pub struct TypedRenameRequest<'a> {
    pub files: &'a BTreeMap<String, String>,
    pub decl_file: &'a str,
    pub decl_line: u32,
    pub decl_character: u32,
    pub new_name: &'a str,
    pub timeout: Duration,
}

/// A pluggable resolver for `RenameFunctionTyped` ops.
///
/// The default implementation — [`OneShotResolver`] — spawns a fresh
/// `rust-analyzer` process for every call, mirroring the in-memory
/// file map to a throwaway tempdir. That is the simplest but also the
/// slowest strategy: rust-analyzer's initial index is rebuilt per
/// request. `aa-core`'s `RaSessionPool` implements the same trait but
/// reuses a single RA session per workspace across calls, paying the
/// indexing cost once.
///
/// The trait exists so `aa-patch::apply_plan` remains oblivious to
/// who is on the other side of the resolver: one-shot, pool, future
/// caching proxy — all interchangeable.
pub trait TypedRenameResolver: Send + Sync {
    /// Resolve a typed rename. The caller passes the current shadow
    /// file map; the resolver returns the map after RA's edits are
    /// applied. An implementation that fails to reach rust-analyzer
    /// must return [`TypedRenameError::Unavailable`] so the planner
    /// can emit a clear diagnostic and the apply can degrade
    /// gracefully.
    fn resolve(
        &self,
        req: TypedRenameRequest<'_>,
    ) -> Result<BTreeMap<String, String>, TypedRenameError>;
}

/// Default one-shot resolver: every call spawns a fresh rust-analyzer.
pub struct OneShotResolver;

impl TypedRenameResolver for OneShotResolver {
    fn resolve(
        &self,
        req: TypedRenameRequest<'_>,
    ) -> Result<BTreeMap<String, String>, TypedRenameError> {
        resolve(req)
    }
}

/// Apply a scope-resolved rename. Returns the updated file map.
pub fn resolve(req: TypedRenameRequest<'_>) -> Result<BTreeMap<String, String>, TypedRenameError> {
    if !req.files.contains_key(req.decl_file) {
        return Err(TypedRenameError::MissingDeclFile(req.decl_file.to_string()));
    }

    // 1. Materialize the shadow to a temp directory so rust-analyzer
    //    can inspect a real cargo project.
    let tmp = tempfile::tempdir().map_err(|e| TypedRenameError::Io(e.to_string()))?;
    let root = tmp.path();
    for (rel, content) in req.files {
        let dest = root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| TypedRenameError::Io(e.to_string()))?;
        }
        fs::write(&dest, content).map_err(|e| TypedRenameError::Io(e.to_string()))?;
    }

    // 2. Spawn rust-analyzer and run the rename.
    let mut client = match Client::spawn(root, req.timeout) {
        Ok(c) => c,
        Err(ClientError::NotAvailable(msg)) => {
            return Err(TypedRenameError::Unavailable(msg));
        }
        Err(e) => return Err(TypedRenameError::Client(e.to_string())),
    };
    let decl_path = root.join(req.decl_file);
    let edit = client
        .rename(RenameRequest {
            file: &decl_path,
            line: req.decl_line,
            character: req.decl_character,
            new_name: req.new_name,
        })
        .map_err(|e| TypedRenameError::Client(e.to_string()))?;
    let _ = client.shutdown();

    // 3. Apply the WorkspaceEdit back to the in-memory map. The URIs RA
    //    emits are absolute `file://` paths under our tempdir; we strip
    //    the root prefix to recover the workspace-relative key.
    let mut out = req.files.clone();
    apply_workspace_edit(&mut out, &edit, root)?;
    Ok(out)
}

fn apply_workspace_edit(
    files: &mut BTreeMap<String, String>,
    edit: &WorkspaceEdit,
    root: &Path,
) -> Result<(), TypedRenameError> {
    // `flatten()` merges `changes` and `documentChanges` into a
    // single `(uri -> edits)` map. rust-analyzer 1.95+ ignores our
    // declared `documentChanges: false` capability and always returns
    // the newer form; iterating only `edit.changes` (as we did before
    // the WorkspaceEdit overhaul) silently dropped every edit and
    // made typed-rename a no-op against the real binary.
    for (uri, edits) in edit.flatten() {
        let rel = uri_to_relative(&uri, root)
            .ok_or_else(|| TypedRenameError::UnknownEditTarget(uri.0.clone()))?;
        let source = files
            .get(&rel)
            .cloned()
            .ok_or_else(|| TypedRenameError::UnknownEditTarget(rel.clone()))?;
        let new = apply_text_edits(&source, &edits)?;
        files.insert(rel, new);
    }
    Ok(())
}

fn uri_to_relative(uri: &DocumentUri, root: &Path) -> Option<String> {
    let path_str = uri.0.strip_prefix("file://")?;
    let absolute = std::path::Path::new(path_str);
    let rel = absolute.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().into_owned())
}

fn apply_text_edits(
    source: &str,
    edits: &[aa_ra_client::TextEdit],
) -> Result<String, TypedRenameError> {
    // Convert (line, character) to byte offsets, sort edits descending,
    // and splice from the end so earlier offsets remain valid.
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
    // Reject overlapping edits — LSP says servers must not return them.
    for pair in byte_edits.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(TypedRenameError::InvalidEdit(
                Range {
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
    // UTF-16 code units are what LSP actually uses. For Rust identifiers
    // (all ASCII) the difference is moot; we fall back to a char-count
    // walk for the rare non-ASCII case, which is correct for code
    // points <= U+FFFF. Astral-plane characters in a Rust source file
    // are rare enough to leave for a follow-up.
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
    use aa_ra_client::{Position, TextEdit};

    #[test]
    fn apply_text_edits_scope_resolved() {
        let src = "pub fn add() {}\nfn other() { add(); }\n";
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 7,
                    },
                    end: Position {
                        line: 0,
                        character: 10,
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
        ];
        let out = apply_text_edits(src, &edits).unwrap();
        assert_eq!(out, "pub fn sum() {}\nfn other() { sum(); }\n");
    }

    #[test]
    fn apply_text_edits_rejects_overlaps() {
        let src = "abcdef";
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "x".into(),
            },
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 2,
                    },
                    end: Position {
                        line: 0,
                        character: 5,
                    },
                },
                new_text: "y".into(),
            },
        ];
        let err = apply_text_edits(src, &edits).unwrap_err();
        assert!(matches!(err, TypedRenameError::InvalidEdit(_, _)));
    }

    #[test]
    fn missing_decl_file_returns_error() {
        let mut files = BTreeMap::new();
        files.insert("src/lib.rs".into(), "fn add(){}".into());
        let err = resolve(TypedRenameRequest {
            files: &files,
            decl_file: "src/missing.rs",
            decl_line: 0,
            decl_character: 0,
            new_name: "sum",
            timeout: Duration::from_secs(30),
        })
        .unwrap_err();
        assert!(matches!(err, TypedRenameError::MissingDeclFile(_)));
    }

    /// `rust-analyzer` is not on PATH in the CI host used while this
    /// code was written. `resolve` must report that cleanly rather than
    /// hanging or panicking. On a host where RA is available this test
    /// is not exercised (the `resolve` call succeeds instead), which is
    /// a legitimate pass too.
    #[test]
    fn degrades_gracefully_without_rust_analyzer() {
        // Asserts the *absent-RA* contract: the resolver must emit
        // `TypedRenameError::Unavailable` so the planner falls back to
        // the syntactic rename. On hosts with RA installed,
        // `OneShotResolver::resolve` actually talks to the binary and
        // races RA's workspace indexing — the very first rename
        // typically fails with `-32602 "No references found at
        // position"` until indexing completes. Production
        // `resolve()` does not retry (the new `aa-ra-client`
        // e2e test does, in test code only); fixing the cold-cache
        // race in production is tracked as a follow-up. The dedicated
        // `rust-analyzer-e2e` CI job covers the available-RA path
        // end-to-end with the retry helper, so skipping here loses no
        // signal for the absent-path contract this test owns.
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
        let mut files = BTreeMap::new();
        files.insert(
            "Cargo.toml".into(),
            "[package]\nname=\"x\"\nversion=\"0.0.0\"\nedition=\"2021\"\n\
             [lib]\npath=\"src/lib.rs\"\n[workspace]\n"
                .into(),
        );
        files.insert(
            "src/lib.rs".into(),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n".into(),
        );
        let result = resolve(TypedRenameRequest {
            files: &files,
            decl_file: "src/lib.rs",
            decl_line: 0,
            decl_character: 7,
            new_name: "sum",
            timeout: Duration::from_secs(30),
        });
        match result {
            Err(TypedRenameError::Unavailable(_)) => { /* expected */ }
            Err(other) => panic!("unexpected typed-rename error: {other}"),
            Ok(_) => panic!("RA was supposed to be unavailable but resolve succeeded"),
        }
    }
}
