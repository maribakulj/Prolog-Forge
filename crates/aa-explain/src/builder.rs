//! Compose evidence from the four AYE-AYE subsystems into a single
//! [`Explanation`].
//!
//! The builder is intentionally pure: all sources of evidence are passed in
//! explicitly, so the caller (typically `aa-core`) retains ownership of the
//! filesystem and the LLM lifecycle. This keeps `aa-explain` trivial to
//! unit-test and lets the same code explain both a pre-apply preview and a
//! post-apply commit.

use std::collections::BTreeSet;

use aa_graph::{Fact, GraphStore};
use aa_protocol::FactLayer;
use aa_rules::{trace_derivations, Rule};

use crate::model::{
    CandidateRef, EvidenceNode, Explanation, ExplanationStats, PremiseFact, StageDiagnostic,
    Verdict,
};

/// Minimal, protocol-agnostic view of one validation stage outcome so the
/// builder does not need to depend on `aa-validate`.
#[derive(Debug, Clone)]
pub struct ValidationStageRef {
    pub name: String,
    pub ok: bool,
    pub diagnostics: Vec<StageDiagnostic>,
}

pub struct ExplainInput<'a> {
    pub plan_label: &'a str,
    /// Identifiers touched by the plan. Typically the `old_name` and
    /// `new_name` of every rename op, plus any entity id cited.
    pub anchors: &'a [String],
    pub graph: &'a GraphStore,
    pub rules: &'a [Rule],
    /// Candidate outcomes to cite. Usually carried over from `llm.propose`
    /// or `llm.refine` so the explanation can show rejections in context.
    pub candidate_outcomes: &'a [CandidateRef],
    pub validation_stages: &'a [ValidationStageRef],
    /// Present when the plan actually applied (from `patch.apply`).
    pub commit_id: Option<String>,
    /// `Some(reason)` when `patch.apply` rejected the plan. Takes priority
    /// over `validation_stages` for the synthesized [`Verdict`].
    pub rejection_reason: Option<String>,
}

/// Build the explanation.
pub fn build(input: ExplainInput<'_>) -> Explanation {
    let anchor_set: BTreeSet<&str> = input.anchors.iter().map(|s| s.as_str()).collect();

    let mut evidence: Vec<EvidenceNode> = Vec::new();
    let mut stats = ExplanationStats {
        anchors: input.anchors.len(),
        ..Default::default()
    };

    // 1. Observed + inferred facts that mention an anchor.
    for fact in input.graph.all_facts() {
        if !touches_anchor(fact, &anchor_set) {
            continue;
        }
        match fact.layer {
            FactLayer::Observed => {
                evidence.push(EvidenceNode::Observed {
                    predicate: fact.predicate.clone(),
                    args: fact.args.clone(),
                    role: role_for(fact, &anchor_set),
                });
                stats.observed_cited += 1;
            }
            FactLayer::Inferred => {
                evidence.push(EvidenceNode::Inferred {
                    predicate: fact.predicate.clone(),
                    args: fact.args.clone(),
                });
                stats.inferred_cited += 1;
            }
            // Candidates come via `input.candidate_outcomes`, not the graph
            // scan (we want justifications and rejection reasons too).
            FactLayer::Candidate => {}
            FactLayer::Validated | FactLayer::Constraint => {
                // Report them alongside observed for completeness. These
                // layers are not used yet (Phase 3+) but the match is
                // exhaustive so the compiler nags if new layers appear.
                evidence.push(EvidenceNode::Observed {
                    predicate: fact.predicate.clone(),
                    args: fact.args.clone(),
                    role: format!("{:?}", fact.layer).to_lowercase(),
                });
                stats.observed_cited += 1;
            }
        }
    }

    // 2. Candidates (with justifications and rejection reasons intact).
    for c in input.candidate_outcomes {
        if !c.args.iter().any(|a| anchor_set.contains(a.as_str())) {
            continue;
        }
        evidence.push(EvidenceNode::Candidate(c.clone()));
        stats.candidates_considered += 1;
    }

    // 3. Rule activations. Trace against the *current* graph and keep
    // activations whose head or any premise touches an anchor.
    if !input.rules.is_empty() {
        let derivations = trace_derivations(input.rules, input.graph);
        for d in derivations {
            if !touches_anchor(&d.head, &anchor_set)
                && !d.premises.iter().any(|p| touches_anchor(p, &anchor_set))
            {
                continue;
            }
            evidence.push(EvidenceNode::RuleActivation {
                rule_index: d.rule_index,
                head: to_premise(&d.head),
                premises: d.premises.iter().map(to_premise).collect(),
            });
            stats.rule_activations += 1;
        }
    }

    // 4. Validation stages — one node per stage, verbatim.
    for s in input.validation_stages {
        evidence.push(EvidenceNode::Stage {
            name: s.name.clone(),
            ok: s.ok,
            diagnostics: s.diagnostics.clone(),
        });
        stats.stages_run += 1;
    }

    // 5. Verdict.
    let verdict = synth_verdict(&input, &stats);
    let summary = summarize(&verdict, &stats, input.plan_label);

    Explanation {
        plan_label: input.plan_label.to_string(),
        anchors: input.anchors.to_vec(),
        verdict,
        evidence,
        stats,
        summary,
    }
}

