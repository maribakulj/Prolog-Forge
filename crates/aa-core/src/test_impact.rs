//! Impacted-tests selection.
//!
//! When a patch touches a symbol, only the tests that mention that
//! symbol — directly *or through the call chain* — need to run.
//! Running the whole suite on every apply is wasteful on real
//! workspaces and outright prohibitive at CI scale. This module
//! computes a narrow test list from the plan's anchors and a
//! snapshot of the workspace files; `CargoTestStage` then invokes
//! `cargo test <name1> <name2>` instead of the full suite.
//!
//! # Transitive impact (Phase 1.17)
//!
//! A test is impacted if an anchor appears anywhere in its
//! *transitive reachability* set — the union of its own body idents
//! and, recursively, the ident sets of every same-name function it
//! refers to. Concretely: `test_X` mentions helper `h`, `h` mentions
//! anchor `a` → `test_X` is impacted. Phase 1.16 shipped the direct
//! case only; 1.17 does the full closure.
//!
//! Ambiguity: we key functions by simple name (no module path
//! disambiguation), so `fn foo` in module A and `fn foo` in module B
//! are conflated. The failure mode of that conflation is "run more
//! than necessary", which matches the deliberate conservative bias
//! of this computation — we never skip an impacted test.
//!
//! When the computed set is empty the pipeline falls back to running
//! every test — "nothing to run" is almost always a sign the impact
//! computation missed something (e.g. no anchors, all renames on
//! private helpers) rather than a genuine no-op.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::Visit;
use syn::{Item, ItemFn};

/// Every `#[test]`-annotated function whose *transitive* body mentions
/// any of the given anchor names. The result is a flat list of test
/// function *identifiers* (not full module paths) so it can be passed
/// as a `cargo test` filter directly — cargo uses substring matching
/// by default, which subsumes fully-qualified names like `tests::adds`.
pub fn impacted_test_names(files: &BTreeMap<String, String>, anchors: &[String]) -> Vec<String> {
    if anchors.is_empty() {
        return Vec::new();
    }
    let Catalog {
        fn_idents,
        test_fns,
    } = build_catalog(files);

    let anchor_set: BTreeSet<&str> = anchors.iter().map(|s| s.as_str()).collect();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for test_fn in &test_fns {
        let reachable = reachable_idents(test_fn, &fn_idents);
        if anchor_set.iter().any(|a| reachable.contains(*a)) {
            out.insert(test_fn.clone());
        }
    }
    out.into_iter().collect()
}

/// Per-function ident catalog. `fn_idents[name]` is the set of
/// identifiers mentioned in the body of *some* function called `name`
/// (if several functions share a simple name the sets are merged —
/// see the module-level note on the conservative bias that produces).
struct Catalog {
    fn_idents: BTreeMap<String, BTreeSet<String>>,
    test_fns: Vec<String>,
}

fn build_catalog(files: &BTreeMap<String, String>) -> Catalog {
    let mut fn_idents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut test_fns: Vec<String> = Vec::new();
    for (path, source) in files {
        if !path.ends_with(".rs") {
            continue;
        }
        let Ok(file) = syn::parse_file(source) else {
            continue;
        };
        let mut collector = FnCatalogBuilder {
            fns: &mut fn_idents,
            tests: &mut test_fns,
        };
        collector.visit_file(&file);
    }
    // `test_fns` may end up with duplicates if the same test name
    // appears in two files (rare but legal). `cargo test` substring
    // filter doesn't care, but the pipeline's own selection vec would
    // duplicate the CLI arg — dedup here.
    test_fns.sort();
    test_fns.dedup();
    Catalog {
        fn_idents,
        test_fns,
    }
}

struct FnCatalogBuilder<'a> {
    fns: &'a mut BTreeMap<String, BTreeSet<String>>,
    tests: &'a mut Vec<String>,
}

