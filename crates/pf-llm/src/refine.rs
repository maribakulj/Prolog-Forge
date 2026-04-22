//! Refinement mode — the iterative neuro-symbolic loop.
//!
//! `propose` is one-shot: a single prompt → a single set of candidates →
//! accept/reject. `refine` closes the loop. Each round:
//!
//! 1. assemble a context from the graph (same trusted layers as `propose`),
//! 2. render a `refine.v1` prompt that carries forward *all* prior
//!    rejection reasons and validator diagnostics from earlier rounds,
//! 3. call the provider (cache-keyed per round so identical prompts are
//!    free),
//! 4. parse + identifier-resolve the response, insert survivors at
//!    `FactLayer::Candidate`,
//! 5. if no rejections occurred in this round, break — the loop has
//!    converged. Otherwise carry the new rejections into the next round.
//!
//! The result carries a per-round summary so callers can show progress and
//! budget. `max_rounds` bounds the loop; convergence or exhaustion
//! (whichever comes first) terminates it.

use std::collections::HashSet;

use pf_graph::{Fact, GraphStore};
use pf_protocol::FactLayer;
use serde::{Deserialize, Serialize};

use crate::cache::ResponseCache;
use crate::context::ContextSelector;
use crate::prompt::{DiagnosticLine, PromptBuilder, RejectionLine};
use crate::propose::ProposalOutcome;
use crate::provider::{LlmError, LlmProvider, LlmRequest};

/// One diagnostic passed back to the refiner from an earlier validator run
/// (typically from a rejected `patch.apply`). Keeps the LLM crate
/// dependency-free of `pf-validate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinerDiagnostic {
    pub severity: String,
    pub file: Option<String>,
    pub message: String,
}

