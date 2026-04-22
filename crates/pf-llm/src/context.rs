//! Context selection: extract a sub-graph around an anchor and serialize it
//! for consumption by an LLM.
//!
//! Phase 1.2 supports a simple k-hop walk. Budget-aware pruning and embedding
//! retrieval land in later phases.

use pf_graph::{Fact, GraphStore};
use pf_protocol::FactLayer;

#[derive(Debug, Clone)]
pub struct ContextSelector<'a> {
    pub graph: &'a GraphStore,
    /// Maximum number of facts to include in the context window.
    pub max_facts: usize,
}

impl<'a> ContextSelector<'a> {
    pub fn new(graph: &'a GraphStore, max_facts: usize) -> Self {
        Self { graph, max_facts }
    }

    /// Collect every **trusted** fact (observed or inferred) that mentions
    /// `anchor_id` as one of its args, plus those reachable through `hops`
    /// more hops. `candidate`, `validated`, and `constraint` facts are
    /// excluded — the context the LLM sees must not contain untrusted
    /// material produced by earlier LLM calls.
    pub fn k_hop(&self, anchor_id: &str, hops: usize) -> Vec<Fact> {
        let mut frontier: Vec<String> = vec![anchor_id.to_string()];
        let mut seen_ids: std::collections::HashSet<String> = frontier.iter().cloned().collect();
        let mut out: Vec<Fact> = Vec::new();
        let mut out_seen: std::collections::HashSet<Fact> = std::collections::HashSet::new();
        for _ in 0..=hops {
            let mut next: Vec<String> = Vec::new();
            for id in &frontier {
                // Iterate over a deterministically-ordered snapshot. The
                // graph uses a `HashMap` internally, and on a large
                // enough graph the iteration order varies *across calls*
                // in the same process — which silently breaks the
                // response cache (identical params but a different
                // prompt render → cache miss). Sorting keeps every
                // repeated call byte-identical.
                let mut facts: Vec<&Fact> = self.graph.all_facts().collect();
                facts.sort_by(|a, b| {
                    a.predicate
                        .cmp(&b.predicate)
                        .then_with(|| a.args.cmp(&b.args))
                });
                for fact in facts {
                    if !is_trusted(fact.layer) {
                        continue;
                    }
                    if fact.args.iter().any(|a| a == id) && out_seen.insert(fact.clone()) {
                        out.push(fact.clone());
                        if out.len() >= self.max_facts {
                            return out;
                        }
                        for a in &fact.args {
                            if seen_ids.insert(a.clone()) {
                                next.push(a.clone());
                            }
                        }
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        out
    }

    /// Collect every trusted fact, truncated to `max_facts` — use with
    /// care; intended for small fixtures and smoke tests.
    pub fn everything(&self) -> Vec<Fact> {
        self.graph
            .all_facts()
            .filter(|f| is_trusted(f.layer))
            .take(self.max_facts)
            .cloned()
            .collect()
    }
}

fn is_trusted(l: FactLayer) -> bool {
    matches!(l, FactLayer::Observed | FactLayer::Inferred)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_protocol::FactLayer;

    fn f(pred: &str, args: &[&str]) -> Fact {
        Fact {
            predicate: pred.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            layer: FactLayer::Observed,
        }
    }

    #[test]
    fn one_hop_collects_neighbors() {
        let mut g = GraphStore::new();
        g.insert(f("function", &["a", "foo"])).unwrap();
        g.insert(f("function", &["b", "bar"])).unwrap();
        g.insert(f("calls", &["a", "b"])).unwrap();
        let s = ContextSelector::new(&g, 100);
        let facts = s.k_hop("a", 1);
        let preds: Vec<&str> = facts.iter().map(|x| x.predicate.as_str()).collect();
        assert!(preds.contains(&"function"));
        assert!(preds.contains(&"calls"));
        assert!(facts.iter().any(|x| x.args.contains(&"b".to_string())));
    }
}