fn touches_anchor(f: &Fact, anchors: &BTreeSet<&str>) -> bool {
    f.args.iter().any(|a| anchors.contains(a.as_str()))
}

fn role_for(f: &Fact, anchors: &BTreeSet<&str>) -> String {
    // First-arg hits are usually the entity itself; later-arg hits are a
    // reference to the anchor (call target, member, etc.).
    match f.args.iter().position(|a| anchors.contains(a.as_str())) {
        Some(0) => "anchor".into(),
        Some(_) => "neighbor".into(),
        None => "unknown".into(),
    }
}

fn to_premise(f: &Fact) -> PremiseFact {
    PremiseFact {
        predicate: f.predicate.clone(),
        args: f.args.clone(),
        layer: f.layer,
    }
}

fn synth_verdict(input: &ExplainInput<'_>, _stats: &ExplanationStats) -> Verdict {
    if let Some(reason) = &input.rejection_reason {
        let failing: Vec<String> = input
            .validation_stages
            .iter()
            .filter(|s| !s.ok)
            .map(|s| s.name.clone())
            .collect();
        return Verdict::Rejected {
            reason: reason.clone(),
            failing_stages: failing,
        };
    }
    let any_stage_failed = input.validation_stages.iter().any(|s| !s.ok);
    if any_stage_failed {
        let failing: Vec<String> = input
            .validation_stages
            .iter()
            .filter(|s| !s.ok)
            .map(|s| s.name.clone())
            .collect();
        return Verdict::Rejected {
            reason: "validation failed".into(),
            failing_stages: failing,
        };
    }
    // All stages green. If the only stage available is the syntactic one
    // we cannot claim to have *proven* the patch safe — signal that
    // honestly.
    let has_semantic_stage = input
        .validation_stages
        .iter()
        .any(|s| s.name != "syntactic");
    if !has_semantic_stage {
        return Verdict::NotProven {
            reason: "syntactic validation only; no rule / type / behavioral evidence".into(),
        };
    }
    let notes = if input.commit_id.is_some() {
        vec!["commit recorded to journal".into()]
    } else {
        vec!["plan is a preview; no commit recorded".into()]
    };
    Verdict::Accepted {
        commit_id: input.commit_id.clone(),
        notes,
    }
}