impl<'a, 'ast> Visit<'ast> for FnCatalogBuilder<'a> {
    fn visit_item(&mut self, item: &'ast Item) {
        if let Item::Fn(item_fn) = item {
            let name = item_fn.sig.ident.to_string();
            let mut idents: BTreeSet<String> = BTreeSet::new();
            collect_idents_in_fn(item_fn, &mut idents);
            // Remove the fn's own name from its own ident set — a
            // recursive function mentions itself, which shouldn't
            // inflate the reachability closure.
            idents.remove(&name);
            merge_idents(self.fns.entry(name.clone()).or_default(), idents);
            if has_test_attr(&item_fn.attrs) {
                self.tests.push(name);
            }
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_impl_item_fn(&mut self, m: &'ast syn::ImplItemFn) {
        let name = m.sig.ident.to_string();
        let mut idents: BTreeSet<String> = BTreeSet::new();
        let mut v = IdentCollector { out: &mut idents };
        v.visit_impl_item_fn(m);
        idents.remove(&name);
        merge_idents(self.fns.entry(name.clone()).or_default(), idents);
        // A `#[test]` method inside an `impl` is unusual but legal;
        // still record it.
        if has_test_attr(&m.attrs) {
            self.tests.push(name);
        }
        syn::visit::visit_impl_item_fn(self, m);
    }
}

fn merge_idents(dst: &mut BTreeSet<String>, src: BTreeSet<String>) {
    for s in src {
        dst.insert(s);
    }
}

/// BFS reachability closure over the call-name graph. Starting from
/// `start`'s own ident set, we expand by walking to every ident that
/// is itself a known function name. Uses a worklist + visited set so
/// a cycle (mutual recursion or otherwise) terminates in O(N).
fn reachable_idents(
    start: &str,
    fn_idents: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(start.to_string());
    while let Some(fn_name) = queue.pop_front() {
        if !visited.insert(fn_name.clone()) {
            continue;
        }
        let Some(idents) = fn_idents.get(&fn_name) else {
            continue;
        };
        for ident in idents {
            out.insert(ident.clone());
            if fn_idents.contains_key(ident) && !visited.contains(ident) {
                queue.push_back(ident.clone());
            }
        }
    }
    out
}

/// Match `#[test]`, `#[tokio::test]`, `#[rstest]`, and any other
/// path-suffix of `test`. Errs on the side of "yes, this is a
/// test" — a false positive here means we run an extra function as
/// part of the impact set, not a correctness issue.
fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        let Some(seg) = a.path().segments.last() else {
            return false;
        };
        seg.ident == "test"
    })
}

fn collect_idents_in_fn(item_fn: &ItemFn, out: &mut BTreeSet<String>) {
    // `syn::visit::Visit` walks the function's AST including inner
    // blocks, but it treats macro invocations as opaque. We layer a
    // token-stream walk on top (mirroring the Phase 1.10 rename
    // visitor) so idents inside `assert_eq!(...)` and friends are
    // captured too.
    let mut ident_visitor = IdentCollector { out };
    ident_visitor.visit_item_fn(item_fn);
}

struct IdentCollector<'a> {
    out: &'a mut BTreeSet<String>,
}

impl<'a, 'ast> Visit<'ast> for IdentCollector<'a> {
    fn visit_ident(&mut self, i: &'ast proc_macro2::Ident) {
        self.out.insert(i.to_string());
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        syn::visit::visit_macro(self, m);
        // macro_rules! bodies carry meta-variable grammar that we
        // must not mine for anchor names — same carve-out as the
        // rename visitor.
        if m.path.is_ident("macro_rules") {
            return;
        }
        walk_tokens(m.tokens.clone(), self.out, false);
    }
}

