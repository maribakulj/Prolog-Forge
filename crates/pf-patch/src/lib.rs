//! Patch planner.
//!
//! A patch is **not** a textual diff — it is a typed, structured plan of
//! operations on the Common Semantic Model. The planner turns a `PatchPlan`
//! into a `PatchPreview` by running each op against an in-memory map of
//! `{path -> source text}` and rendering a unified diff per affected file.
//!
//! Phase 1.3 step 1 ships:
//!   - `PatchOp::RenameFunction` — rename a function and every identifier
//!     occurrence bearing the same name, applied as byte-accurate textual
//!     edits driven by `syn` spans so comments and formatting are
//!     preserved.
//!   - `patch::preview` — pure function `(plan, files) -> PatchPreview`.
//!     Does not touch the filesystem. Does not mutate the graph.
//!
//! Application (write back to FS transactionally), validation, and
//! LLM-driven planning land in 1.3 step 2 and 1.4.

pub mod ops;
pub mod plan;
pub mod rust_rename;

pub use ops::{PatchOp, PatchPlan};
pub use plan::{preview, FilePatch, PatchError, PatchPreview};
