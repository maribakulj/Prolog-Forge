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
