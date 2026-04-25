//! Repo memory surface — queryable view of the commit journal.
//!
//! The journal at `<root>/.aye-aye/journal/*.json` is the runtime's
//! memory of every `patch.apply` that ever landed. Until Phase 1.14 that
//! memory was internal plumbing: `patch.rollback` read entries by id,
//! nobody else could. This module exposes it.
//!
//! `memory.history` lists metadata for every committed patch (no file
//! bodies — those can be huge). `memory.get` fetches the full entry
//! including before/after bytes. `memory.stats` aggregates: how many
//! commits, how many of each op kind, which validation profile was
//! used, which files get touched most often, which rules (via future
//! work) fire the most. Together they turn the runtime from a tool
//! that executes requests into one that can answer "what have I done
//! on this repo" — the "il manque une mémoire" critique point from
//! the original review.
//!
//! The surface is read-only today. Promoting commits into the
//! `validated` epistemic layer (per-repo learning) is a Phase 3 item;
//! for now the memory is descriptive, not prescriptive.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::journal::{list as list_entries, read as read_entry, CommitEntry, JournalError};

/// Filter applied to `memory.history`. All fields are ANDed; missing
/// fields match everything. Kept intentionally narrow — complex
/// queries belong in the future graph-backed memory surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryFilter {
    /// Match only entries whose `label` starts with this prefix.
    /// Useful for "show all typed renames" style queries.
    #[serde(default)]
    pub label_prefix: Option<String>,
    /// Match only entries that contain this op tag in their
    /// `ops_summary`.
    #[serde(default)]
    pub op_tag: Option<String>,
    /// Match only entries whose `validation_profile` equals this.
    #[serde(default)]
    pub validation_profile: Option<String>,
    /// Maximum entries to return. `None` means all.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Lightweight history item — no file bodies, just metadata. The full
/// entry is available via `memory.get` when the caller wants to
/// inspect the actual bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub commit_id: String,
    pub timestamp_unix: u64,
    pub label: String,
    pub files_changed: usize,
    pub bytes_after: u64,
    pub ops_summary: Vec<String>,
    pub validation_profile: Option<String>,
    pub total_replacements: usize,
}

impl From<&CommitEntry> for HistoryItem {
    fn from(e: &CommitEntry) -> Self {
        let bytes_after: u64 = e.files.iter().map(|f| f.after.len() as u64).sum();
        Self {
            commit_id: e.commit_id.clone(),
            timestamp_unix: e.timestamp_unix,
            label: e.label.clone(),
            files_changed: e.files.len(),
            bytes_after,
            ops_summary: e.ops_summary.clone(),
            validation_profile: e.validation_profile.clone(),
            total_replacements: e.total_replacements,
        }
    }
}

/// Aggregate stats over the full journal. Shape is optimised for a
/// single JSON-RPC response — every field is a plain scalar or a flat
/// map, nothing nested beyond what a dashboard can render without
/// recursion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryStats {
    pub commits: usize,
    pub files_touched: usize,
    /// commit count by op tag (`rename_function` → 7, etc.).
    pub by_op_kind: BTreeMap<String, usize>,
    /// commit count by validation profile (`default` / `typed` /
    /// `tested` / `unknown`). `unknown` covers pre-1.14 entries where
    /// the field was not recorded.
    pub by_validation_profile: BTreeMap<String, usize>,
    /// Top files by edit count. Bounded at 20 entries to keep the
    /// payload predictable; full per-file counts are queryable by
    /// replaying the history stream.
    pub top_files: Vec<(String, usize)>,
    /// `None` when the journal is empty.
    pub first_commit_at: Option<u64>,
    pub last_commit_at: Option<u64>,
    /// Total bytes written after across every commit. A rough proxy
    /// for "how much editing did the runtime do here".
    pub total_bytes_written: u64,
}

pub fn history(root: &Path, filter: &HistoryFilter) -> Result<Vec<HistoryItem>, JournalError> {
    let all = list_entries(root)?;
    let mut items: Vec<HistoryItem> = all
        .iter()
        .filter(|e| {
            if let Some(prefix) = &filter.label_prefix {
                if !e.label.starts_with(prefix) {
                    return false;
                }
            }
            if let Some(tag) = &filter.op_tag {
                if !e.ops_summary.iter().any(|t| t == tag) {
                    return false;
                }
            }
            if let Some(profile) = &filter.validation_profile {
                if e.validation_profile.as_deref() != Some(profile.as_str()) {
                    return false;
                }
            }
            true
        })
        .map(HistoryItem::from)
        .collect();
    // History is presented newest-first so the list reads like a log.
    items.sort_by_key(|item| std::cmp::Reverse(item.timestamp_unix));
    if let Some(n) = filter.limit {
        items.truncate(n);
    }
    Ok(items)
}

pub fn get(root: &Path, commit_id: &str) -> Result<CommitEntry, JournalError> {
    read_entry(root, commit_id)
}

