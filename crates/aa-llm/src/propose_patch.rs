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

use aa_graph::GraphStore;
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
    /// Snapshot of relevant past commits the caller wants the model
    /// to condition on. Empty means "pure" proposal (same prompt shape
    /// as the v1 mode from Phase 1.9). When non-empty the prompt
    /// switches to `patch_propose.v2` — which additionally renders a
    /// `Prior successes:` block — so the cache key naturally
    /// distinguishes the two modes. Callers populate this from
    /// `memory.history` when `include_memory` is set on the wire.
    pub memory_hints: Vec<MemoryHint<'a>>,
}

/// Minimal shape of a past commit the proposer cares about. Kept
/// narrow on purpose: the LLM should learn from *what kind of work
/// landed here*, not from the actual bytes. Full history is still
/// available via `memory.get` if a higher-powered future proposer
/// wants it.
#[derive(Debug, Clone, Copy)]
pub struct MemoryHint<'a> {
    pub label: &'a str,
    pub ops_summary: &'a [String],
    pub validation_profile: Option<&'a str>,
    pub total_replacements: usize,
}

/// A single proposed plan with its justification and acceptance verdict.
///
/// The plan is kept as `serde_json::Value` at this layer so the contract
/// stays narrow: `aa-llm` validates op shape and identifier grounding
/// but is not responsible for building a preview. The caller (typically
/// `aa-core`) re-decodes the plan into its typed form when feeding it
/// to `patch.preview`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchCandidate {
    pub plan: PlanShape,
    pub justification: String,
    pub accepted: bool,
    pub rejection_reason: Option<String>,
}

