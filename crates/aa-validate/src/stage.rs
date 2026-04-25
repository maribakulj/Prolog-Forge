//! `ValidationStage` trait + supporting types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Input passed to every stage. Stages do **not** mutate the context; they
/// only report. Pipelines that need cross-stage side effects (e.g. to pass
/// inferred facts between stages) will grow explicit channels in a later
/// phase.
pub struct ValidationContext<'a> {
    /// The proposed file contents after the plan has been applied.
    /// Keyed by workspace-relative path.
    pub shadow_files: &'a BTreeMap<String, String>,
    /// The file contents the plan was rendered against. A stage that cares
    /// about deltas can diff `shadow_files` against this map.
    pub original_files: &'a BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub file: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageReport {
    pub stage: String,
    pub ok: bool,
    pub diagnostics: Vec<Diagnostic>,
}

impl StageReport {
    pub fn ok(stage: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            ok: true,
            diagnostics: Vec::new(),
        }
    }

    pub fn with_errors(stage: impl Into<String>, diagnostics: Vec<Diagnostic>) -> Self {
        let ok = !diagnostics.iter().any(|d| d.severity == Severity::Error);
        Self {
            stage: stage.into(),
            ok,
            diagnostics,
        }
    }
}

pub trait ValidationStage: Send + Sync {
    fn name(&self) -> &'static str;
    fn validate(&self, ctx: &ValidationContext<'_>) -> StageReport;
}
