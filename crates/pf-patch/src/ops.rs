//! Typed patch operations.
//!
//! Each variant is a coarse-grained, auditable edit. Ops are designed to be
//! reviewable by a human in a diff panel and to compose well: the planner
//! applies them sequentially against an accumulating shadow file map.

use serde::{Deserialize, Serialize};

/// A single patch operation. Tagged-enum JSON with `op` as the tag, so
/// clients can construct plans without a Rust build.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchOp {
    /// Rename every identifier literally matching `old_name` to `new_name`
    /// across the selected files. Driven by `syn` spans so strings and
    /// comments are not touched; a post-edit re-parse rejects the operation
    /// if it would break syntax.
    ///
    /// Phase 1.3 step 1 does **not** perform scope-aware resolution: a
    /// shadow variable with the same name is renamed too. A scope-aware
    /// implementation lands once the type-aware Rust analyzer arrives
    /// (Phase 2).
    RenameFunction {
        old_name: String,
        new_name: String,
        /// If empty, the op runs on every file in the preview input.
        /// Otherwise it is restricted to paths whose `relative` form
        /// matches one of the entries exactly.
        #[serde(default)]
        files: Vec<String>,
    },
}

/// A `PatchPlan` is an ordered sequence of ops plus auditable metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchPlan {
    pub ops: Vec<PatchOp>,
    /// Free-text label used in diff headers and provenance entries.
    #[serde(default)]
    pub label: String,
}

impl PatchPlan {
    pub fn new(ops: Vec<PatchOp>) -> Self {
        Self {
            ops,
            label: String::new(),
        }
    }

    pub fn labelled(ops: Vec<PatchOp>, label: impl Into<String>) -> Self {
        Self {
            ops,
            label: label.into(),
        }
    }
}
