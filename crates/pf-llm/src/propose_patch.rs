//! Patch-proposer mode — the LLM emits typed `PatchPlan`s, not facts.
//!
//! Where [`propose`](crate::propose) asks the model for hypothesis facts
//! that eventually land at `FactLayer::Candidate`, this mode closes the
//! loop all the way to an actionable artifact: a `PatchPlan` the caller
//! can pass directly to `patch.preview` / `patch.apply` / `explain.patch`
//! without any adapter layer. That is the point at which the LLM starts
//! speaking the op vocabulary instead of the fact vocabulary.
//!
//! Grounding is identical in spirit to the fact proposer's identifier
//! resolver: every op's referenced entity must exist in the graph, and
//! the op itself must parse as a known `PatchOp` variant. Unknown ops
//! and hallucinated identifiers are rejected with a structured reason
//! that carries downstream into `explain.patch`.

use std::collections::HashSet;

use pf_graph::GraphStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cache::ResponseCache;
use crate::context::ContextSelector;
use crate::prompt::PromptBuilder;
use crate::provider::{LlmError, LlmProvider, LlmRequest};

pub struct ProposePatchRequest<'a> {
    pub intent: &'a str,
    pub anchor_id: &'a str,
    pub hops: usize,
    pub max_facts: usize,
    pub max_tokens: u32,
    pub temperature: f32,
}

/// A single proposed plan with its justification and acceptance verdict.
///
/// The plan is kept as `serde_json::Value` at this layer so the contract
/// stays narrow: `pf-llm` validates op shape and identifier grounding
/// but is not responsible for building a preview. The caller (typically
/// `pf-core`) re-decodes the plan into its typed form when feeding it
/// to `patch.preview`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchCandidate {
    pub plan: PlanShape,
    pub justification: String,
    pub accepted: bool,
    pub rejection_reason: Option<String>,
}

/// Minimal shape of a plan mirrored from `pf-protocol::PatchPlanDto`,
/// duplicated here to keep `pf-llm` free of a circular dep on the
/// protocol crate's higher-level types. Kept as `Value` ops for the same
/// forward-compatibility reason as the wire type: new op variants do not
/// break older LLM-orchestrator builds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanShape {
    pub ops: Vec<Value>,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProposePatchResult {
    pub accepted: usize,
    pub rejected: usize,
    pub cache_hit: bool,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub candidates: Vec<PatchCandidate>,
}

