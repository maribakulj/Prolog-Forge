//! In-memory knowledge graph store for Phase 0.
//!
//! Facts are n-ary atoms `predicate(a1, a2, ...)` over a single `Atom` type
//! (strings in v0). Each fact is tagged with an **epistemic layer**:
//! Observed / Inferred / Candidate / Validated / Constraint.
//!
//! The store maintains:
//!   - a set of deduplicated facts per predicate,
//!   - a mapping predicate → arity,
//!   - a simple predicate index for fast lookup.
//!
//! A disk-backed store lands in a later phase; the trait shape is already
//! compatible.

use std::collections::HashMap;

use aa_protocol::FactLayer;
use indexmap::IndexSet;
use serde::{Deserialize, Serialize};

pub type Atom = String;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fact {
    pub predicate: String,
    pub args: Vec<Atom>,
    pub layer: FactLayer,
}

impl Fact {
    pub fn observed(pred: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            predicate: pred.into(),
            args,
            layer: FactLayer::Observed,
        }
    }
    pub fn arity(&self) -> usize {
        self.args.len()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("arity mismatch for predicate `{pred}`: expected {expected}, got {got}")]
    ArityMismatch {
        pred: String,
        expected: usize,
        got: usize,
    },
}

#[derive(Debug, Default)]
pub struct GraphStore {
    /// predicate -> set of facts (deduped). IndexSet preserves insertion order
    /// which is useful for determinism in tests and explanations.
    buckets: HashMap<String, IndexSet<Fact>>,
    /// predicate -> arity. First insertion fixes the arity.
    arities: HashMap<String, usize>,
}

impl GraphStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a fact. Returns true if it was newly added.
    pub fn insert(&mut self, fact: Fact) -> Result<bool, GraphError> {
        let ar = fact.arity();
        match self.arities.get(&fact.predicate) {
            Some(&k) if k != ar => {
                return Err(GraphError::ArityMismatch {
                    pred: fact.predicate.clone(),
                    expected: k,
                    got: ar,
                });
            }
            None => {
                self.arities.insert(fact.predicate.clone(), ar);
            }
            _ => {}
        }
        let bucket = self.buckets.entry(fact.predicate.clone()).or_default();
        Ok(bucket.insert(fact))
    }

    pub fn contains(&self, fact: &Fact) -> bool {
        self.buckets
            .get(&fact.predicate)
            .is_some_and(|b| b.contains(fact))
    }

    pub fn facts_of(&self, predicate: &str) -> impl Iterator<Item = &Fact> {
        self.buckets
            .get(predicate)
            .into_iter()
            .flat_map(|b| b.iter())
    }

    pub fn all_facts(&self) -> impl Iterator<Item = &Fact> {
        self.buckets.values().flat_map(|b| b.iter())
    }

    pub fn total(&self) -> usize {
        self.buckets.values().map(|b| b.len()).sum()
    }

    pub fn count_layer(&self, layer: FactLayer) -> usize {
        self.all_facts().filter(|f| f.layer == layer).count()
    }

    pub fn arity(&self, predicate: &str) -> Option<usize> {
        self.arities.get(predicate).copied()
    }

    pub fn predicates(&self) -> impl Iterator<Item = &str> {
        self.buckets.keys().map(|s| s.as_str())
    }
}

/// One term in a query pattern: either a concrete atom or a variable binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Atom(Atom),
    Var(String),
}

/// A query pattern: `predicate(t1, t2, ...)`.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub predicate: String,
    pub args: Vec<Term>,
}

impl Pattern {
    /// Match this pattern against every fact of its predicate. Returns one
    /// binding map per successful match.
    pub fn matches<'a>(
        &'a self,
        store: &'a GraphStore,
    ) -> impl Iterator<Item = HashMap<String, Atom>> + 'a {
        store
            .facts_of(&self.predicate)
            .filter_map(move |f| unify(&self.args, &f.args))
    }
}

fn unify(pattern: &[Term], args: &[Atom]) -> Option<HashMap<String, Atom>> {
    if pattern.len() != args.len() {
        return None;
    }
    let mut bind: HashMap<String, Atom> = HashMap::new();
    for (t, a) in pattern.iter().zip(args.iter()) {
        match t {
            Term::Atom(x) if x == a => {}
            Term::Atom(_) => return None,
            Term::Var(name) => match bind.get(name) {
                Some(prev) if prev != a => return None,
                Some(_) => {}
                None => {
                    bind.insert(name.clone(), a.clone());
                }
            },
        }
    }
    Some(bind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_dedupe() {
        let mut g = GraphStore::new();
        assert!(g
            .insert(Fact::observed("p", vec!["a".into(), "b".into()]))
            .unwrap());
        assert!(!g
            .insert(Fact::observed("p", vec!["a".into(), "b".into()]))
            .unwrap());
        assert_eq!(g.total(), 1);
    }

    #[test]
    fn arity_mismatch_rejected() {
        let mut g = GraphStore::new();
        g.insert(Fact::observed("p", vec!["a".into()])).unwrap();
        assert!(g
            .insert(Fact::observed("p", vec!["a".into(), "b".into()]))
            .is_err());
    }

    #[test]
    fn pattern_match() {
        let mut g = GraphStore::new();
        g.insert(Fact::observed("parent", vec!["alice".into(), "bob".into()]))
            .unwrap();
        g.insert(Fact::observed("parent", vec!["bob".into(), "carol".into()]))
            .unwrap();
        let pat = Pattern {
            predicate: "parent".into(),
            args: vec![Term::Var("X".into()), Term::Atom("bob".into())],
        };
        let res: Vec<_> = pat.matches(&g).collect();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].get("X"), Some(&"alice".to_string()));
    }
}