pub fn stats(root: &Path) -> Result<MemoryStats, JournalError> {
    let entries = list_entries(root)?;
    if entries.is_empty() {
        return Ok(MemoryStats::default());
    }
    let mut by_op_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_profile: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_file: BTreeMap<String, usize> = BTreeMap::new();
    let mut total_bytes: u64 = 0;
    let mut first: Option<u64> = None;
    let mut last: Option<u64> = None;
    for e in &entries {
        for tag in &e.ops_summary {
            *by_op_kind.entry(tag.clone()).or_insert(0) += 1;
        }
        let profile = e
            .validation_profile
            .clone()
            .unwrap_or_else(|| "unknown".into());
        *by_profile.entry(profile).or_insert(0) += 1;
        for f in &e.files {
            *per_file.entry(f.path.clone()).or_insert(0) += 1;
            total_bytes += f.after.len() as u64;
        }
        first = Some(first.map_or(e.timestamp_unix, |v| v.min(e.timestamp_unix)));
        last = Some(last.map_or(e.timestamp_unix, |v| v.max(e.timestamp_unix)));
    }
    let mut top_files: Vec<(String, usize)> = per_file.into_iter().collect();
    top_files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top_files.truncate(20);
    Ok(MemoryStats {
        commits: entries.len(),
        files_touched: top_files.len(),
        by_op_kind,
        by_validation_profile: by_profile,
        top_files,
        first_commit_at: first,
        last_commit_at: last,
        total_bytes_written: total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{new_entry_with_stats, write, CommitFile};
    use std::fs;

    fn tmp_root() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "aa-memory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn seed(root: &Path, commit_id: &str, label: &str, ops: &[&str], profile: Option<&str>) {
        let entry = new_entry_with_stats(
            commit_id.into(),
            label.into(),
            vec![CommitFile {
                path: "src/lib.rs".into(),
                before: "before".into(),
                after: "after".into(),
            }],
            ops.iter().map(|s| s.to_string()).collect(),
            profile.map(|s| s.to_string()),
            ops.len(),
        );
        write(root, &entry).unwrap();
    }

    #[test]
    fn history_empty_when_no_commits() {
        let root = tmp_root();
        let items = history(&root, &HistoryFilter::default()).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn history_sorted_newest_first() {
        let root = tmp_root();
        seed(&root, "c1", "first", &["rename_function"], Some("default"));
        // Small sleep so timestamps differ on fast filesystems.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        seed(
            &root,
            "c2",
            "second",
            &["add_derive_to_struct"],
            Some("typed"),
        );
        let items = history(&root, &HistoryFilter::default()).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].commit_id, "c2");
        assert_eq!(items[1].commit_id, "c1");
    }

    #[test]
    fn history_filters_by_op_tag_and_profile() {
        let root = tmp_root();
        seed(&root, "c1", "first", &["rename_function"], Some("default"));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        seed(
            &root,
            "c2",
            "second",
            &["add_derive_to_struct"],
            Some("typed"),
        );
        let only_rename = history(
            &root,
            &HistoryFilter {
                op_tag: Some("rename_function".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(only_rename.len(), 1);
        assert_eq!(only_rename[0].commit_id, "c1");
        let only_typed = history(
            &root,
            &HistoryFilter {
                validation_profile: Some("typed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(only_typed.len(), 1);
        assert_eq!(only_typed[0].commit_id, "c2");
    }

    #[test]
    fn stats_aggregate_shape() {
        let root = tmp_root();
        seed(&root, "c1", "first", &["rename_function"], Some("default"));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        seed(&root, "c2", "second", &["rename_function"], Some("typed"));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        seed(
            &root,
            "c3",
            "third",
            &["add_derive_to_struct"],
            Some("default"),
        );
        let s = stats(&root).unwrap();
        assert_eq!(s.commits, 3);
        assert_eq!(s.by_op_kind["rename_function"], 2);
        assert_eq!(s.by_op_kind["add_derive_to_struct"], 1);
        assert_eq!(s.by_validation_profile["default"], 2);
        assert_eq!(s.by_validation_profile["typed"], 1);
        // `src/lib.rs` touched by every commit.
        assert_eq!(s.top_files[0].1, 3);
        assert!(s.first_commit_at.is_some());
        assert!(s.last_commit_at >= s.first_commit_at);
    }

    #[test]
    fn get_returns_full_entry_with_bodies() {
        let root = tmp_root();
        seed(&root, "c1", "first", &["rename_function"], Some("default"));
        let got = get(&root, "c1").unwrap();
        assert_eq!(got.commit_id, "c1");
        assert_eq!(got.files[0].before, "before");
        assert_eq!(got.files[0].after, "after");
        assert_eq!(got.ops_summary, vec!["rename_function".to_string()]);
        assert_eq!(got.validation_profile.as_deref(), Some("default"));
    }
}
