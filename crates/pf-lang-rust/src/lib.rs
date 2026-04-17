//! Rust language analyzer.
//!
//! Uses `syn` to parse a single `.rs` file and lowers its top-level items and
//! call expressions into a `CsmFragment`. This is the **syntactic** pass —
//! no type resolution, no cross-module linkage. That lives in a later phase
//! (Phase 2: type-aware analyzer backed by `rust-analyzer`).
//!
//! The facts produced here are enough to drive a useful set of rules
//! already: call graphs (syntactic), recursion detection, module containment,
//! trait implementations, orphan types, etc.

use pf_csm::{
    AnalyzerError, AuxFact, CsmFragment, Entity, EntityKind, LanguageAnalyzer, NodeId, Relation,
    RelationKind, SourceSpan,
};
use syn::visit::Visit;

pub struct RustAnalyzer;

impl RustAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RustAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for RustAnalyzer {
    fn language(&self) -> &'static str {
        "rust"
    }

    fn analyze(&self, source: &str, path: &str) -> Result<CsmFragment, AnalyzerError> {
        let file = syn::parse_file(source).map_err(|e| AnalyzerError {
            message: format!("syn parse error: {e}"),
            span: None,
        })?;
        let file_id = NodeId(format!("{path}#file"));
        let mut frag = CsmFragment::default();
        frag.entities.push(Entity {
            id: file_id.clone(),
            kind: EntityKind::File,
            name: path.to_string(),
            span: None,
        });
        let mut v = Visitor {
            path: path.to_string(),
            parent: file_id,
            frag: &mut frag,
        };
        v.visit_file(&file);
        Ok(frag)
    }
}

struct Visitor<'a> {
    path: String,
    parent: NodeId,
    frag: &'a mut CsmFragment,
}

impl<'a> Visitor<'a> {
    fn child_id(&self, kind: &str, name: &str) -> NodeId {
        NodeId(format!("{}#{}:{}@{}", self.path, kind, name, self.parent.0))
    }

    fn push_entity(
        &mut self,
        id: NodeId,
        kind: EntityKind,
        name: String,
        span: Option<SourceSpan>,
    ) {
        self.frag.entities.push(Entity {
            id,
            kind,
            name,
            span,
        });
    }

    fn push_rel(&mut self, kind: RelationKind, subject: NodeId, object: NodeId) {
        self.frag.relations.push(Relation {
            kind,
            subject,
            object,
        });
    }

    /// Build a synthetic `#ref:Name` NodeId and emit an accompanying
    /// `ref_name(ref_id, name)` aux fact so rules can resolve references
    /// back to entities without needing string manipulation.
    fn make_ref(&mut self, name: &str) -> NodeId {
        let id = NodeId(format!("{}#ref:{}", self.path, name));
        self.frag.aux_facts.push(AuxFact {
            predicate: "ref_name".into(),
            args: vec![id.0.clone(), name.to_string()],
        });
        id
    }

    fn descend<F: FnOnce(&mut Visitor<'_>)>(&mut self, new_parent: NodeId, f: F) {
        let saved = std::mem::replace(&mut self.parent, new_parent);
        f(self);
        self.parent = saved;
    }
}

