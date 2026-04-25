//! Proposer mode.
//!
//! Pipeline:
//! 1. Select a sub-graph around `anchor_id` and serialize it as context.
//! 2. Build the `propose.v1` prompt (system + user) from the intent + context.
//! 3. Look up the cache; on miss, call the provider.
//! 4. Parse the JSON response into a strictly-typed `Proposals` struct
//!    (unknown fields rejected).
//! 5. Resolve every proposal's identifiers against the graph. Any argument
//!    that does not appear as the first arg of an entity fact
//!    (`function/2`, `struct_def/2`, …) is a hallucination — the proposal
//!    is rejected.
//! 6. Insert accepted proposals into the graph at `FactLayer::Candidate`.

use std::collections::HashSet;

use aa_graph::{Fact, GraphStore};
use aa_protocol::FactLayer;
use serde::{Deserialize, Serialize};

use crate::cache::ResponseCache;
use crate::context::ContextSelector;
use crate::prompt::PromptBuilder;
use crate::provider::{LlmError, LlmProvider, LlmRequest};

/// Input to the `propose` pipeline.
pub struct ProposeRequest<'a> {
    pub intent: &'a str,
    pub anchor_id: &'a str,
    pub hops: usize,
    pub max_facts: usize,
    pub max_tokens: u32,
    pub temperature: f32,
}

/// Outcome of a single proposal after identifier resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalOutcome {
    pub predicate: String,
    pub args: Vec<String>,
    pub justification: String,
    pub accepted: bool,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProposeResult {
    pub accepted: usize,
    pub rejected: usize,
    pub cache_hit: bool,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub outcomes: Vec<ProposalOutcome>,
}