fn summarize(verdict: &Verdict, stats: &ExplanationStats, label: &str) -> String {
    let head = match verdict {
        Verdict::Accepted { commit_id, .. } => match commit_id {
            Some(id) => format!("accepted ({})", id),
            None => "accepted (preview)".into(),
        },
        Verdict::Rejected { failing_stages, .. } => {
            if failing_stages.is_empty() {
                "rejected".into()
            } else {
                format!("rejected by [{}]", failing_stages.join(", "))
            }
        }
        Verdict::NotProven { .. } => "not proven".into(),
    };
    format!(
        "{label}: {head} — {} observed · {} inferred · {} rule(s) · {} candidate(s) · {} stage(s)",
        stats.observed_cited,
        stats.inferred_cited,
        stats.rule_activations,
        stats.candidates_considered,
        stats.stages_run,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_graph::Fact;
    use aa_rules::parse;

    fn fact(pred: &str, args: &[&str], layer: FactLayer) -> Fact {
        Fact {
            predicate: pred.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            layer,
        }
    }

    #[test]
    fn reports_observed_and_candidate_evidence() {
        let mut g = GraphStore::new();
        g.insert(fact("function", &["id_add", "add"], FactLayer::Observed))
            .unwrap();
        g.insert(fact("function", &["id_foo", "foo"], FactLayer::Observed))
            .unwrap();
        let anchors = vec!["id_add".to_string()];
        let candidates = vec![CandidateRef {
            predicate: "pure".into(),
            args: vec!["id_add".into()],
            justification: "no side effects".into(),
            accepted: true,
            rejection_reason: None,
            round: Some(0),
        }];
        let out = build(ExplainInput {
            plan_label: "rename add -> sum",
            anchors: &anchors,
            graph: &g,
            rules: &[],
            candidate_outcomes: &candidates,
            validation_stages: &[],
            commit_id: None,
            rejection_reason: None,
        });
        assert_eq!(out.anchors, anchors);
        assert!(matches!(out.verdict, Verdict::NotProven { .. }));
        // function(id_add, add) present; function(id_foo, foo) absent.
        let observed_preds: Vec<&str> = out
            .evidence
            .iter()
            .filter_map(|e| match e {
                EvidenceNode::Observed { args, .. } => {
                    args.iter()
                        .find_map(|a| if a == "id_add" { Some("found") } else { None })
                }
                _ => None,
            })
            .collect();
        assert_eq!(observed_preds, vec!["found"]);
        assert_eq!(out.stats.candidates_considered, 1);
    }

    #[test]
    fn rejected_verdict_when_stage_fails() {
        let g = GraphStore::new();
        let stages = vec![ValidationStageRef {
            name: "rules".into(),
            ok: false,
            diagnostics: vec![StageDiagnostic {
                severity: "error".into(),
                file: None,
                message: "violation(forbidden)".into(),
            }],
        }];
        let out = build(ExplainInput {
            plan_label: "t",
            anchors: &[],
            graph: &g,
            rules: &[],
            candidate_outcomes: &[],
            validation_stages: &stages,
            commit_id: None,
            rejection_reason: Some("validation failed".into()),
        });
        match out.verdict {
            Verdict::Rejected {
                ref failing_stages, ..
            } => assert_eq!(failing_stages, &vec!["rules".to_string()]),
            other => panic!("expected Rejected, got {:?}", other),
        }
    }

    #[test]
    fn rule_activation_captured_when_head_touches_anchor() {
        let src = r#"
            recursive(F) :- function(F, _N).
        "#;
        let program = parse(src).unwrap();
        let mut g = GraphStore::new();
        g.insert(fact("function", &["id_add", "add"], FactLayer::Observed))
            .unwrap();
        // Saturate: not strictly required here, but mirrors real usage.
        let _ = aa_rules::evaluate(&program.rules, &mut g).unwrap();
        let anchors = vec!["id_add".to_string()];
        let out = build(ExplainInput {
            plan_label: "t",
            anchors: &anchors,
            graph: &g,
            rules: &program.rules,
            candidate_outcomes: &[],
            validation_stages: &[],
            commit_id: None,
            rejection_reason: None,
        });
        let activations = out
            .evidence
            .iter()
            .filter(|n| matches!(n, EvidenceNode::RuleActivation { .. }))
            .count();
        assert!(activations >= 1, "expected at least one activation");
    }
}
