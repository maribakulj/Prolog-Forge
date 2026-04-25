//! Validation pipeline.
//!
//! Every `patch.apply` is gated by a pipeline of pluggable `ValidationStage`s
//! run against the shadow workspace (the `{path -> new_content}` map produced
//! by the patch planner). A patch only reaches the filesystem if every
//! mandatory stage reports `ok`.
//!
//! Phase 1.4 ships:
//!   - `ValidationStage` trait + `ValidationContext`,
//!   - `Pipeline` with stage composition and fail-fast semantics,
//!   - `SyntacticStage` for Rust source files (re-parses every changed
//!     `.rs` file with `syn`),
//!   - `ValidationReport` DTO for the wire protocol.
//!
//! Later phases will add a type stage (via rust-analyzer), a rule stage
//! (re-evaluating the rule engine on the shadow graph to detect constraint
//! violations), a behavioral stage (running impacted tests), and an
//! optional oracle stage (secondary LLM judge, advisory only).

pub mod pipeline;
pub mod stage;
pub mod stages;

pub use pipeline::{Pipeline, ValidationReport};
pub use stage::{Diagnostic, Severity, StageReport, ValidationContext, ValidationStage};
pub use stages::SyntacticStage;
