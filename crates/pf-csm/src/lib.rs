//! Common Semantic Model — **v0**.
//!
//! Intentionally tiny. Phase 0 ships only the types needed to sketch the
//! contract between language analyzers and the rest of the Core. Phase 1
//! and beyond extend this with type systems, effects, and lifetimes.
//!
//! The CSM is **not** a language AST. It is a normalized, language-agnostic
//! representation destined to be flattened into facts for the knowledge graph.

use serde::{Deserialize, Serialize};

/// Stable, content-addressed identifier. In v0 this is just a string; in later
/// phases it will be a typed hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct NodeId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSpan {
    pub file: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Module,
    Package,
    File,
    Function,
    Type,
    Trait,
    Struct,
    Field,
    Variable,
    Macro,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Defines,
    Declares,
    Contains,
    References,
    Calls,
    Implements,
    Extends,
    Imports,
    DependsOn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: NodeId,
    pub kind: EntityKind,
    pub name: String,
    pub span: Option<SourceSpan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    pub kind: RelationKind,
    pub subject: NodeId,
    pub object: NodeId,
}

/// A complete CSM fragment emitted by one analyzer for one source unit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CsmFragment {
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
}

/// Trait every language analyzer must implement. Not used yet in Phase 0,
/// but fixes the shape early.
pub trait LanguageAnalyzer {
    /// A short stable identifier (e.g. `rust`, `typescript`, `python`).
    fn language(&self) -> &'static str;

    /// Parse one source unit and produce a CSM fragment.
    fn analyze(&self, source: &str, path: &str) -> Result<CsmFragment, AnalyzerError>;
}

#[derive(Debug)]
pub struct AnalyzerError {
    pub message: String,
    pub span: Option<SourceSpan>,
}

impl std::fmt::Display for AnalyzerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AnalyzerError {}
