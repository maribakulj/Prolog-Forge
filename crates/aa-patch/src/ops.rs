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
    /// `crates/aa-patch/src/rust_rename.rs` for the full map): delegate
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
    /// Phase 1.22 — dual of [`InlineFunction`]. Take a contiguous run
    /// of statements inside a free-standing function body and lift it
    /// into a new free-standing helper, replacing the original site
    /// with a call to that helper.
    ///
    /// Deliberately narrow contract (see `crate::extract`):
    ///
    /// - The selection is given as a *line range* (1-indexed,
    ///   inclusive) inside `source_file`, and must cover *exactly* a
    ///   contiguous run of complete `Stmt`s in a free-standing fn —
    ///   the last one must not be the function's tail expression.
    ///   Empty / partial / non-contiguous selections are refused.
    /// - Control-flow leaks out of the selection are refused:
    ///   `return`, `break`, `continue`, `?`, `await`, `yield`. So is
    ///   any macro invocation (we cannot reason about token-stream
    ///   bodies safely yet).
    /// - The enclosing fn must itself be free-standing, with no
    ///   `self` / generics / `async` / `const` / `unsafe`.
    /// - **Parameters are explicit.** The caller (LLM or human) lists
    ///   `(name, type)` pairs the new fn should take. The transform
    ///   checks each `name` is a valid Rust ident, each `type` parses
    ///   as `syn::Type`, and each `name` is mentioned at least once
    ///   in the selection — but it does not try to *infer* types
    ///   from the source (that's a rust-analyzer job, deferred to a
    ///   later phase). The new fn always returns `()`; the call site
    ///   is rendered as a statement.
    /// - The new fn is inserted immediately after the enclosing fn
    ///   in the same file. A mandatory post-edit re-parse rejects
    ///   any rewrite that would not be valid Rust.
    ExtractFunction {
        /// Workspace-relative path of the file containing the range
        /// to extract.
        source_file: String,
        /// 1-indexed inclusive start line of the selection within
        /// `source_file`.
        start_line: u32,
        /// 1-indexed inclusive end line of the selection.
        end_line: u32,
        /// Name of the new helper to create.
        new_name: String,
        /// Explicit parameter list `(name, ty)` for the new helper.
        /// Each name must appear in the selection; each ty must parse
        /// as a Rust type.
        #[serde(default)]
        params: Vec<ExtractParam>,
        /// If empty, the op runs on every `.rs` file in the preview
        /// input (but only `source_file` is rewritten). Otherwise it
        /// is restricted to paths whose `relative` form matches one
        /// of the entries exactly.
        #[serde(default)]
        files: Vec<String>,
    },
    /// Phase 1.23 — reorder a free-standing function's parameters and
    /// optionally rename them. The transform rewrites both the
    /// signature and every bare call site so arguments stay aligned
    /// with the params they were always meant for.
    ///
    /// Deliberately narrow contract (see `crate::change_sig`):
    ///
    /// - The op is a *permutation only*. `new_params.len()` must
    ///   equal the function's current arity, and the multiset of
    ///   `from_index` values must be exactly `0..n`. Adding or
    ///   removing parameters is refused — those need different
    ///   contracts (default values, side-effect analysis on dropped
    ///   args) tracked as separate future ops.
    /// - The enclosing fn must be free-standing, with no `self` /
    ///   generics / `async` / `const` / `unsafe` / variadic.
    /// - Renames are syntactic. The transform refuses if the new
    ///   name would shadow another binding in the body, or if the
    ///   old name is itself shadowed before any use (the rename
    ///   would change semantics).
    /// - Macro-body call sites and qualified-path call sites
    ///   (`crate::f(...)`, `mod::f(...)`) are refused — same
    ///   posture as `InlineFunction`. Reordering only the bare
    ///   call sites would silently desync the qualified ones.
    /// - A mandatory post-edit `syn::parse_file` rejects any
    ///   rewrite that would not be valid Rust.
    ChangeSignature {
        /// Name of the target function. Must resolve to exactly one
        /// free-standing definition across `files` (or the whole
        /// workspace if `files` is empty).
        function: String,
        /// Permutation of the existing parameters, with optional
        /// renames. `new_params.len()` must equal the fn's arity;
        /// the `from_index` values must form a permutation of
        /// `0..n`. Each entry's `rename` is `None` to keep the
        /// param's existing name or `Some(new_name)` to change it.
        new_params: Vec<ParamReorder>,
        /// If empty, the op runs on every `.rs` file in the preview
        /// input. Otherwise it is restricted to paths whose
        /// `relative` form matches one of the entries exactly.
        #[serde(default)]
        files: Vec<String>,
    },
}

/// One `(name, type)` pair for [`PatchOp::ExtractFunction::params`].
/// Kept as a plain struct (not a tuple) so the wire shape is
/// self-describing and tolerates future fields (e.g. `mutable: bool`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractParam {
    pub name: String,
    /// Rust type as source text, e.g. `"i32"`, `"&str"`, `"Vec<u8>"`.
    /// Parsed with `syn::parse_str::<syn::Type>` at apply time.
    #[serde(rename = "type")]
    pub ty: String,
}

/// One slot in [`PatchOp::ChangeSignature::new_params`]. Each entry
/// names an existing param by its 0-indexed position and optionally
/// renames it; the absence of any "Add a fresh param" variant is the
/// 1.23 narrow contract speaking — additions land in a later phase
/// when default-value semantics are pinned down.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamReorder {
    /// 0-indexed position of the param in the *current* signature.
    pub from_index: usize,
    /// `Some(new_name)` to rename the param in the signature *and*
    /// every use inside the function body. `None` keeps the
    /// existing name. Renames are refused when shadowing would
    /// change observable semantics — see `crate::change_sig`.
    #[serde(default)]
    pub rename: Option<String>,
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
