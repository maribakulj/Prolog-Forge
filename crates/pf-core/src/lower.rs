//! Lowering from CSM fragments to graph facts.
//!
//! The graph schema is the observable surface that rules are written
//! against. Every predicate emitted here is documented in
//! `docs/rules-dsl.md` under "CSM fact schema".
//!
//! Conventions:
//! - Entity kinds become predicates of arity 2: `kind(id, name)`.
//! - Relation kinds become predicates of arity 2: `kind(subject, object)`.
//! - All facts are written at the `observed` epistemic layer.

use pf_csm::{CsmFragment, EntityKind, RelationKind};
use pf_graph::Fact;
use pf_protocol::FactLayer;

pub fn lower(frag: &CsmFragment) -> Vec<Fact> {
    let mut out = Vec::with_capacity(frag.entities.len() + frag.relations.len());
    for e in &frag.entities {
        out.push(Fact {
            predicate: entity_predicate(e.kind).to_string(),
            args: vec![e.id.0.clone(), e.name.clone()],
            layer: FactLayer::Observed,
        });
    }
    for r in &frag.relations {
        out.push(Fact {
            predicate: relation_predicate(r.kind).to_string(),
            args: vec![r.subject.0.clone(), r.object.0.clone()],
            layer: FactLayer::Observed,
        });
    }
    for aux in &frag.aux_facts {
        out.push(Fact {
            predicate: aux.predicate.clone(),
            args: aux.args.clone(),
            layer: FactLayer::Observed,
        });
    }
    out
}

pub fn entity_predicate(k: EntityKind) -> &'static str {
    match k {
        EntityKind::Module => "module",
        EntityKind::Package => "package",
        EntityKind::File => "file",
        EntityKind::Function => "function",
        EntityKind::Type => "type_def",
        EntityKind::Trait => "trait_def",
        EntityKind::Struct => "struct_def",
        EntityKind::Field => "field",
        EntityKind::Variable => "variable",
        EntityKind::Macro => "macro_def",
    }
}

pub fn relation_predicate(k: RelationKind) -> &'static str {
    match k {
        RelationKind::Defines => "defines",
        RelationKind::Declares => "declares",
        RelationKind::Contains => "contains",
        RelationKind::References => "references",
        RelationKind::Calls => "calls",
        RelationKind::Implements => "implements",
        RelationKind::Extends => "extends",
        RelationKind::Imports => "imports",
        RelationKind::DependsOn => "depends_on",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_csm::{Entity, NodeId, Relation};

    #[test]
    fn lowers_entities_and_relations() {
        let mut frag = CsmFragment::default();
        frag.entities.push(Entity {
            id: NodeId("f".into()),
            kind: EntityKind::Function,
            name: "foo".into(),
            span: None,
        });
        frag.relations.push(Relation {
            kind: RelationKind::Calls,
            subject: NodeId("f".into()),
            object: NodeId("g".into()),
        });
        let facts = lower(&frag);
        assert!(facts
            .iter()
            .any(|f| f.predicate == "function" && f.args == vec!["f", "foo"]));
        assert!(facts
            .iter()
            .any(|f| f.predicate == "calls" && f.args == vec!["f", "g"]));
        assert!(facts.iter().all(|f| f.layer == FactLayer::Observed));
    }
}