pub fn propose(
    provider: &dyn LlmProvider,
    cache: &ResponseCache,
    graph: &mut GraphStore,
    req: ProposeRequest<'_>,
) -> Result<ProposeResult, LlmError> {
    // 1. Context
    let facts = ContextSelector::new(graph, req.max_facts).k_hop(req.anchor_id, req.hops);

    // 2. Prompt
    let (system, user) = PromptBuilder::propose_v1().build(req.intent, &facts);
    let lreq = LlmRequest {
        system,
        user,
        schema_id: "propose.v1".into(),
        max_tokens: req.max_tokens,
        temperature: req.temperature,
    };

    // 3. Cache
    let (raw, cache_hit) = match cache.get(provider.name(), &lreq) {
        Some(hit) => (hit, true),
        None => {
            let r = provider.complete(&lreq)?;
            cache.put(provider.name(), &lreq, r.clone());
            (r, false)
        }
    };

    // 4. Parse against the strict schema
    let parsed: Proposals = serde_json::from_str(&raw.text)
        .map_err(|e| LlmError::InvalidResponse(format!("schema mismatch: {e}")))?;

    // 5 + 6. Resolve and insert
    let known_entity_ids = known_entity_ids(graph);
    let mut result = ProposeResult {
        accepted: 0,
        rejected: 0,
        cache_hit,
        tokens_in: raw.tokens_in,
        tokens_out: raw.tokens_out,
        outcomes: Vec::with_capacity(parsed.candidates.len()),
    };
    for c in parsed.candidates {
        match resolve(&c, &known_entity_ids) {
            Ok(()) => {
                let fact = Fact {
                    predicate: c.predicate.clone(),
                    args: c.args.clone(),
                    layer: FactLayer::Candidate,
                };
                match graph.insert(fact) {
                    Ok(_) => {
                        result.accepted += 1;
                        result.outcomes.push(ProposalOutcome {
                            predicate: c.predicate,
                            args: c.args,
                            justification: c.justification,
                            accepted: true,
                            rejection_reason: None,
                        });
                    }
                    Err(e) => {
                        result.rejected += 1;
                        result.outcomes.push(ProposalOutcome {
                            predicate: c.predicate,
                            args: c.args,
                            justification: c.justification,
                            accepted: false,
                            rejection_reason: Some(e.to_string()),
                        });
                    }
                }
            }
            Err(reason) => {
                result.rejected += 1;
                result.outcomes.push(ProposalOutcome {
                    predicate: c.predicate,
                    args: c.args,
                    justification: c.justification,
                    accepted: false,
                    rejection_reason: Some(reason),
                });
            }
        }
    }
    Ok(result)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposals {
    candidates: Vec<Proposal>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    predicate: String,
    args: Vec<String>,
    justification: String,
}

/// The set of ids that appear as `args[0]` of any entity-kind fact. Any
/// identifier in a proposal must belong to this set or the proposal is a
/// hallucination.
fn known_entity_ids(graph: &GraphStore) -> HashSet<String> {
    const ENTITY_PREDICATES: &[&str] = &[
        "module",
        "package",
        "file",
        "function",
        "type_def",
        "trait_def",
        "struct_def",
        "field",
        "variable",
        "macro_def",
    ];
    let mut set = HashSet::new();
    for p in ENTITY_PREDICATES {
        for f in graph.facts_of(p) {
            if let Some(id) = f.args.first() {
                set.insert(id.clone());
            }
        }
    }
    set
}

fn resolve(c: &Proposal, known: &HashSet<String>) -> Result<(), String> {
    if c.args.is_empty() {
        return Err("empty args".into());
    }
    if c.predicate.is_empty() || !c.predicate.chars().next().unwrap().is_ascii_lowercase() {
        return Err("predicate must be a lowercase identifier".into());
    }
    for a in &c.args {
        if !known.contains(a) {
            return Err(format!("unknown identifier `{a}` (hallucination)"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;

    fn seed(g: &mut GraphStore) {
        g.insert(Fact {
            predicate: "function".into(),
            args: vec!["id_a".into(), "a".into()],
            layer: FactLayer::Observed,
        })
        .unwrap();
        g.insert(Fact {
            predicate: "function".into(),
            args: vec!["id_b".into(), "b".into()],
            layer: FactLayer::Observed,
        })
        .unwrap();
    }

    #[test]
    fn hallucination_rejected_real_ids_accepted() {
        let mut g = GraphStore::new();
        seed(&mut g);
        let cache = ResponseCache::new();
        let provider = MockProvider;

        let req = ProposeRequest {
            intent: "propose purity",
            anchor_id: "id_a",
            hops: 1,
            max_facts: 100,
            max_tokens: 1024,
            temperature: 0.0,
        };
        let r = propose(&provider, &cache, &mut g, req).unwrap();

        assert!(r.accepted >= 1, "expected at least one accepted proposal");
        assert!(r.rejected >= 1, "expected the hallucination to be rejected");

        // candidate facts must be stored at the candidate layer, never elsewhere
        assert!(g.count_layer(FactLayer::Candidate) >= 1);
        assert_eq!(g.count_layer(FactLayer::Inferred), 0);

        let rejected_reasons: Vec<_> = r
            .outcomes
            .iter()
            .filter(|o| !o.accepted)
            .filter_map(|o| o.rejection_reason.clone())
            .collect();
        assert!(rejected_reasons.iter().any(|s| s.contains("hallucination")));
    }

    #[test]
    fn second_call_hits_cache() {
        let mut g = GraphStore::new();
        seed(&mut g);
        let cache = ResponseCache::new();
        let provider = MockProvider;

        let mk = || ProposeRequest {
            intent: "propose purity",
            anchor_id: "id_a",
            hops: 1,
            max_facts: 100,
            max_tokens: 1024,
            temperature: 0.0,
        };

        let r1 = propose(&provider, &cache, &mut g, mk()).unwrap();
        assert!(!r1.cache_hit);
        let r2 = propose(&provider, &cache, &mut g, mk()).unwrap();
        assert!(r2.cache_hit);
    }
}