/// Minimal shape of a plan mirrored from `aa-protocol::PatchPlanDto`,
/// duplicated here to keep `aa-llm` free of a circular dep on the
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

    // 2. Prompt + request. When the caller passes memory hints we
    // switch to `patch_propose.v2` — identical schema shape, but the
    // rendered user-prompt carries a `Prior successes:` block. The
    // distinct `schema_id` also lets the response cache separate
    // memory-aware runs from pure ones (otherwise a first no-memory
    // call would poison the cache for later memory-aware calls).
    let (system, user, schema_id) = if req.memory_hints.is_empty() {
        let (s, u) = PromptBuilder::propose_patch_v1().build(req.intent, &facts);
        (s, u, "patch_propose.v1".to_string())
    } else {
        let (s, u) = PromptBuilder::propose_patch_v2().build_with_memory(
            req.intent,
            &facts,
            &req.memory_hints,
        );
        (s, u, "patch_propose.v2".to_string())
    };
    let lreq = LlmRequest {
        system,
        user,
        schema_id,
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
    let known_type_names = known_type_names(graph);
    let mut result = ProposePatchResult {
        cache_hit,
        tokens_in: raw.tokens_in,
        tokens_out: raw.tokens_out,
        ..Default::default()
    };
    for c in parsed.candidates {
        let verdict = validate_plan(&c.plan, &known_function_names, &known_type_names);
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

/// Collect every type name the graph knows about — structs, enums,
/// unions, and plain `type` aliases. Used to ground
/// `add_derive_to_struct` ops. Kept separate from
/// [`known_function_names`] so a rename op can't accidentally target a
/// struct (and vice versa).
fn known_type_names(graph: &GraphStore) -> HashSet<String> {
    let mut set = HashSet::new();
    for pred in ["struct_def", "enum_def", "union_def", "type_def"] {
        for f in graph.facts_of(pred) {
            if let Some(name) = f.args.get(1) {
                set.insert(name.clone());
            }
        }
    }
    set
}

/// Validate a single plan: each op must be a known variant, and each
/// op's grounded identifiers must exist in the graph. Returns `Ok(())`
/// on accept, `Err(reason)` on reject. The reason string is the wire
/// representation of the rejection so the caller (`explain.patch` in
/// particular) can display it.
fn validate_plan(
    plan: &PlanShape,
    known_function_names: &HashSet<String>,
    known_type_names: &HashSet<String>,
) -> Result<(), String> {
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
            "rename_function_typed" => {
                // Step 2 of the type-aware rename ladder. The op
                // resolves by declaration-site position, so the LLM
                // must supply `decl_file`, `decl_line`,
                // `decl_character`, and the new identifier. `old_name`
                // is informative only but, when present, must resolve.
                for field in ["decl_file", "new_name"] {
                    raw.get(field).and_then(|v| v.as_str()).ok_or_else(|| {
                        format!("op[{idx}] rename_function_typed missing `{field}` string")
                    })?;
                }
                for field in ["decl_line", "decl_character"] {
                    raw.get(field).and_then(|v| v.as_u64()).ok_or_else(|| {
                        format!("op[{idx}] rename_function_typed missing `{field}` unsigned int")
                    })?;
                }
                let new_name = raw.get("new_name").and_then(|v| v.as_str()).unwrap_or("");
                if new_name.is_empty() {
                    return Err(format!(
                        "op[{idx}] rename_function_typed: `new_name` must be non-empty"
                    ));
                }
                if let Some(old) = raw.get("old_name").and_then(|v| v.as_str()) {
                    if !old.is_empty() && !known_function_names.contains(old) {
                        return Err(format!(
                            "op[{idx}] rename_function_typed: unknown identifier `{old}` (hallucination)"
                        ));
                    }
                }
            }
            "add_derive_to_struct" => {
                // Off-rename ops: prove the algebra is extensible.
                // Grounding: the `type_name` must exist as a
                // `struct_def/_/_`, `type_def/_/_`, or similar kind in
                // the graph. `derives` must be a non-empty string list.
                let type_name = raw
                    .get("type_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!("op[{idx}] add_derive_to_struct missing `type_name` string")
                    })?;
                let derives = raw
                    .get("derives")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        format!("op[{idx}] add_derive_to_struct missing `derives` array")
                    })?;
                if derives.is_empty() {
                    return Err(format!(
                        "op[{idx}] add_derive_to_struct: `derives` must be non-empty"
                    ));
                }
                for (j, d) in derives.iter().enumerate() {
                    let s = d.as_str().ok_or_else(|| {
                        format!("op[{idx}] add_derive_to_struct: derives[{j}] is not a string")
                    })?;
                    if s.is_empty() {
                        return Err(format!(
                            "op[{idx}] add_derive_to_struct: derives[{j}] is empty"
                        ));
                    }
                }
                if !known_type_names.contains(type_name) {
                    return Err(format!(
                        "op[{idx}] add_derive_to_struct: unknown type `{type_name}` (hallucination)"
                    ));
                }
            }
            "remove_derive_from_struct" => {
                // Dual of `add_derive_to_struct`: same params, same
                // grounding, same hallucination check. Keeping the
                // validator logic symmetric makes it obvious the
                // op-pair is meant as inverses.
                let type_name = raw
                    .get("type_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!("op[{idx}] remove_derive_from_struct missing `type_name` string")
                    })?;
                let derives = raw
                    .get("derives")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        format!("op[{idx}] remove_derive_from_struct missing `derives` array")
                    })?;
                if derives.is_empty() {
                    return Err(format!(
                        "op[{idx}] remove_derive_from_struct: `derives` must be non-empty"
                    ));
                }
                for (j, d) in derives.iter().enumerate() {
                    let s = d.as_str().ok_or_else(|| {
                        format!("op[{idx}] remove_derive_from_struct: derives[{j}] is not a string")
                    })?;
                    if s.is_empty() {
                        return Err(format!(
                            "op[{idx}] remove_derive_from_struct: derives[{j}] is empty"
                        ));
                    }
                }
                if !known_type_names.contains(type_name) {
                    return Err(format!(
                        "op[{idx}] remove_derive_from_struct: unknown type `{type_name}` (hallucination)"
                    ));
                }
            }
            "inline_function" => {
                // First Phase-1.21 op. Grounding: the target function
                // name must resolve to a known `function(_, name)` fact
                // in the graph — same hallucination guard as
                // `rename_function`. Full correctness (free-standing,
                // non-recursive, no `return`, …) is checked by the
                // patch planner; the LLM guard only enforces that the
                // identifier is real.
                let function = raw
                    .get("function")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!("op[{idx}] inline_function missing `function` string")
                    })?;
                if function.is_empty() {
                    return Err(format!(
                        "op[{idx}] inline_function: `function` must be non-empty"
                    ));
                }
                if !known_function_names.contains(function) {
                    return Err(format!(
                        "op[{idx}] inline_function: unknown identifier `{function}` (hallucination)"
                    ));
                }
            }
            "extract_function" => {
                // Phase 1.22. Op-shape only: `source_file` non-empty,
                // a 1-indexed inclusive line range with start <= end,
                // a non-empty `new_name`, and (optionally) a `params`
                // array of `{name, type}` objects. The full narrow
                // contract — selection covers exactly a contiguous
                // run of stmts, no control-flow leak, no macro body,
                // free-standing enclosing fn — is enforced by the
                // planner at preview time. The LLM guard's job here
                // is only to refuse plans that obviously can't ground:
                // empty file, empty name, inverted range.
                let source_file =
                    raw.get("source_file")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            format!("op[{idx}] extract_function missing `source_file` string")
                        })?;
                if source_file.is_empty() {
                    return Err(format!(
                        "op[{idx}] extract_function: `source_file` must be non-empty"
                    ));
                }
                let start_line =
                    raw.get("start_line")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| {
                            format!("op[{idx}] extract_function missing `start_line` unsigned int")
                        })?;
                let end_line = raw
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| {
                        format!("op[{idx}] extract_function missing `end_line` unsigned int")
                    })?;
                if start_line == 0 || end_line == 0 {
                    return Err(format!(
                        "op[{idx}] extract_function: line numbers are 1-indexed (got start={start_line} end={end_line})"
                    ));
                }
                if end_line < start_line {
                    return Err(format!(
                        "op[{idx}] extract_function: end_line {end_line} < start_line {start_line}"
                    ));
                }
                let new_name = raw
                    .get("new_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!("op[{idx}] extract_function missing `new_name` string")
                    })?;
                if new_name.is_empty() {
                    return Err(format!(
                        "op[{idx}] extract_function: `new_name` must be non-empty"
                    ));
                }
                if let Some(params) = raw.get("params") {
                    let arr = params.as_array().ok_or_else(|| {
                        format!("op[{idx}] extract_function: `params` must be an array")
                    })?;
                    for (j, p) in arr.iter().enumerate() {
                        let obj = p.as_object().ok_or_else(|| {
                            format!("op[{idx}] extract_function: params[{j}] is not an object")
                        })?;
                        let name = obj.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                            format!("op[{idx}] extract_function: params[{j}] missing `name` string")
                        })?;
                        let ty = obj.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
                            format!("op[{idx}] extract_function: params[{j}] missing `type` string")
                        })?;
                        if name.is_empty() || ty.is_empty() {
                            return Err(format!(
                                "op[{idx}] extract_function: params[{j}] name/type must be non-empty"
                            ));
                        }
                    }
                }
            }
            other => {
                return Err(format!(
                    "op[{idx}] unknown op tag `{other}` (known: rename_function, \
                     rename_function_typed, add_derive_to_struct, \
                     remove_derive_from_struct, inline_function, extract_function)"
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
    use aa_graph::Fact;
    use aa_protocol::FactLayer;

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
                memory_hints: Vec::new(),
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
            memory_hints: Vec::new(),
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
        let types: HashSet<String> = HashSet::new();
        let err = validate_plan(&plan, &known, &types).unwrap_err();
        assert!(err.contains("unknown op tag"), "{err}");
    }

    #[test]
    fn add_derive_plan_is_accepted_when_grounded() {
        let mut g = GraphStore::new();
        g.insert(aa_graph::Fact {
            predicate: "struct_def".into(),
            args: vec!["id_counter".into(), "Counter".into()],
            layer: aa_protocol::FactLayer::Observed,
        })
        .unwrap();
        let plan = PlanShape {
            ops: vec![serde_json::json!({
                "op": "add_derive_to_struct",
                "type_name": "Counter",
                "derives": ["Debug", "Clone"],
            })],
            label: "add_derive".into(),
        };
        let funcs: HashSet<String> = HashSet::new();
        let types = known_type_names(&g);
        assert!(types.contains("Counter"));
        validate_plan(&plan, &funcs, &types).expect("well-grounded add_derive must validate");
    }

    #[test]
    fn add_derive_plan_rejects_unknown_type() {
        let g = GraphStore::new();
        let plan = PlanShape {
            ops: vec![serde_json::json!({
                "op": "add_derive_to_struct",
                "type_name": "NotARealType",
                "derives": ["Debug"],
            })],
            label: "hallucinated".into(),
        };
        let funcs: HashSet<String> = HashSet::new();
        let types = known_type_names(&g);
        let err = validate_plan(&plan, &funcs, &types).unwrap_err();
        assert!(err.contains("unknown type"), "{err}");
        assert!(err.contains("hallucination"), "{err}");
    }

    /// Phase 1.15: with an empty memory list, `propose_patch` uses
    /// the v1 prompt path. With a non-empty memory that mentions
    /// `add_derive_to_struct` as a prior success, the v2 mock
    /// additionally emits an `add_derive_to_struct` candidate
    /// biased by the history. Proves the memory-aware channel plumbs
    /// end-to-end through the orchestrator, the prompt builder, and
    /// the mock — without any RA or real LLM dependency.
    #[test]
    fn memory_biased_run_emits_extra_grounded_add_derive() {
        let mut g = GraphStore::new();
        seed(&mut g);
        // Seed a struct_def so the memory-biased candidate has a
        // grounding target. Without it the mock's v2 add_derive path
        // is silent (no struct in context → no bias).
        g.insert(aa_graph::Fact {
            predicate: "struct_def".into(),
            args: vec!["id_counter".into(), "Counter".into()],
            layer: aa_protocol::FactLayer::Observed,
        })
        .unwrap();
        let cache = ResponseCache::new();
        let provider = MockProvider;

        // Run #1: empty memory → v1 path. No add_derive candidate
        // with the memory-biased label.
        let r1 = propose_patch(
            &provider,
            &cache,
            &g,
            ProposePatchRequest {
                intent: "propose",
                anchor_id: "id_counter",
                hops: 1,
                max_facts: 100,
                max_tokens: 1024,
                temperature: 0.0,
                memory_hints: Vec::new(),
            },
        )
        .unwrap();
        assert!(!r1
            .candidates
            .iter()
            .any(|c| c.plan.label.contains("[memory-biased]")));

        // Run #2: memory shows `add_derive_to_struct` has landed
        // before → v2 path. Must include the memory-biased candidate
        // on top of the base proposals.
        let ops = vec!["add_derive_to_struct".to_string()];
        let r2 = propose_patch(
            &provider,
            &cache,
            &g,
            ProposePatchRequest {
                intent: "propose",
                anchor_id: "id_counter",
                hops: 1,
                max_facts: 100,
                max_tokens: 1024,
                temperature: 0.0,
                memory_hints: vec![MemoryHint {
                    label: "add derive(Debug, Clone) to Counter",
                    ops_summary: &ops,
                    validation_profile: Some("default"),
                    total_replacements: 1,
                }],
            },
        )
        .unwrap();
        let biased: Vec<&PatchCandidate> = r2
            .candidates
            .iter()
            .filter(|c| c.plan.label.contains("[memory-biased]"))
            .collect();
        assert!(
            !biased.is_empty(),
            "expected a memory-biased candidate, got {:?}",
            r2.candidates
                .iter()
                .map(|c| &c.plan.label)
                .collect::<Vec<_>>()
        );
        // The biased candidate must also be grounded (accepted) — the
        // validator sees `Counter` in the graph.
        assert!(biased.iter().any(|c| c.accepted), "bias must resolve");
    }
}
