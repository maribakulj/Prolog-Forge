//! Proof-carrying explanations.
//!
//! Every patch in AYE-AYE can be rejected or accepted. The epistemic
//! layers, the rule engine, the LLM orchestrator and the validation pipeline
//! each contribute a slice of the reasoning behind that verdict; on their
//! own none of them makes the decision legible. `aa-explain` composes those
//! slices into an [`Explanation`] — a structured, serializable justification
//! that pairs with the patch itself.
//!
//! The output is deliberately shaped for two consumers:
//!
//! - **Humans**, through the CLI and (later) the web explainer: an ordered
//!   evidence stream reads like "these facts were observed; these rules
//!   fired; these hypotheses were considered; these validators ran; verdict".
//! - **Machines**, through JSON-RPC: an adapter can re-render the same data
//!   as a proof tree, a changelog entry, or a filter for future proposals.
//!
//! This crate is pure: it accepts an [`ExplainInput`] and returns a value,
//! never touching the filesystem or mutating the graph. It depends only on
//! `aa-graph`, `aa-rules`, and `aa-protocol`.

pub mod builder;
pub mod model;

pub use builder::{build, ExplainInput, ValidationStageRef};
pub use model::{
    CandidateRef, EvidenceNode, Explanation, ExplanationStats, PremiseFact, StageDiagnostic,
    Verdict,
};