fn walk_tokens(tokens: TokenStream, out: &mut BTreeSet<String>, mut prev_dollar: bool) {
    for tt in tokens {
        match tt {
            TokenTree::Ident(i) => {
                if !prev_dollar {
                    out.insert(i.to_string());
                }
                prev_dollar = false;
            }
            TokenTree::Group(g) => {
                walk_tokens(g.stream(), out, false);
                prev_dollar = false;
            }
            TokenTree::Punct(p) => {
                prev_dollar = p.as_char() == '$';
            }
            TokenTree::Literal(_) => {
                prev_dollar = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(src: &str) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("src/lib.rs".into(), src.into());
        m
    }

    #[test]
    fn narrows_to_tests_that_mention_the_anchor() {
        let src = r#"
            pub fn add(a: i32, b: i32) -> i32 { a + b }
            pub fn mul(a: i32, b: i32) -> i32 { a * b }

            #[cfg(test)]
            mod tests {
                use super::*;

                #[test]
                fn adds() { assert_eq!(add(1, 2), 3); }

                #[test]
                fn muls() { assert_eq!(mul(2, 3), 6); }
            }
        "#;
        let names = impacted_test_names(&files(src), &["add".to_string()]);
        assert_eq!(names, vec!["adds".to_string()]);
    }

    #[test]
    fn empty_anchors_returns_empty_list() {
        let src = "#[test]\nfn t() {}";
        let names = impacted_test_names(&files(src), &[]);
        assert!(names.is_empty());
    }

    #[test]
    fn catches_anchor_in_macro_body() {
        let src = r#"
            pub fn add(a: i32, b: i32) -> i32 { a + b }
            #[test]
            fn t() { assert_eq!(add(1, 2), 3); }
        "#;
        let names = impacted_test_names(&files(src), &["add".to_string()]);
        assert_eq!(names, vec!["t".to_string()]);
    }

    #[test]
    fn skips_macro_rules_meta_vars() {
        let src = r#"
            macro_rules! expand_add {
                ($add:ident) => { $add() };
            }
            fn unrelated() {}
            #[test]
            fn only_uses_macro() { let _ = stringify!($add); }
        "#;
        let names = impacted_test_names(&files(src), &["add".to_string()]);
        assert!(names.is_empty(), "got {names:?}");
    }

    #[test]
    fn detects_tokio_test_attribute() {
        let src = r#"
            pub fn add() {}
            #[tokio::test]
            async fn async_test() { add(); }
        "#;
        let names = impacted_test_names(&files(src), &["add".to_string()]);
        assert_eq!(names, vec!["async_test".to_string()]);
    }

    #[test]
    fn transitive_impact_catches_indirect_callers() {
        // This is the shape that Phase 1.16's direct-only impact
        // used to miss: `double_uses_add` never writes `add`, but
        // calls `double`, which does. Phase 1.17 must include it.
        let src = r#"
            pub fn add(a: i32, b: i32) -> i32 { a + b }
            pub fn double(x: i32) -> i32 { add(x, x) }

            #[cfg(test)]
            mod tests {
                use super::*;
                #[test]
                fn double_uses_add() { assert_eq!(double(5), 10); }
            }
        "#;
        let names = impacted_test_names(&files(src), &["add".to_string()]);
        assert_eq!(names, vec!["double_uses_add".to_string()]);
    }

    #[test]
    fn transitive_impact_excludes_unrelated_chains() {
        // Impact must stay narrow: a test that goes through an
        // unrelated helper chain must not be selected.
        let src = r#"
            pub fn add(a: i32, b: i32) -> i32 { a + b }
            pub fn shout(s: &str) -> String { s.to_uppercase() }
            pub fn yelling_add(a: i32, b: i32) -> String { shout(&add(a, b).to_string()) }

            #[cfg(test)]
            mod tests {
                use super::*;
                #[test]
                fn test_add_through_yelling() {
                    assert_eq!(yelling_add(1, 2), "3");
                }
                #[test]
                fn test_shout_only() {
                    assert_eq!(shout("hi"), "HI");
                }
            }
        "#;
        let names = impacted_test_names(&files(src), &["add".to_string()]);
        // Only the test that transitively reaches `add` should be selected.
        assert_eq!(names, vec!["test_add_through_yelling".to_string()]);
    }

    #[test]
    fn terminates_on_mutual_recursion() {
        // Two helpers that call each other, one of them mentions the
        // anchor. The BFS `visited` set must prevent infinite looping.
        let src = r#"
            pub fn add(a: i32, b: i32) -> i32 { a + b }
            pub fn a_helper(x: i32) -> i32 { b_helper(x) }
            pub fn b_helper(x: i32) -> i32 { a_helper(add(x, 0)) }

            #[cfg(test)]
            mod tests {
                use super::*;
                #[test]
                fn uses_cycle() { let _ = a_helper(1); }
            }
        "#;
        let names = impacted_test_names(&files(src), &["add".to_string()]);
        assert_eq!(names, vec!["uses_cycle".to_string()]);
    }
}
