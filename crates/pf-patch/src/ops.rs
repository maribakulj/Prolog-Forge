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
    /// Step 2 of the type-aware rename ladder (see
    /// `crates/pf-patch/src/rust_rename.rs` for the full map): delegate
    /// the rename to `rust-analyzer` via LSP. The caller names a
    /// declaration site (file + 0-indexed line/character of any
    /// occurrence of the symbol) and the new identifier; RA returns the
    /// exact set of scope-resolved text edits to apply.
    ///
    /// This variant requires `rust-analyzer` on `PATH`. When it is
    /// absent, the patch pipeline degrades gracefully: the op is
    /// skipped and a diagnostic explains why (same pattern used by
    /// `CargoCheckStage` when `cargo` is missing).
    RenameFunctionTyped {
        /// Workspace-relative path of any file that contains an
        /// occurrence of the symbol (typically the declaration site).
        decl_file: String,
        /// 0-indexed line within `decl_file`.
        decl_line: u32,
        /// 0-indexed character offset within the line. Must fall inside
        /// the identifier so rust-analyzer can resolve the symbol.
        decl_character: u32,
        /// New identifier name.
        new_name: String,
        /// Informative only — the old name is not needed by RA (it
        /// resolves by position) but keeping it in the wire shape makes
        /// the op self-describing in logs and proof trees.
        #[serde(default)]
        old_name: String,
    },
    /// Add one or more trait names to the `#[derive(...)]` attribute of
    /// a struct, enum, or union. Merges into the first existing
    /// `#[derive(...)]` attribute if there is one; otherwise inserts a
    /// fresh `#[derive(...)]` line immediately above the `struct` /
    /// `enum` / `union` keyword. Duplicates (a derive already listed on
    /// the target) are skipped — the op is idempotent.
    ///
    /// The transform is syn-driven: structured parse, span-located
    /// byte edit, and a mandatory post-edit re-parse that rejects any
    /// rewrite that would break Rust syntax. Strings, comments, macro
    /// bodies and unrelated attributes are never touched.
    AddDeriveToStruct {
        /// Name of the target type (struct / enum / union).
        type_name: String,
        /// Trait names to add. Each must be a valid Rust identifier or
        /// a path like `serde::Serialize`. Duplicates with existing
        /// derives on the target are skipped.
        derives: Vec<String>,
        /// If empty, the op runs on every `.rs` file in the preview
        /// input. Otherwise it is restricted to paths whose
        /// `relative` form matches one of the entries exactly.
        #[serde(default)]
        files: Vec<String>,
    },
    /// Dual of [`AddDeriveToStruct`]: remove one or more trait names
    /// from the target type's `#[derive(...)]` attribute. If every
    /// listed derive is absent, the op is a no-op (idempotent — dual
    /// of the add-op's duplicate-skip). If the filter empties the
    /// derive list entirely, the whole `#[derive(...)]` attribute
    /// line is deleted, trailing newline included, so the source
    /// never grows a `#[derive()]` stub.
    ///
    /// Unlisted derives on the target are preserved verbatim;
    /// multiple `#[derive]` attributes on the same item are tolerated
    /// but only the first is edited (same conservative posture as the
    /// add-op).
    RemoveDeriveFromStruct {
        type_name: String,
        /// Trait names to drop. Whitespace-insensitive comparison.
        derives: Vec<String>,
        #[serde(default)]
        files: Vec<String>,
    },
    /// Substitute every call site of a free-standing function with the
    /// function's body, wrapped in a block that binds every formal
    /// parameter to its actual argument, and then remove the function
    /// definition. First Phase-1.21 op; extends the patch algebra
    /// beyond "modify-in-place" into "replace-and-delete".
    ///
    /// Deliberately narrow contract (see `crate::inline`): free-standing
    /// fn, no `self`, no generics, no `async`/`const`/`unsafe`, no
    /// `return` in body, non-recursive, not called inside any macro body
    /// in scope. The transform refuses ambiguity rather than produce a
    /// half-inlined program.
    InlineFunction {
        /// Name of the function to inline. Must resolve to exactly one
        /// free-standing definition across `files` (or the whole
        /// workspace if `files` is empty).
        function: String,
        /// If empty, the op runs on every `.rs` file in the preview
        /// input. Otherwise it is restricted to paths whose
        /// `relative` form matches one of the entries exactly.
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
