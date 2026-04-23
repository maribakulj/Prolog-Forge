//! Typed request / response payloads for every API method exposed by the Core.
//!
//! Method names are namespaced with dots (e.g. `graph.query`). The wire
//! representation of `params` and `result` is the JSON encoding of the types
//! below via serde.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------- session --------------------------------------------------------

pub const METHOD_INITIALIZE: &str = "session.initialize";
pub const METHOD_SHUTDOWN: &str = "session.shutdown";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCapabilities {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub client: ClientCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub name: String,
    pub version: String,
    pub protocol_version: String,
    pub methods: Vec<String>,
}

// ---------- workspace ------------------------------------------------------

pub const METHOD_WORKSPACE_OPEN: &str = "workspace.open";
pub const METHOD_WORKSPACE_STATUS: &str = "workspace.status";
pub const METHOD_WORKSPACE_INDEX: &str = "workspace.index";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceOpenParams {
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceOpenResult {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    pub workspace_id: WorkspaceId,
    pub root: String,
    pub fact_count: usize,
    pub rule_count: usize,
    pub derived_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceIndexParams {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceIndexResult {
    pub files_indexed: usize,
    pub files_failed: usize,
    pub entities: usize,
    pub relations: usize,
    pub facts_inserted: usize,
    pub errors: Vec<IndexingError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingError {
    pub file: String,
    pub message: String,
}

// ---------- graph ----------------------------------------------------------

pub const METHOD_GRAPH_INGEST: &str = "graph.ingestFact";
pub const METHOD_GRAPH_QUERY: &str = "graph.query";

/// A triple. Values are untyped strings in v0; typed atoms come in CSM v1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct FactDto {
    pub predicate: String,
    pub args: Vec<String>,
    /// Epistemic layer. Defaults to `observed` for externally ingested facts.
    #[serde(default)]
    pub layer: FactLayer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum FactLayer {
    #[default]
    Observed,
    Inferred,
    Candidate,
    Validated,
    Constraint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestFactParams {
    pub workspace_id: WorkspaceId,
    pub facts: Vec<FactDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestFactResult {
    pub inserted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryParams {
    pub workspace_id: WorkspaceId,
    /// A single atom pattern, e.g. `parent(X, bob)`. A richer query language
    /// lands in Phase 1; v0 supports pattern-match of one atom.
    pub pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    /// Each binding is a map from variable name to value.
    pub bindings: Vec<Value>,
    pub count: usize,
}

// ---------- rules ----------------------------------------------------------

pub const METHOD_RULES_LOAD: &str = "rules.load";
pub const METHOD_RULES_EVALUATE: &str = "rules.evaluate";
pub const METHOD_RULES_LIST: &str = "rules.list";

// ---------- llm -----------------------------------------------------------

pub const METHOD_LLM_PROPOSE: &str = "llm.propose";
pub const METHOD_LLM_REFINE: &str = "llm.refine";
pub const METHOD_LLM_PROPOSE_PATCH: &str = "llm.propose_patch";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProposeParams {
    pub workspace_id: WorkspaceId,
    pub intent: String,
    /// Entity id to use as the starting point for context extraction.
    pub anchor_id: String,
    #[serde(default = "default_hops")]
    pub hops: usize,
    #[serde(default = "default_max_facts")]
    pub max_facts: usize,
}

fn default_hops() -> usize {
    1
}
fn default_max_facts() -> usize {
    256
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProposeResult {
    pub accepted: usize,
    pub rejected: usize,
    pub cache_hit: bool,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub outcomes: Vec<ProposalOutcomeDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalOutcomeDto {
    pub predicate: String,
    pub args: Vec<String>,
    pub justification: String,
    pub accepted: bool,
    pub rejection_reason: Option<String>,
    /// Round index (0 for the first proposer pass, ≥ 1 for refinement
    /// rounds). Optional for backwards compatibility with `llm.propose`,
    /// which does not loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
}

// ---------- llm.refine ----------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRefineParams {
    pub workspace_id: WorkspaceId,
    pub intent: String,
    pub anchor_id: String,
    #[serde(default = "default_hops")]
    pub hops: usize,
    #[serde(default = "default_max_facts")]
    pub max_facts: usize,
    /// Cap on refinement rounds (including the initial proposer pass).
    /// The loop also breaks early on convergence (no rejections in a
    /// round). Default: 3.
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    /// Prior outcomes (typically from an earlier `llm.propose` or a failed
    /// `patch.apply`). Passed to the provider as structured feedback so the
    /// next round can drop hallucinations or pivot.
    #[serde(default)]
    pub prior_outcomes: Vec<ProposalOutcomeDto>,
    /// Validation diagnostics the refiner should take into account (for
    /// instance, the stage diagnostics from a rejected `patch.apply`). They
    /// are rendered into the prompt as structured feedback, not free text.
    #[serde(default)]
    pub prior_diagnostics: Vec<DiagnosticDto>,
}

fn default_max_rounds() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRefineResult {
    pub rounds: u32,
    pub converged: bool,
    pub final_accepted: usize,
    pub final_rejected: usize,
    pub tokens_in_total: u32,
    pub tokens_out_total: u32,
    /// All outcomes across all rounds, annotated with their `round` index.
    pub outcomes: Vec<ProposalOutcomeDto>,
    /// Per-round summary, parallel to the loop iterations actually run.
    pub rounds_summary: Vec<RefineRoundSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineRoundSummary {
    pub round: u32,
    pub accepted: usize,
    pub rejected: usize,
    pub cache_hit: bool,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

// ---------- llm.propose_patch ---------------------------------------------
//
// Asks the bounded LLM orchestrator to produce typed *patch plans* rather
// than fact candidates. Closes the LLM -> symbolic loop end-to-end: the
// returned plans are consumable by `patch.preview`, `patch.apply`, and
// `explain.patch` without any translation step.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProposePatchParams {
    pub workspace_id: WorkspaceId,
    pub intent: String,
    pub anchor_id: String,
    #[serde(default = "default_hops")]
    pub hops: usize,
    #[serde(default = "default_max_facts")]
    pub max_facts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProposePatchResult {
    pub accepted: usize,
    pub rejected: usize,
    pub cache_hit: bool,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub candidates: Vec<PatchCandidateDto>,
}

/// One proposed plan plus the LLM's reason for suggesting it and, when
/// applicable, the symbolic grounding reason for rejecting it. The plan
/// is the exact wire shape accepted by `patch.preview` / `patch.apply` /
/// `explain.patch` — no translation step required on the caller side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchCandidateDto {
    pub plan: PatchPlanDto,
    pub justification: String,
    pub accepted: bool,
    pub rejection_reason: Option<String>,
}

// ---------- explain.patch -------------------------------------------------

pub const METHOD_EXPLAIN_PATCH: &str = "explain.patch";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainPatchParams {
    pub workspace_id: WorkspaceId,
    pub plan: PatchPlanDto,
    /// Optional candidate outcomes to cite in the explanation (typically
    /// forwarded from a recent `llm.propose` / `llm.refine`).
    #[serde(default)]
    pub candidate_outcomes: Vec<ProposalOutcomeDto>,
    /// Same vocabulary as `PatchApplyParams::validation_profile`. Runs
    /// the requested pipeline against the shadow graph to populate the
    /// explanation's stage evidence. FS is not mutated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainPatchResult {
    pub plan_label: String,
    pub anchors: Vec<String>,
    pub verdict: VerdictDto,
    pub evidence: Vec<EvidenceNodeDto>,
    pub stats: ExplanationStatsDto,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerdictDto {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceNodeDto {
    Observed {
        predicate: String,
        args: Vec<String>,
        role: String,
    },
    Inferred {
        predicate: String,
        args: Vec<String>,
    },
    RuleActivation {
        rule_index: usize,
        head: PremiseFactDto,
        premises: Vec<PremiseFactDto>,
    },
    Candidate {
        predicate: String,
        args: Vec<String>,
        justification: String,
        accepted: bool,
        rejection_reason: Option<String>,
        round: Option<u32>,
    },
    Stage {
        name: String,
        ok: bool,
        diagnostics: Vec<DiagnosticDto>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PremiseFactDto {
    pub predicate: String,
    pub args: Vec<String>,
    pub layer: FactLayer,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExplanationStatsDto {
    pub anchors: usize,
    pub observed_cited: usize,
    pub inferred_cited: usize,
    pub rule_activations: usize,
    pub candidates_considered: usize,
    pub stages_run: usize,
}

// ---------- patch ---------------------------------------------------------

pub const METHOD_PATCH_PREVIEW: &str = "patch.preview";
pub const METHOD_PATCH_APPLY: &str = "patch.apply";
pub const METHOD_PATCH_ROLLBACK: &str = "patch.rollback";

/// Wire shape of a patch plan. The `op` field tags the variant, matching the
/// `#[serde(tag = "op")]` enum in `pf-patch`. Kept as `Value` at the
/// protocol boundary so new op kinds do not break older clients: the server
/// decodes and rejects unknown ops, the JSON-RPC schema only guarantees
/// `ops: Array<Object>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchPlanDto {
    pub ops: Vec<Value>,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchPreviewParams {
    pub workspace_id: WorkspaceId,
    pub plan: PatchPlanDto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchPreviewResult {
    pub total_replacements: usize,
    pub files: Vec<FilePatchDto>,
    pub errors: Vec<FilePatchError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePatchDto {
    pub path: String,
    pub before_len: usize,
    pub after_len: usize,
    pub replacements: usize,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePatchError {
    pub file: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchApplyParams {
    pub workspace_id: WorkspaceId,
    pub plan: PatchPlanDto,
    /// Validation profile name. `"default"` (or `None`) runs the
    /// syntactic + rule stages. `"typed"` additionally runs the
    /// `cargo_check` stage; `cargo` must be on `PATH`. See
    /// `docs/protocol.md#validation-profiles`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchApplyResult {
    pub applied: bool,
    pub commit_id: Option<String>,
    pub files_written: usize,
    pub bytes_written: u64,
    pub total_replacements: usize,
    pub validation: ValidationReportDto,
    /// `None` when the patch applied cleanly, `Some(reason)` when it was
    /// rejected (validation failure, preflight mismatch, or rollback).
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationReportDto {
    pub ok: bool,
    pub stages: Vec<StageReportDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageReportDto {
    pub stage: String,
    pub ok: bool,
    pub diagnostics: Vec<DiagnosticDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticDto {
    pub severity: String,
    pub file: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchRollbackParams {
    pub workspace_id: WorkspaceId,
    pub commit_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchRollbackResult {
    pub rolled_back: bool,
    pub commit_id: String,
    pub files_restored: usize,
    pub label: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesLoadParams {
    pub workspace_id: WorkspaceId,
    /// Datalog source text. See docs/rules-dsl.md.
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesLoadResult {
    pub rules_added: usize,
    pub facts_added: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesEvaluateParams {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesEvaluateResult {
    pub derived: usize,
    pub iterations: usize,
}

// ---------- memory --------------------------------------------------------
//
// Queryable view of the runtime's commit journal. Turns the pile of
// `<root>/.prolog-forge/journal/*.json` entries into a first-class
// surface: `memory.history` (log-style list with metadata), `memory.get`
// (full entry including before/after bytes), `memory.stats` (aggregates:
// by op kind, by validation profile, top-N edited files). Addresses the
// "il manque une mémoire exploitable" critique from the original
// neuro-symbolic review.

pub const METHOD_MEMORY_HISTORY: &str = "memory.history";
pub const METHOD_MEMORY_GET: &str = "memory.get";
pub const METHOD_MEMORY_STATS: &str = "memory.stats";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHistoryParams {
    pub workspace_id: WorkspaceId,
    /// Return only entries whose label starts with this prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_prefix: Option<String>,
    /// Return only entries whose `ops_summary` contains this tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_tag: Option<String>,
    /// Return only entries with this validation profile (`default`,
    /// `typed`, `tested`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_profile: Option<String>,
    /// Cap the response size. `None` means all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHistoryResult {
    pub items: Vec<MemoryHistoryItemDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHistoryItemDto {
    pub commit_id: String,
    pub timestamp_unix: u64,
    pub label: String,
    pub files_changed: usize,
    pub bytes_after: u64,
    pub ops_summary: Vec<String>,
    pub validation_profile: Option<String>,
    pub total_replacements: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryGetParams {
    pub workspace_id: WorkspaceId,
    pub commit_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryGetResult {
    pub commit_id: String,
    pub timestamp_unix: u64,
    pub label: String,
    pub ops_summary: Vec<String>,
    pub validation_profile: Option<String>,
    pub total_replacements: usize,
    pub files: Vec<CommitFileDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitFileDto {
    pub path: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStatsParams {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryStatsResult {
    pub commits: usize,
    pub files_touched: usize,
    pub by_op_kind: std::collections::BTreeMap<String, usize>,
    pub by_validation_profile: std::collections::BTreeMap<String, usize>,
    pub top_files: Vec<MemoryTopFileDto>,
    pub first_commit_at: Option<u64>,
    pub last_commit_at: Option<u64>,
    pub total_bytes_written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryTopFileDto {
    pub path: String,
    pub commit_count: usize,
}
