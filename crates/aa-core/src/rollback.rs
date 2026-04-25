//! Rollback pipeline.
//!
//! Given a committed `commit_id`, read the journal entry, verify on-disk
//! state still matches the `after` bytes we wrote (otherwise someone
//! hand-edited the file and we refuse), then write `before` bytes back via
//! the same atomic temp + rename protocol used by `apply`. On success the
//! journal entry is removed — rollbacks do not cascade through history in
//! Phase 1.5. A linear redo/undo stack is a later concern.

use std::collections::BTreeMap;
use std::path::Path;

use crate::apply;
use crate::journal;

#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    #[error("journal: {0}")]
    Journal(#[from] journal::JournalError),
    #[error("preflight: {0}")]
    Preflight(String),
    #[error(transparent)]
    Apply(#[from] apply::ApplyError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct RollbackOutcome {
    pub commit_id: String,
    pub files_restored: usize,
    pub label: String,
}

pub fn rollback(root: &Path, commit_id: &str) -> Result<RollbackOutcome, RollbackError> {
    let entry = journal::read(root, commit_id)?;

    // Preflight: current on-disk state must match the `after` bytes.
    for f in &entry.files {
        let p = root.join(&f.path);
        let on_disk = std::fs::read_to_string(&p)
            .map_err(|e| RollbackError::Preflight(format!("reading {}: {}", p.display(), e)))?;
        if on_disk != f.after {
            return Err(RollbackError::Preflight(format!(
                "workspace has drifted since the commit: {}",
                f.path
            )));
        }
    }

    // Build shadow = `before` bytes; original = `after` bytes. The atomic
    // apply path gives us the preflight + backup + rollback-on-failure
    // story for free.
    let mut shadow: BTreeMap<String, String> = BTreeMap::new();
    let mut original: BTreeMap<String, String> = BTreeMap::new();
    for f in &entry.files {
        shadow.insert(f.path.clone(), f.before.clone());
        original.insert(f.path.clone(), f.after.clone());
    }

    let out = apply::apply_transactional(root, &shadow, &original)?;
    journal::delete(root, commit_id)?;

    Ok(RollbackOutcome {
        commit_id: entry.commit_id,
        files_restored: out.files_written,
        label: entry.label,
    })
}
