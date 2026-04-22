//! Transactional patch apply.
//!
//! Given a shadow file map `{relative_path -> new_content}` produced by the
//! planner, this module:
//!
//! 1. **Preflight** — verifies that the current on-disk content of every
//!    target file matches the `original_files` the plan was rendered
//!    against. If not, a concurrent edit landed since the preview and we
//!    refuse to write (optimistic concurrency).
//! 2. **Backup** — reads the current on-disk bytes for every target into
//!    an in-memory map, so a rollback can restore them if a later write
//!    fails.
//! 3. **Write** — for each target, writes a sibling temp file and
//!    `rename`s it over the target. Rename is atomic on POSIX filesystems.
//! 4. **Rollback on failure** — if any step in (3) fails, every file
//!    already renamed is restored from its backup bytes, every lingering
//!    temp is removed.
//!
//! The commit id returned is a random nonce useful for provenance and (in
//! Phase 2) on-disk journal lookup.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("preflight failed: {0}")]
    Preflight(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("write failed and rollback attempted: {reason}")]
    RolledBack { reason: String },
}

#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub commit_id: String,
    pub files_written: usize,
    pub bytes_written: u64,
}

pub fn apply_transactional(
    root: &Path,
    shadow: &BTreeMap<String, String>,
    original: &BTreeMap<String, String>,
) -> Result<ApplyOutcome, ApplyError> {
    // Select the files that actually change (shadow differs from original).
    let targets: Vec<(&String, &String, &String)> = shadow
        .iter()
        .filter_map(|(rel, new_content)| {
            let before = original.get(rel)?;
            if before == new_content {
                None
            } else {
                Some((rel, before, new_content))
            }
        })
        .collect();

    // 1. Preflight: verify on-disk content matches `original`.
    for (rel, before, _) in &targets {
        let p = root.join(rel);
        let on_disk = fs::read_to_string(&p)
            .map_err(|e| ApplyError::Preflight(format!("reading {}: {}", p.display(), e)))?;
        if on_disk != **before {
            return Err(ApplyError::Preflight(format!(
                "workspace changed since preview: {}",
                rel
            )));
        }
    }

    // 2. Backup current bytes (same as `before`, but re-read to be strict).
    let mut backups: Vec<(PathBuf, String)> = Vec::with_capacity(targets.len());
    for (rel, _before, _new) in &targets {
        let p = root.join(rel);
        let bytes = fs::read_to_string(&p)?;
        backups.push((p, bytes));
    }

    // 3. Write through temp files + atomic rename.
    let commit_id = new_commit_id();
    let mut written: Vec<PathBuf> = Vec::new();
    let mut total_bytes: u64 = 0;

    for ((rel, _before, new_content), (target_path, _backup)) in targets.iter().zip(backups.iter())
    {
        let tmp_path = tmp_for(target_path, &commit_id);
        // Write the temp file.
        if let Err(e) = fs::write(&tmp_path, new_content) {
            rollback(&written, &backups);
            let _ = fs::remove_file(&tmp_path);
            return Err(ApplyError::RolledBack {
                reason: format!("temp write for {}: {}", rel, e),
            });
        }
        // Atomic rename over the target.
        if let Err(e) = fs::rename(&tmp_path, target_path) {
            let _ = fs::remove_file(&tmp_path);
            rollback(&written, &backups);
            return Err(ApplyError::RolledBack {
                reason: format!("rename for {}: {}", rel, e),
            });
        }
        total_bytes += new_content.len() as u64;
        written.push(target_path.clone());
    }

    Ok(ApplyOutcome {
        commit_id,
        files_written: written.len(),
        bytes_written: total_bytes,
    })
}

fn rollback(written: &[PathBuf], backups: &[(PathBuf, String)]) {
    // Build a path -> original-content lookup for quick restore.
    let lookup: BTreeMap<&PathBuf, &String> = backups.iter().map(|(p, c)| (p, c)).collect();
    for p in written {
        if let Some(content) = lookup.get(p) {
            let _ = fs::write(p, content.as_bytes());
        }
    }
}

fn tmp_for(target: &Path, nonce: &str) -> PathBuf {
    let mut file_name = target.file_name().unwrap_or_default().to_os_string();
    file_name.push(format!(".pf-tmp-{}", nonce));
    match target.parent() {
        Some(dir) => dir.join(file_name),
        None => PathBuf::from(file_name),
    }
}

fn new_commit_id() -> String {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("commit-{ns:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "pf-apply-{}-{}",
            std::process::id(),
            new_commit_id()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn happy_path_writes_atomically() {
        let root = tmpdir();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "fn a(){}").unwrap();
        fs::write(root.join("src/b.rs"), "fn b(){}").unwrap();

        let mut original = BTreeMap::new();
        original.insert("src/a.rs".into(), "fn a(){}".into());
        original.insert("src/b.rs".into(), "fn b(){}".into());

        let mut shadow = BTreeMap::new();
        shadow.insert("src/a.rs".into(), "fn a2(){}".into());
        shadow.insert("src/b.rs".into(), "fn b2(){}".into());

        let out = apply_transactional(&root, &shadow, &original).unwrap();
        assert_eq!(out.files_written, 2);
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "fn a2(){}"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/b.rs")).unwrap(),
            "fn b2(){}"
        );
    }

    #[test]
    fn preflight_fails_on_external_change() {
        let root = tmpdir();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "drifted").unwrap();

        let mut original = BTreeMap::new();
        original.insert("src/a.rs".into(), "fn a(){}".into()); // stale preview
        let mut shadow = BTreeMap::new();
        shadow.insert("src/a.rs".into(), "fn a2(){}".into());

        let err = apply_transactional(&root, &shadow, &original).unwrap_err();
        assert!(matches!(err, ApplyError::Preflight(_)));
        // File on disk must be untouched.
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "drifted"
        );
    }
}
