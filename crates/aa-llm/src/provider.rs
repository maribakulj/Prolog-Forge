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
            "refine.v1" => mock_refine(&req.user),
            "patch_propose.v1" => mock_propose_patch(&req.user),
            "patch_propose.v2" => mock_propose_patch_v2(&req.user),
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
    let mut out = propose_from_context(user);
    // Always add one hallucinated identifier so the anti-hallucination
    // filter is exercised by the smoke test.
    out.push(serde_json::json!({
        "predicate": "pure",
        "args": ["does_not_exist_in_graph"],
        "justification": "intentional hallucination to exercise the resolver"
    }));
    serde_json::json!({ "candidates": out }).to_string()
}

/// The `refine.v1` mock re-reads the same `function(<id>, <name>)` lines
/// from the context but skips any identifier the prompt already flagged as
/// a "Prior rejection". This models the minimum neuro-symbolic revision
/// loop the real provider is expected to exhibit: the structured feedback
/// shrinks the hypothesis space.
fn mock_refine(user: &str) -> String {
    let banned = extract_rejected_ids(user);
    let proposals = propose_from_context(user)
        .into_iter()
        .filter(|p| {
            p.get("args")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .all(|id| !banned.iter().any(|b| b == id))
                })
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "candidates": proposals }).to_string()
}

/// The `patch_propose.v1` mock produces typed patch plans rather than
/// fact candidates. For every function in the context it emits a
/// `rename_function` op that renames `<name>` to `<name>_renamed` — a
/// deterministic, non-destructive synthetic refactor that exercises the
/// full propose → preview → apply pipeline end-to-end. It also emits one
/// plan with a hallucinated identifier so the symbolic grounding guard
/// is tested (the validator rejects any op whose `old_name` does not
/// resolve against the graph).
fn mock_propose_patch(user: &str) -> String {
    let mut candidates: Vec<serde_json::Value> = Vec::new();
    for (_id, name) in context_functions(user) {
        let label = format!("rename {name} -> {name}_renamed");
        candidates.push(serde_json::json!({
            "plan": {
                "ops": [{
                    "op": "rename_function",
                    "old_name": name,
                    "new_name": format!("{name}_renamed"),
                    "files": []
                }],
                "label": label
            },
            "justification": format!("mark {name} as renamed for review"),
        }));
    }
    // Also exercise the `add_derive_to_struct` op path when the context
    // has any struct_def entries. The derive set is deliberately
    // reasonable-by-default (Debug+Clone) rather than aspirational —
    // the point is to prove the op shape round-trips through the
    // validator, not to suggest a refactor.
    for (_id, type_name) in context_types(user) {
        candidates.push(serde_json::json!({
            "plan": {
                "ops": [{
                    "op": "add_derive_to_struct",
                    "type_name": type_name,
                    "derives": ["Debug", "Clone"],
                    "files": [],
                }],
                "label": format!("add Debug, Clone to {type_name}"),
            },
            "justification": format!(
                "add Debug + Clone to {type_name} to make it easy to log and copy"
            ),
        }));
    }
    // One plan on a hallucinated name — exercises the op-resolution guard.
    candidates.push(serde_json::json!({
        "plan": {
            "ops": [{
                "op": "rename_function",
                "old_name": "not_a_real_function_anywhere",
                "new_name": "ghost",
                "files": []
            }],
            "label": "hallucinated rename (expected to be rejected)"
        },
        "justification": "intentional hallucination to exercise the op-resolution guard",
    }));
    serde_json::json!({ "candidates": candidates }).to_string()
}

