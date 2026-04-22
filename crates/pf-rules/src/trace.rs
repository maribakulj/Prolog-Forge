//! Provenance-aware single-pass tracer.
//!
//! `pf_rules::evaluate` runs the fixpoint and writes derived facts back into
//! the graph, but it does not expose *which* rule produced each fact or with
//! what premises. The explainer needs that information.
//!
//! `trace_derivations` walks every rule once against the *current* graph
//! state and emits a [`Derivation`] per satisfying body binding. It is not
//! itself a fixpoint: run `evaluate` first to saturate the graph, then call
//! this to attribute every derived fact to a rule + premises. Composed,
//! they produce a one-level proof fragment per inferred fact — enough for a
//! useful explanation today; full multi-step proof trees land with the
//! richer explainer in Phase 2.

use std::collections::HashMap;

use pf_graph::{Fact, GraphStore, Pattern as GPattern, Term as GTerm};
use pf_protocol::FactLayer;

use crate::ast::{Atom, Rule, Term};

/// A single rule activation: a satisfying body binding yields one head fact
/// plus the instantiated body atoms used as premises.
#[derive(Debug, Clone)]
pub struct Derivation {
    /// Index of the rule in the input slice (stable addressing for reports).
    pub rule_index: usize,
    /// Head predicate name, duplicated here for convenience.
    pub head_name: String,
    /// Head fact produced by this activation.
    pub head: Fact,
    /// Body atoms instantiated with the binding — the premises the engine
    /// relied on.
    pub premises: Vec<Fact>,
}

/// Enumerate every rule activation that would fire against the *current*
/// state of `store`. Does not mutate the store.
pub fn trace_derivations(rules: &[Rule], store: &GraphStore) -> Vec<Derivation> {
    let mut out = Vec::new();
    for (idx, rule) in rules.iter().enumerate() {
        for binding in solve_body(&rule.body, store) {
            let head = match instantiate_head(&rule.head, &binding) {
                Ok(h) => h,
                Err(_) => continue,
            };
            let premises = rule
                .body
                .iter()
                .map(|a| instantiate_body_atom(a, &binding))
                .collect();
            out.push(Derivation {
                rule_index: idx,
                head_name: rule.head.predicate.clone(),
                head,
                premises,
            });
        }
    }
    out
}

fn solve_body(body: &[Atom], store: &GraphStore) -> Vec<HashMap<String, String>> {
    let mut bindings: Vec<HashMap<String, String>> = vec![HashMap::new()];
    for atom in body {
        let mut next = Vec::new();
        for b in &bindings {
            let pat = to_pattern_with_binding(atom, b);
            for m in pat.matches(store) {
                if let Some(merged) = merge(b, &m) {
                    next.push(merged);
                }
            }
        }
        bindings = next;
    }
    bindings
}

fn merge(
    base: &HashMap<String, String>,
    add: &HashMap<String, String>,
) -> Option<HashMap<String, String>> {
    let mut out = base.clone();
    for (k, v) in add {
        match out.get(k) {
            Some(prev) if prev != v => return None,
            Some(_) => {}
            None => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Some(out)
}

fn to_pattern_with_binding(atom: &Atom, b: &HashMap<String, String>) -> GPattern {
    let args = atom
        .args
        .iter()
        .map(|t| match t {
            Term::Const(c) => GTerm::Atom(c.clone()),
            Term::Var(v) => match b.get(v) {
                Some(val) => GTerm::Atom(val.clone()),
                None => GTerm::Var(v.clone()),
            },
        })
        .collect();
    GPattern {
        predicate: atom.predicate.clone(),
        args,
    }
}

fn instantiate_head(head: &Atom, b: &HashMap<String, String>) -> Result<Fact, ()> {
    let mut args = Vec::with_capacity(head.args.len());
    for t in &head.args {
        match t {
            Term::Const(c) => args.push(c.clone()),
            Term::Var(v) => match b.get(v) {
                Some(val) => args.push(val.clone()),
                None => return Err(()),
            },
        }
    }
    Ok(Fact {
        predicate: head.predicate.clone(),
        args,
        layer: FactLayer::Inferred,
    })
}

fn instantiate_body_atom(atom: &Atom, b: &HashMap<String, String>) -> Fact {
    let args = atom
        .args
        .iter()
        .map(|t| match t {
            Term::Const(c) => c.clone(),
            Term::Var(v) => b.get(v).cloned().unwrap_or_else(|| format!("?{v}")),
        })
        .collect();
    Fact {
        predicate: atom.predicate.clone(),
        args,
        layer: FactLayer::Observed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn traces_each_direct_activation() {
        let src = r#"
            parent(alice, bob).
            parent(bob, carol).
            ancestor(X, Y) :- parent(X, Y).
            ancestor(X, Z) :- parent(X, Y), ancestor(Y, Z).
        "#;
        let program = parse(src).unwrap();
        let mut store = GraphStore::new();
        for a in &program.facts {
            let args = a
                .args
                .iter()
                .map(|t| match t {
                    Term::Const(c) => c.clone(),
                    Term::Var(_) => unreachable!(),
                })
                .collect();
            store
                .insert(Fact::observed(a.predicate.clone(), args))
                .unwrap();
        }
        // First saturate the graph so rule 2's body atoms exist.
        let _ = crate::evaluate(&program.rules, &mut store).unwrap();
        let derivations = trace_derivations(&program.rules, &store);
        // rule 0 (direct): one activation per parent fact.
        let direct = derivations.iter().filter(|d| d.rule_index == 0).count();
        assert_eq!(direct, 2);
        // rule 1 (transitive): at least one activation (alice -> carol).
        assert!(derivations
            .iter()
            .any(|d| d.rule_index == 1
                && d.head.args == vec!["alice".to_string(), "carol".to_string()]));
        // Every derivation must have the right number of premises.
        for d in &derivations {
            assert_eq!(d.premises.len(), program.rules[d.rule_index].body.len());
        }
    }
}
