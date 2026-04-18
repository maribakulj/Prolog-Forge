//! Provider trait + mock implementation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub system: String,
    pub user: String,
    /// Name of the JSON shape the response must satisfy. The provider does
    /// not enforce it; the caller validates with serde + a concrete type.
    /// The shape name is part of the cache key.
    pub schema_id: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub text: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub provider: String,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("budget exceeded: requested {requested} > cap {cap}")]
    Budget { requested: u32, cap: u32 },
}

pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError>;
}

/// Deterministic in-process provider used for tests, CI, and offline
/// development. Responds to a small set of known schemas; unknown schemas
/// yield an empty but well-typed response.
///
/// This is *not* a simulation of a real model. Its job is to prove the
/// orchestrator plumbing works end-to-end without requiring a network call.
pub struct MockProvider;

impl Default for MockProvider {
    fn default() -> Self {
        Self
    }
}

impl LlmProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let text = match req.schema_id.as_str() {
            "propose.v1" => mock_propose(&req.user),
            _ => r#"{"candidates":[]}"#.to_string(),
        };
        Ok(LlmResponse {
            tokens_in: estimate_tokens(&req.system) + estimate_tokens(&req.user),
            tokens_out: estimate_tokens(&text),
            text,
            provider: "mock".into(),
        })
    }
}

/// Very rough token estimate: ≈ 4 chars per token. Sufficient for budget
/// accounting in tests; real providers return exact counts.
fn estimate_tokens(s: &str) -> u32 {
    ((s.len() as f32 / 4.0).ceil() as u32).max(1)
}

/// For the `propose.v1` schema, the mock scans the prompt for lines of the
/// form `function(<id>, <name>)` and proposes a `pure` candidate for each.
/// This exercises the identifier-resolution guard downstream without needing
/// an actual LLM.
fn mock_propose(user: &str) -> String {
    let mut out = Vec::new();
    for line in user.lines() {
        let line = line.trim().trim_end_matches('.');
        if let Some(rest) = line.strip_prefix("function(") {
            if let Some(end) = rest.find(')') {
                let inner = &rest[..end];
                let parts: Vec<&str> = inner.splitn(2, ',').map(|s| s.trim()).collect();
                if parts.len() == 2 {
                    out.push(serde_json::json!({
                        "predicate": "pure",
                        "args": [parts[0]],
                        "justification": format!("no side effects observed for {}", parts[1])
                    }));
                }
            }
        }
    }
    // Always add one hallucinated identifier so the anti-hallucination
    // filter is exercised by the smoke test.
    out.push(serde_json::json!({
        "predicate": "pure",
        "args": ["does_not_exist_in_graph"],
        "justification": "intentional hallucination to exercise the resolver"
    }));
    serde_json::json!({ "candidates": out }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_proposes_one_per_function_plus_hallucination() {
        let req = LlmRequest {
            system: "s".into(),
            user: "function(id_a, a)\nfunction(id_b, b)".into(),
            schema_id: "propose.v1".into(),
            max_tokens: 1024,
            temperature: 0.0,
        };
        let r = MockProvider.complete(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.text).unwrap();
        assert_eq!(v["candidates"].as_array().unwrap().len(), 3);
    }
}