pub fn propose_patch(
    provider: &dyn LlmProvider,
    cache: &ResponseCache,
    graph: &GraphStore,
    req: ProposePatchRequest<'_>,
) -> Result<ProposePatchResult, LlmError> {
    // 1. Context — same trusted-layer view the fact proposer sees.
    let facts = ContextSelector::new(graph, req.max_facts).k_hop(req.anchor_id, req.hops);

    // 2. Prompt + request.
    let (system, user) = PromptBuilder::propose_patch_v1().build(req.intent, &facts);
    let lreq = LlmRequest {
        system,
        user,
        schema_id: "patch_propose.v1".into(),
        max_tokens: req.max_tokens,
        temperature: req.temperature,
    };

    // 3. Cache-keyed provider call.
    let (raw, cache_hit) = match cache.get(provider.name(), &lreq) {
        Some(hit) => (hit, true),
        None => {
            let r = provider.complete(&lreq)?;
            cache.put(provider.name(), &lreq, r.clone());
            (r, false)
        }
    };

    // 4. Parse.
    let parsed: PatchPropose = serde_json::from_str(&raw.text)
        .map_err(|e| LlmError::InvalidResponse(format!("patch_propose schema mismatch: {e}")))?;

    // 5. Op-shape validation + identifier grounding. Each candidate is
    // accepted or rejected independently so one hallucination does not
    // throw out the rest of the batch.
    let known_function_names = known_function_names(graph);
    let mut result = ProposePatchResult {
        cache_hit,
        tokens_in: raw.tokens_in,
        tokens_out: raw.tokens_out,
        ..Default::default()
    };
    for c in parsed.candidates {
        let verdict = validate_plan(&c.plan, &known_function_names);
        let (accepted, rejection_reason) = match verdict {
            Ok(()) => (true, None),
            Err(reason) => (false, Some(reason)),
        };
        if accepted {
            result.accepted += 1;
        } else {
            result.rejected += 1;
        }
        result.candidates.push(PatchCandidate {
            plan: c.plan,
            justification: c.justification,
            accepted,
            rejection_reason,
        });
    }
    Ok(result)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchPropose {
    candidates: Vec<PatchProposeCandidate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchProposeCandidate {
    plan: PlanShape,
    justification: String,
}

/// Collect every function name (args[1] of `function/2`) for grounding.
/// Grounding on names — not ids — is deliberate: rename ops cite names,
/// which is the shape that matches the `rename_function` op vocabulary.
fn known_function_names(graph: &GraphStore) -> HashSet<String> {
    let mut set = HashSet::new();
    for f in graph.facts_of("function") {
        if let Some(name) = f.args.get(1) {
            set.insert(name.clone());
        }
    }
    set
}

/// Validate a single plan: each op must be a known variant, and each
/// op's grounded identifiers must exist in the graph. Returns `Ok(())`
/// on accept, `Err(reason)` on reject. The reason string is the wire
/// representation of the rejection so the caller (`explain.patch` in
/// particular) can display it.
fn validate_plan(plan: &PlanShape, known_function_names: &HashSet<String>) -> Result<(), String> {
    if plan.ops.is_empty() {
        return Err("plan has no ops".into());
    }
    for (idx, raw) in plan.ops.iter().enumerate() {
        let op_tag = raw
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("op[{idx}] missing `op` tag"))?;
        match op_tag {
            "rename_function" => {
                let old_name = raw
                    .get("old_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!("op[{idx}] rename_function missing `old_name` string")
                    })?;
                let new_name = raw
                    .get("new_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!("op[{idx}] rename_function missing `new_name` string")
                    })?;
                if !known_function_names.contains(old_name) {
                    return Err(format!(
                        "op[{idx}] rename_function: unknown identifier `{old_name}` (hallucination)"
                    ));
                }
                if new_name.is_empty() {
                    return Err(format!(
                        "op[{idx}] rename_function: `new_name` must be a non-empty identifier"
                    ));
                }
            }
            other => {
                return Err(format!(
                    "op[{idx}] unknown op tag `{other}` (known: rename_function)"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use pf_graph::Fact;
    use pf_protocol::FactLayer;

    fn seed(g: &mut GraphStore) {
        g.insert(Fact {
            predicate: "function".into(),
            args: vec!["id_add".into(), "add".into()],
            layer: FactLayer::Observed,
        })
        .unwrap();
        g.insert(Fact {
            predicate: "function".into(),
            args: vec!["id_mul".into(), "mul".into()],
            layer: FactLayer::Observed,
        })
        .unwrap();
    }

    #[test]
    fn accepts_a_well_grounded_plan_rejects_a_hallucinated_one() {
        let mut g = GraphStore::new();
        seed(&mut g);
        let cache = ResponseCache::new();
        let provider = MockProvider;
        let r = propose_patch(
            &provider,
            &cache,
            &g,
            ProposePatchRequest {
                intent: "propose any rename",
                anchor_id: "id_add",
                hops: 1,
                max_facts: 100,
                max_tokens: 1024,
                temperature: 0.0,
            },
        )
        .unwrap();
        // Mock emits one plan per function in the context (here just `add`
        // because `id_mul` is out of k_hop range) plus one hallucination.
        assert!(
            r.accepted >= 1,
            "expected at least one grounded plan: {r:?}"
        );
        assert!(r.rejected >= 1, "expected the hallucination: {r:?}");
        let rejected = r
            .candidates
            .iter()
            .find(|c| !c.accepted)
            .expect("a rejected candidate");
        assert!(
            rejected
                .rejection_reason
                .as_deref()
                .unwrap_or("")
                .contains("hallucination"),
            "rejection reason should flag hallucination: {rejected:?}"
        );
    }

    #[test]
    fn cache_hit_on_second_identical_call() {
        let mut g = GraphStore::new();
        seed(&mut g);
        let cache = ResponseCache::new();
        let provider = MockProvider;
        let mk = || ProposePatchRequest {
            intent: "propose any rename",
            anchor_id: "id_add",
            hops: 1,
            max_facts: 100,
            max_tokens: 1024,
            temperature: 0.0,
        };
        let r1 = propose_patch(&provider, &cache, &g, mk()).unwrap();
        assert!(!r1.cache_hit);
        let r2 = propose_patch(&provider, &cache, &g, mk()).unwrap();
        assert!(r2.cache_hit);
    }

    #[test]
    fn unknown_op_tag_is_rejected() {
        let mut g = GraphStore::new();
        seed(&mut g);
        let plan = PlanShape {
            ops: vec![serde_json::json!({ "op": "delete_universe" })],
            label: "bogus".into(),
        };
        let known: HashSet<String> = ["add".into()].into_iter().collect();
        let err = validate_plan(&plan, &known).unwrap_err();
        assert!(err.contains("unknown op tag"), "{err}");
    }
}