impl<'ast, 'a> Visit<'ast> for Visitor<'a> {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let name = node.sig.ident.to_string();
        let id = self.child_id("fn", &name);
        self.push_entity(id.clone(), EntityKind::Function, name, None);
        self.push_rel(RelationKind::Contains, self.parent.clone(), id.clone());
        self.push_rel(RelationKind::Defines, self.parent.clone(), id.clone());
        self.descend(id, |v| syn::visit::visit_item_fn(v, node));
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        let name = node.ident.to_string();
        let id = self.child_id("struct", &name);
        self.push_entity(id.clone(), EntityKind::Struct, name, None);
        self.push_rel(RelationKind::Contains, self.parent.clone(), id.clone());
        self.push_rel(RelationKind::Defines, self.parent.clone(), id.clone());
        self.descend(id, |v| syn::visit::visit_item_struct(v, node));
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        let name = node.ident.to_string();
        let id = self.child_id("enum", &name);
        self.push_entity(id.clone(), EntityKind::Type, name, None);
        self.push_rel(RelationKind::Contains, self.parent.clone(), id.clone());
        self.push_rel(RelationKind::Defines, self.parent.clone(), id.clone());
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        let name = node.ident.to_string();
        let id = self.child_id("trait", &name);
        self.push_entity(id.clone(), EntityKind::Trait, name, None);
        self.push_rel(RelationKind::Contains, self.parent.clone(), id.clone());
        self.push_rel(RelationKind::Defines, self.parent.clone(), id.clone());
        self.descend(id, |v| syn::visit::visit_item_trait(v, node));
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let ty_name = type_last_segment(&node.self_ty).unwrap_or_else(|| "<unknown>".into());
        if let Some((_, trait_path, _)) = &node.trait_ {
            let trait_name = last_segment_of_path(trait_path).unwrap_or_else(|| "<unknown>".into());
            let subj = self.make_ref(&ty_name);
            let obj = self.make_ref(&trait_name);
            self.push_rel(RelationKind::Implements, subj, obj);
        }
        let ty_id = self.make_ref(&ty_name);
        self.descend(ty_id, |v| syn::visit::visit_item_impl(v, node));
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let name = node.sig.ident.to_string();
        let id = self.child_id("method", &name);
        self.push_entity(id.clone(), EntityKind::Function, name, None);
        self.push_rel(RelationKind::Contains, self.parent.clone(), id.clone());
        self.descend(id, |v| syn::visit::visit_impl_item_fn(v, node));
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let name = node.ident.to_string();
        let id = self.child_id("mod", &name);
        self.push_entity(id.clone(), EntityKind::Module, name, None);
        self.push_rel(RelationKind::Contains, self.parent.clone(), id.clone());
        self.descend(id, |v| syn::visit::visit_item_mod(v, node));
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        // Record imported leaf names as Imports relations for the enclosing
        // parent. Best-effort; a full path resolver lands with rust-analyzer.
        let mut names = Vec::new();
        collect_use_leaves(&node.tree, &mut String::new(), &mut names);
        for n in names {
            let obj = self.make_ref(&n);
            self.push_rel(RelationKind::Imports, self.parent.clone(), obj);
        }
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = node.func.as_ref() {
            if let Some(name) = last_segment_of_path(&p.path) {
                let obj = self.make_ref(&name);
                self.push_rel(RelationKind::Calls, self.parent.clone(), obj);
            }
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let name = node.method.to_string();
        let obj = self.make_ref(&name);
        self.push_rel(RelationKind::Calls, self.parent.clone(), obj);
        syn::visit::visit_expr_method_call(self, node);
    }
}

fn type_last_segment(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(tp) => last_segment_of_path(&tp.path),
        _ => None,
    }
}

fn last_segment_of_path(p: &syn::Path) -> Option<String> {
    p.segments.last().map(|s| s.ident.to_string())
}

fn collect_use_leaves(tree: &syn::UseTree, prefix: &mut String, out: &mut Vec<String>) {
    match tree {
        syn::UseTree::Path(p) => {
            let saved = prefix.len();
            if !prefix.is_empty() {
                prefix.push_str("::");
            }
            prefix.push_str(&p.ident.to_string());
            collect_use_leaves(&p.tree, prefix, out);
            prefix.truncate(saved);
        }
        syn::UseTree::Name(n) => out.push(n.ident.to_string()),
        syn::UseTree::Rename(r) => out.push(r.rename.to_string()),
        syn::UseTree::Glob(_) => {}
        syn::UseTree::Group(g) => {
            for t in &g.items {
                collect_use_leaves(t, prefix, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_functions_structs_calls() {
        let src = r#"
            struct Point { x: i32, y: i32 }
            fn add(a: i32, b: i32) -> i32 { a + b }
            fn main() {
                let p = Point { x: 1, y: 2 };
                let z = add(p.x, p.y);
                helper(z);
            }
            fn helper(_n: i32) {}
        "#;
        let a = RustAnalyzer::new();
        let frag = a.analyze(src, "src/demo.rs").unwrap();
        let fn_names: Vec<_> = frag
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .map(|e| e.name.as_str())
            .collect();
        assert!(fn_names.contains(&"main"));
        assert!(fn_names.contains(&"add"));
        assert!(fn_names.contains(&"helper"));
        let structs: Vec<_> = frag
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(structs, vec!["Point"]);
        let call_count = frag
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .count();
        // `add(...)`, `helper(...)` at minimum.
        assert!(call_count >= 2, "expected >=2 calls, got {call_count}");
    }

    #[test]
    fn handles_traits_and_impls() {
        let src = r#"
            trait Greet { fn hello(&self); }
            struct Bot;
            impl Greet for Bot { fn hello(&self) {} }
        "#;
        let frag = RustAnalyzer::new().analyze(src, "x.rs").unwrap();
        let traits: Vec<_> = frag
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Trait)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(traits, vec!["Greet"]);
        let impls = frag
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Implements)
            .count();
        assert_eq!(impls, 1);
    }
}
