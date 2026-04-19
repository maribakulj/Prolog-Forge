//! Filesystem ingestion.
//!
//! Phase 1 step 1: a simple recursive walker that yields source files keyed
//! by a stable language identifier. `.gitignore` / `.ignore` awareness is
//! deferred to a later phase (via `ignore::Walk`).
//!
//! Target directories (`target/`, `node_modules/`, hidden directories) are
//! skipped by default; the skip list is extensible.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: PathBuf,
    pub relative: PathBuf,
    pub language: &'static str,
}

#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// Directory names to skip entirely (any depth).
    pub skip_dirs: Vec<String>,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            skip_dirs: vec![
                "target".into(),
                "node_modules".into(),
                ".git".into(),
                "build".into(),
                "dist".into(),
            ],
        }
    }
}

pub fn walk(root: impl AsRef<Path>, opts: &IngestOptions) -> Vec<SourceFile> {
    let root = root.as_ref();
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            if name.starts_with('.') && e.depth() > 0 {
                return false;
            }
            if e.file_type().is_dir() && opts.skip_dirs.iter().any(|s| s == name.as_ref()) {
                return false;
            }
            true
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if let Some(lang) = classify(entry.path()) {
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_path_buf();
            out.push(SourceFile {
                path: entry.path().to_path_buf(),
                relative,
                language: lang,
            });
        }
    }
    out
}

fn classify(path: &Path) -> Option<&'static str> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    match ext {
        "rs" => Some("rust"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn walks_and_classifies_rust() {
        let tmp = tempdir();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::create_dir_all(tmp.join("target")).unwrap();
        fs::write(tmp.join("src/lib.rs"), "fn main(){}").unwrap();
        fs::write(tmp.join("target/junk.rs"), "fn main(){}").unwrap();
        fs::write(tmp.join("README.md"), "x").unwrap();
        let files = walk(&tmp, &IngestOptions::default());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].language, "rust");
        assert!(files[0].relative.to_string_lossy().ends_with("lib.rs"));
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("pf-ingest-{}", rand_suffix()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }
}
