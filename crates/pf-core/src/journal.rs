//! Disk-persistent commit journal.
//!
//! Every successful `patch.apply` writes an entry to
//! `<workspace_root>/.prolog-forge/journal/<commit_id>.json` containing the
//! before/after bytes of every file the commit touched. `patch.rollback`
//! reads the entry, verifies the on-disk state still matches the `after`
//! bytes (optimistic concurrency — refuses if anyone hand-edited the file),
//! and restores the `before` bytes via the same atomic write path used by
//! `apply`. Once rolled back, the entry is removed.
//!
//! The format is intentionally self-describing JSON rather than a binary
//! encoding — it's human-auditable, versioned by a `schema` field, and
//! small enough at MVP scale that compression is not a concern yet. A more
//! efficient content-addressed format lands alongside the `pf-persist`
//! disk backend in a later phase.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const JOURNAL_DIR: &str = ".prolog-forge/journal";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitEntry {
    pub schema: u32,
    pub commit_id: String,
    pub timestamp_unix: u64,
    pub label: String,
    pub files: Vec<CommitFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitFile {
    pub path: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("commit not found: {0}")]
    NotFound(String),
    #[error("unsupported journal schema version: {0}")]
    UnsupportedSchema(u32),
}

pub fn journal_dir(root: &Path) -> PathBuf {
    root.join(JOURNAL_DIR)
}

pub fn entry_path(root: &Path, commit_id: &str) -> PathBuf {
    journal_dir(root).join(format!("{commit_id}.json"))
}

pub fn write(root: &Path, entry: &CommitEntry) -> Result<(), JournalError> {
    let dir = journal_dir(root);
    fs::create_dir_all(&dir)?;
    let path = entry_path(root, &entry.commit_id);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(entry)?;
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn read(root: &Path, commit_id: &str) -> Result<CommitEntry, JournalError> {
    let path = entry_path(root, commit_id);
    if !path.exists() {
        return Err(JournalError::NotFound(commit_id.to_string()));
    }
    let bytes = fs::read(&path)?;
    let entry: CommitEntry = serde_json::from_slice(&bytes)?;
    if entry.schema != SCHEMA_VERSION {
        return Err(JournalError::UnsupportedSchema(entry.schema));
    }
    Ok(entry)
}

pub fn delete(root: &Path, commit_id: &str) -> Result<(), JournalError> {
    let path = entry_path(root, commit_id);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn new_entry(commit_id: String, label: String, files: Vec<CommitFile>) -> CommitEntry {
    let timestamp_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    CommitEntry {
        schema: SCHEMA_VERSION,
        commit_id,
        timestamp_unix,
        label,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "pf-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn round_trip() {
        let root = tmp_root();
        let entry = new_entry(
            "commit-abc".into(),
            "test".into(),
            vec![CommitFile {
                path: "src/lib.rs".into(),
                before: "before".into(),
                after: "after".into(),
            }],
        );
        write(&root, &entry).unwrap();
        let got = read(&root, "commit-abc").unwrap();
        assert_eq!(got.commit_id, "commit-abc");
        assert_eq!(got.files.len(), 1);
        assert_eq!(got.files[0].before, "before");
        delete(&root, "commit-abc").unwrap();
        assert!(matches!(
            read(&root, "commit-abc"),
            Err(JournalError::NotFound(_))
        ));
    }
}