/// Memory-aware variant of [`mock_propose_patch`]. Reads the
/// `Prior successes:` block from the rendered prompt to spot the op
/// kinds that have already landed on this repo, and biases its
/// proposals accordingly. The contract is the same: it still emits
/// one rename per function in the context plus one hallucinated name
/// so the grounding guard is exercised; when the memory shows
/// `add_derive_to_struct` has succeeded before, it *additionally*
/// emits an extra grounded `add_derive_to_struct` proposal even when
/// the context is thin on structs — the point is to show the runtime
/// learning from its own history.
fn mock_propose_patch_v2(user: &str) -> String {
    let base_json = mock_propose_patch(user);
    let prior_tags = extract_memory_op_tags(user);
    if !prior_tags.iter().any(|t| t == "add_derive_to_struct") {
        return base_json;
    }
    // Re-decode + tack on a memory-biased extra candidate for any
    // struct_def in the context. If there are none the v1 mock
    // already emits nothing for the struct path; we preserve that
    // silence (no bias = no candidate).
    let mut parsed: serde_json::Value = match serde_json::from_str(&base_json) {
        Ok(v) => v,
        Err(_) => return base_json,
    };
    let mut extra: Vec<serde_json::Value> = Vec::new();
    for (_id, type_name) in context_types(user) {
        extra.push(serde_json::json!({
            "plan": {
                "ops": [{
                    "op": "add_derive_to_struct",
                    "type_name": type_name,
                    "derives": ["PartialEq"],
                    "files": [],
                }],
                "label": format!("[memory-biased] add PartialEq to {type_name}"),
            },
            "justification": format!(
                "history on this repo shows add_derive_to_struct landing before; \
                 extending {type_name} to include PartialEq is a good follow-up"
            ),
        }));
    }
    if extra.is_empty() {
        return base_json;
    }
    if let Some(arr) = parsed.get_mut("candidates").and_then(|v| v.as_array_mut()) {
        arr.extend(extra);
    }
    parsed.to_string()
}

/// Extract the `ops=[tag1,tag2,...]` slices from a rendered
/// `Prior successes:` block. Used by [`mock_propose_patch_v2`] to
/// decide which op kinds to bias toward.
fn extract_memory_op_tags(user: &str) -> Vec<String> {
    let mut in_block = false;
    let mut out: Vec<String> = Vec::new();
    for line in user.lines() {
        let t = line.trim();
        if t.starts_with("Prior successes") {
            in_block = true;
            continue;
        }
        if t.starts_with("Respond with JSON") {
            in_block = false;
        }
        if !in_block {
            continue;
        }
        // `- ops=[rename_function] profile=...` → capture between [ and ].
        let Some(start) = t.find("ops=[") else {
            continue;
        };
        let rest = &t[start + "ops=[".len()..];
        let Some(end) = rest.find(']') else {
            continue;
        };
        for tag in rest[..end].split(',') {
            let tag = tag.trim();
            if !tag.is_empty() {
                out.push(tag.to_string());
            }
        }
    }
    out
}

/// Scan the rendered context for `struct_def(<id>, <name>).` /
/// `enum_def(<id>, <name>).` / `union_def(<id>, <name>).` /
/// `type_def(<id>, <name>).` lines and return their `(id, name)` pairs.
/// Used by the `add_derive_to_struct` mock proposal branch.
fn context_types(user: &str) -> Vec<(String, String)> {
    let mut in_context = false;
    let mut out = Vec::new();
    for line in user.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Context (") {
            in_context = true;
            continue;
        }
        if trimmed.starts_with("Prior rejections")
            || trimmed.starts_with("Prior validator diagnostics")
            || trimmed.starts_with("Respond with JSON")
        {
            in_context = false;
            continue;
        }
        if !in_context {
            continue;
        }
        let trimmed = trimmed.trim_end_matches('.');
        for prefix in ["struct_def(", "enum_def(", "union_def(", "type_def("] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                if let Some(end) = rest.find(')') {
                    let inner = &rest[..end];
                    let parts: Vec<&str> = inner.splitn(2, ',').map(|s| s.trim()).collect();
                    if parts.len() == 2 {
                        out.push((parts[0].to_string(), parts[1].to_string()));
                    }
                }
                break;
            }
        }
    }
    out
}