pub struct RefineRequest<'a> {
    pub intent: &'a str,
    pub anchor_id: &'a str,
    pub hops: usize,
    pub max_facts: usize,
    pub max_rounds: u32,
    pub max_tokens: u32,
    pub temperature: f32,
    pub prior_outcomes: Vec<ProposalOutcome>,
    pub prior_diagnostics: Vec<RefinerDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoundSummary {
    pub round: u32,
    pub accepted: usize,
    pub rejected: usize,
    pub cache_hit: bool,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RefineResult {
    pub rounds: u32,
    pub converged: bool,
    pub final_accepted: usize,
    pub final_rejected: usize,
    pub tokens_in_total: u32,
    pub tokens_out_total: u32,
    pub outcomes: Vec<ProposalOutcome>,
    pub rounds_summary: Vec<RoundSummary>,
}

pub fn refine(
    provider: &dyn LlmProvider,
    cache: &ResponseCache,
    graph: &mut GraphStore,
    req: RefineRequest<'_>,
) -> Result<RefineResult, LlmError> {
    let facts = ContextSelector::new(graph, req.max_facts).k_hop(req.anchor_id, req.hops);

    // Accumulated rejections we carry from round to round. Start with the
    // caller-supplied prior outcomes (the rejections-only subset is what
    // matters to the refiner).
    let mut carried: Vec<ProposalOutcome> = req
        .prior_outcomes
        .iter()
        .filter(|o| !o.accepted)
        .cloned()
        .collect();

    let mut result = RefineResult::default();
    let rounds_cap = req.max_rounds.max(1);
    let mut round: u32 = 0;
    while round < rounds_cap {
        round += 1;

        // Prompt assembly — the rendered rejection + diagnostic blocks are
        // deterministic in `carried` and `req.prior_diagnostics`, so the
        // cache key is stable per round.
        let rej_lines: Vec<RejectionLine<'_>> = carried
            .iter()
            .map(|o| RejectionLine {
                predicate: &o.predicate,
                args: &o.args,
                reason: o.rejection_reason.as_deref().unwrap_or(""),
            })
            .collect();
        let diag_lines: Vec<DiagnosticLine<'_>> = req
            .prior_diagnostics
            .iter()
            .map(|d| DiagnosticLine {
                severity: &d.severity,
                file: d.file.as_deref(),
                message: &d.message,
            })
            .collect();
        let (system, user) =
            PromptBuilder::refine_v1().build_refine(req.intent, &facts, &rej_lines, &diag_lines);
        let lreq = LlmRequest {
            system,
            user,
            schema_id: "refine.v1".into(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
        };

        let (raw, cache_hit) = match cache.get(provider.name(), &lreq) {
            Some(hit) => (hit, true),
            None => {
                let r = provider.complete(&lreq)?;
                cache.put(provider.name(), &lreq, r.clone());
                (r, false)
            }
        };

        let parsed: RefineResponse = serde_json::from_str(&raw.text)
            .map_err(|e| LlmError::InvalidResponse(format!("refine schema mismatch: {e}")))?;

        let known_entity_ids = known_entity_ids(graph);
        let mut round_summary = RoundSummary {
            round,
            cache_hit,
            tokens_in: raw.tokens_in,
            tokens_out: raw.tokens_out,
            ..Default::default()
        };
        let mut new_rejections: Vec<ProposalOutcome> = Vec::new();
        for c in parsed.candidates {
            let outcome = match resolve(&c, &known_entity_ids) {
                Ok(()) => {
                    let fact = Fact {
                        predicate: c.predicate.clone(),
                        args: c.args.clone(),
                        layer: FactLayer::Candidate,
                    };
                    match graph.insert(fact) {
                        Ok(_) => ProposalOutcome {
                            predicate: c.predicate.clone(),
                            args: c.args.clone(),
                            justification: c.justification.clone(),
                            accepted: true,
                            rejection_reason: None,
                        },
                        Err(e) => ProposalOutcome {
                            predicate: c.predicate.clone(),
                            args: c.args.clone(),
                            justification: c.justification.clone(),
                            accepted: false,
                            rejection_reason: Some(e.to_string()),
                        },
                    }
                }
                Err(reason) => ProposalOutcome {
                    predicate: c.predicate.clone(),
                    args: c.args.clone(),
                    justification: c.justification.clone(),
                    accepted: false,
                    rejection_reason: Some(reason),
                },
            };
            if outcome.accepted {
                round_summary.accepted += 1;
                result.final_accepted += 1;
            } else {
                round_summary.rejected += 1;
                result.final_rejected += 1;
                new_rejections.push(outcome.clone());
            }
            result.outcomes.push(outcome);
        }

        result.tokens_in_total = result.tokens_in_total.saturating_add(raw.tokens_in);
        result.tokens_out_total = result.tokens_out_total.saturating_add(raw.tokens_out);
        result.rounds_summary.push(round_summary.clone());

        // Convergence: this round produced zero rejections.
        if new_rejections.is_empty() {
            result.converged = true;
            break;
        }
        // Otherwise accumulate for the next round.
        carried.extend(new_rejections);
    }
    result.rounds = round;
    Ok(result)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefineResponse {
    candidates: Vec<RefineCandidate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefineCandidate {
    predicate: String,
    args: Vec<String>,
    justification: String,
}

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

fn resolve(c: &RefineCandidate, known: &HashSet<String>) -> Result<(), String> {
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
    fn refine_converges_after_one_round_when_no_rejections() {
        let mut g = GraphStore::new();
        seed(&mut g);
        let cache = ResponseCache::new();
        let provider = MockProvider;
        // No prior rejections, no diagnostics — the mock refine produces
        // exactly the two resolvable candidates, zero rejections → convergence.
        let r = refine(
            &provider,
            &cache,
            &mut g,
            RefineRequest {
                intent: "refine purity",
                anchor_id: "id_a",
                hops: 1,
                max_facts: 100,
                max_rounds: 3,
                max_tokens: 1024,
                temperature: 0.0,
                prior_outcomes: Vec::new(),
                prior_diagnostics: Vec::new(),
            },
        )
        .unwrap();
        assert!(r.converged);
        assert_eq!(r.rounds, 1);
        assert!(r.final_accepted >= 1);
        assert_eq!(r.final_rejected, 0);
    }

    #[test]
    fn refine_heals_a_previous_hallucination() {
        let mut g = GraphStore::new();
        seed(&mut g);
        let cache = ResponseCache::new();
        let provider = MockProvider;
        // Pretend a previous `propose` had flagged `does_not_exist` as a
        // hallucination. The refiner must not re-propose it.
        let prior = vec![ProposalOutcome {
            predicate: "pure".into(),
            args: vec!["does_not_exist".into()],
            justification: "bogus".into(),
            accepted: false,
            rejection_reason: Some("unknown identifier `does_not_exist` (hallucination)".into()),
        }];
        let r = refine(
            &provider,
            &cache,
            &mut g,
            RefineRequest {
                intent: "refine purity after hallucination",
                anchor_id: "id_a",
                hops: 1,
                max_facts: 100,
                max_rounds: 3,
                max_tokens: 1024,
                temperature: 0.0,
                prior_outcomes: prior,
                prior_diagnostics: Vec::new(),
            },
        )
        .unwrap();
        // Should converge immediately — all revised candidates resolve.
        assert!(r.converged);
        assert_eq!(r.final_rejected, 0);
        assert!(r.final_accepted >= 1);
        // And the survivors must all be real ids from the seed graph.
        for o in &r.outcomes {
            assert!(o.accepted);
            assert!(o.args.iter().all(|a| a == "id_a" || a == "id_b"));
        }
    }
}
