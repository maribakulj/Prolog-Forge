//! Bottom-up Datalog evaluator (naive fixpoint, Phase 0).
//!
//! Correct and terminating for any Datalog program (no function symbols, no
//! negation). Semi-naive / incremental evaluation is a Phase 1 upgrade and
//! will slot into the same entry-point without changing the API.

use std::collections::HashMap;

use pf_graph::{Fact, GraphStore, Pattern as GPattern, Term as GTerm};
use pf_protocol::FactLayer;

use crate::ast::{Atom, Rule, Term};

#[derive(Debug, Clone, Default)]
pub struct EvalStats {
    pub derived: usize,
    pub iterations: usize,
}

pub fn evaluate(rules: &[Rule], store: &mut GraphStore) -> Result<EvalStats, EvalError> {
    let mut stats = EvalStats::default();
    loop {
        stats.iterations += 1;
        let mut batch: Vec<Fact> = Vec::new();
        for rule in rules {
            for binding in solve_body(&rule.body, store) {
                let fact = instantiate_head(&rule.head, &binding)?;
                if !store.contains(&fact) && !batch.iter().any(|f| f == &fact) {
                    batch.push(fact);
                }
            }
        }
        if batch.is_empty() {
            break;
        }
        for f in batch {
            if store.insert(f).map_err(EvalError::Graph)? {
                stats.derived += 1;
            }
        }
    }
    Ok(stats)
}

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("unbound variable `{0}` in rule head")]
    UnboundHeadVar(String),
    #[error(transparent)]
    Graph(#[from] pf_graph::GraphError),
}

/// Join every body atom against the store. Returns the list of variable
/// bindings that satisfy the whole body. O(facts^|body|) worst case; fine for
/// Phase 0. Replaced by semi-naive incremental evaluation in Phase 1.
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

fn instantiate_head(head: &Atom, b: &HashMap<String, String>) -> Result<Fact, EvalError> {
    let args = head
        .args
        .iter()
        .map(|t| match t {
            Term::Const(c) => Ok(c.clone()),
            Term::Var(v) => b
                .get(v)
                .cloned()
                .ok_or_else(|| EvalError::UnboundHeadVar(v.clone())),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Fact {
        predicate: head.predicate.clone(),
        args,
        layer: FactLayer::Inferred,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn seed(store: &mut GraphStore, atoms: &[Atom]) {
        for a in atoms {
            let args = a
                .args
                .iter()
                .map(|t| match t {
                    Term::Const(c) => c.clone(),
                    Term::Var(_) => panic!("fact has a variable"),
                })
                .collect();
            store
                .insert(Fact::observed(a.predicate.clone(), args))
                .unwrap();
        }
    }

    #[test]
    fn transitive_closure() {
        let src = r#"
            parent(alice, bob).
            parent(bob, carol).
            parent(carol, dan).
            ancestor(X, Y) :- parent(X, Y).
            ancestor(X, Z) :- parent(X, Y), ancestor(Y, Z).
        "#;
        let program = parse(src).unwrap();
        let mut store = GraphStore::new();
        seed(&mut store, &program.facts);
        let stats = evaluate(&program.rules, &mut store).unwrap();
        // 3 direct + 2 indirect + 1 double-indirect = 6 ancestor facts
        let ancestors: Vec<_> = store.facts_of("ancestor").collect();
        assert_eq!(ancestors.len(), 6);
        assert_eq!(stats.derived, 6);
    }

    #[test]
    fn unbound_head_var_errors() {
        let src = "bad(X) :- p(Y).";
        let program = parse(src).unwrap();
        let mut store = GraphStore::new();
        store.insert(Fact::observed("p", vec!["a".into()])).unwrap();
        let err = evaluate(&program.rules, &mut store).unwrap_err();
        assert!(matches!(err, EvalError::UnboundHeadVar(_)));
    }
}