/// Helper: extract `(id, name)` pairs from the rendered context block.
/// Shared between the plan proposer and the fact proposer family so both
/// see the same base identifier set.
fn context_functions(user: &str) -> Vec<(String, String)> {
    let mut in_context = false;
    let mut out = Vec::new();
    for line in user.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Context (") {
            in_context = true;
            continue;
        }
        if trimmed.starts_with("Prior rejections")
            || trimmed.starts_with("Prior validator diagnostics")
            || trimmed.starts_with("Respond with JSON")
        {
            in_context = false;
            continue;
        }
        if !in_context {
            continue;
        }
        let trimmed = trimmed.trim_end_matches('.');
        if let Some(rest) = trimmed.strip_prefix("function(") {
            if let Some(end) = rest.find(')') {
                let inner = &rest[..end];
                let parts: Vec<&str> = inner.splitn(2, ',').map(|s| s.trim()).collect();
                if parts.len() == 2 {
                    out.push((parts[0].to_string(), parts[1].to_string()));
                }
            }
        }
    }
    out
}

/// Scan the rendered context for `function(<id>, <name>).` lines and emit
/// one `pure(<id>)` candidate per match. Shared between the proposer and
/// the refiner so both see the same base hypothesis set.
fn propose_from_context(user: &str) -> Vec<serde_json::Value> {
    // Only read context lines — not the "Prior rejections" block, whose
    // `pure(id)` entries would otherwise be re-proposed as fresh candidates.
    let mut in_context = false;
    let mut out = Vec::new();
    for line in user.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Context (") {
            in_context = true;
            continue;
        }
        if trimmed.starts_with("Prior rejections")
            || trimmed.starts_with("Prior validator diagnostics")
            || trimmed.starts_with("Respond with JSON")
        {
            in_context = false;
            continue;
        }
        if !in_context {
            continue;
        }
        let trimmed = trimmed.trim_end_matches('.');
        if let Some(rest) = trimmed.strip_prefix("function(") {
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
    out
}

/// Parse the "Prior rejections" block and extract every argument id that
/// appeared inside `pred(<args>)`. Used by the refiner mock to avoid
/// re-proposing known hallucinations.
fn extract_rejected_ids(user: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in user.lines() {
        let t = line.trim();
        if t.starts_with("Prior rejections") {
            in_block = true;
            continue;
        }
        if t.starts_with("Prior validator diagnostics") || t.starts_with("Respond with JSON") {
            in_block = false;
        }
        if !in_block {
            continue;
        }
        // Line format: "- pred(arg1, arg2) — reason"
        let Some(stripped) = t.strip_prefix("- ") else {
            continue;
        };
        let Some(open) = stripped.find('(') else {
            continue;
        };
        let Some(close) = stripped[open..].find(')') else {
            continue;
        };
        let inner = &stripped[open + 1..open + close];
        for id in inner.split(',') {
            out.push(id.trim().to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_proposes_one_per_function_plus_hallucination() {
        let req = LlmRequest {
            system: "s".into(),
            user: "Context (observed facts):\n\
                   function(id_a, a).\n\
                   function(id_b, b).\n\
                   Respond with JSON only, no prose."
                .into(),
            schema_id: "propose.v1".into(),
            max_tokens: 1024,
            temperature: 0.0,
        };
        let r = MockProvider.complete(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.text).unwrap();
        assert_eq!(v["candidates"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn refine_mock_drops_previously_rejected_ids() {
        let req = LlmRequest {
            system: "s".into(),
            user: "Context (observed facts):\n\
                   function(id_a, a).\n\
                   function(id_b, b).\n\
                   Prior rejections (do not repeat these):\n\
                   - pure(id_b) — doctrinal rejection example\n\
                   Prior validator diagnostics (address these):\n(none)\n\
                   Respond with JSON only, no prose."
                .into(),
            schema_id: "refine.v1".into(),
            max_tokens: 1024,
            temperature: 0.0,
        };
        let r = MockProvider.complete(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.text).unwrap();
        let cands = v["candidates"].as_array().unwrap();
        // id_b is banned by prior rejections -> only id_a survives, no
        // hallucination added (refine is strict, not exploratory).
        assert_eq!(cands.len(), 1);
        assert_eq!(
            cands[0]["args"].as_array().unwrap()[0].as_str(),
            Some("id_a")
        );
    }
}
