//! Wire-facing types for an [`Explanation`].
//!
//! Kept in a dedicated module so the data shape is obvious at a glance and
//! so consumers (JSON-RPC adapter, CLI, web explainer) can import it without
//! pulling the builder.

use pf_protocol::FactLayer;
use serde::{Deserialize, Serialize};

/// Final judgment on a patch plan, synthesized from the validation report
/// and (optionally) the commit outcome.
///
/// `NotProven` is used when the validation pipeline is thin enough that a
/// green verdict cannot be interpreted as a proof (e.g. no rule stage was
/// available). It is the honest middle ground between `Accepted` and
/// `Rejected` and is what the explainer should report for low-evidence
/// situations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    Accepted {
        commit_id: Option<String>,
        notes: Vec<String>,
    },
    Rejected {
        reason: String,
        failing_stages: Vec<String>,
    },
    NotProven {
        reason: String,
    },
}

/// A flat, pre-ordered evidence stream. The order is part of the contract:
/// observed facts first, then candidates considered, then rule activations,
/// then validator stages. A reader can render it directly or group by kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceNode {
    /// Ground fact pulled from the graph because it mentions one of the
    /// plan's anchor identifiers.
    Observed {
        predicate: String,
        args: Vec<String>,
        role: String,
    },
    /// Fact previously derived by the rule engine and now relevant to the
    /// plan. Without running the tracer we only report the fact itself;
    /// the tracer attaches the rule + premises through [`RuleActivation`].
    Inferred {
        predicate: String,
        args: Vec<String>,
    },
    /// A rule activation captured by `pf_rules::trace_derivations` whose
    /// head or premises touch an anchor.
    RuleActivation {
        rule_index: usize,
        head: PremiseFact,
        premises: Vec<PremiseFact>,
    },
    /// Hypothesis previously produced by the LLM orchestrator. The
    /// explainer cites candidates both to show "what was considered" and
    /// to surface why it was accepted or rejected.
    Candidate(CandidateRef),
    /// Summary of one validation stage's verdict + diagnostics.
    Stage {
        name: String,
        ok: bool,
        diagnostics: Vec<StageDiagnostic>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PremiseFact {
    pub predicate: String,
    pub args: Vec<String>,
    pub layer: FactLayer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRef {
    pub predicate: String,
    pub args: Vec<String>,
    pub justification: String,
    pub accepted: bool,
    pub rejection_reason: Option<String>,
    pub round: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageDiagnostic {
    pub severity: String,
    pub file: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExplanationStats {
    pub anchors: usize,
    pub observed_cited: usize,
    pub inferred_cited: usize,
    pub rule_activations: usize,
    pub candidates_considered: usize,
    pub stages_run: usize,
}

/// The complete justification for a plan's verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Explanation {
    pub plan_label: String,
    pub anchors: Vec<String>,
    pub verdict: Verdict,
    pub evidence: Vec<EvidenceNode>,
    pub stats: ExplanationStats,
    /// Human-readable single-sentence summary — convenient for logs and
    /// CLI headers; adapters may regenerate their own.
    pub summary: String,
}
