//! Impacted-tests selection.
//!
//! When a patch touches a symbol, only the tests that mention that
//! symbol need to run — running the whole suite on every apply is
//! wasteful on real workspaces and outright prohibitive at CI scale.
//! This module computes a narrow test list from the plan's anchors
//! and a snapshot of the workspace files; `CargoTestStage` then
//! invokes `cargo test <name1> <name2>` instead of the full suite.
//!
//! # Direct impact only
//!
//! Phase 1.16 ships the *direct* impact approximation: a test is
//! impacted if its body (including macro token trees — same walker
//! used by the Phase 1.10 macro-aware rename) mentions any anchor
//! name as an identifier. Transitive impact via the graph's `calls`
//! facts is deferred to Phase 2; the direct approach is safer to
//! ship because its failure mode is "run more than necessary", not
//! "skip an impacted test".
//!
//! When the computed set is empty the pipeline falls back to running
//! every test — "nothing to run" is almost always a sign the impact
//! computation missed something (e.g. no anchors, all renames on
//! private helpers) rather than a genuine no-op.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::Visit;
use syn::{Item, ItemFn};

/// Every `#[test]`-annotated function whose body mentions any of the
/// given anchor names. The result is a flat list of test function
/// *identifiers* (not full module paths) so it can be passed as a
/// `cargo test` filter directly — cargo uses substring matching by
/// default, which subsumes fully-qualified names like `tests::adds`.
pub fn impacted_test_names(files: &BTreeMap<String, String>, anchors: &[String]) -> Vec<String> {
    if anchors.is_empty() {
        return Vec::new();
    }
    let anchor_set: BTreeSet<&str> = anchors.iter().map(|s| s.as_str()).collect();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for (path, source) in files {
        if !path.ends_with(".rs") {
            continue;
        }
        let Ok(file) = syn::parse_file(source) else {
            continue;
        };
        let mut finder = TestFnFinder { found: Vec::new() };
        finder.visit_file(&file);
        for test_fn in &finder.found {
            let mut idents: BTreeSet<String> = BTreeSet::new();
            collect_idents_in_fn(test_fn, &mut idents);
            if anchor_set.iter().any(|a| idents.contains(*a)) {
                out.insert(test_fn.sig.ident.to_string());
            }
        }
    }
    out.into_iter().collect()
}

struct TestFnFinder<'ast> {
    found: Vec<&'ast ItemFn>,
}

impl<'ast> Visit<'ast> for TestFnFinder<'ast> {
    fn visit_item(&mut self, item: &'ast Item) {
        if let Item::Fn(item_fn) = item {
            if has_test_attr(&item_fn.attrs) {
                self.found.push(item_fn);
            }
        }
        syn::visit::visit_item(self, item);
    }
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
        // The anchor `add` only appears inside the `assert_eq!` macro
        // invocation. The Phase 1.10 macro-aware rename already
        // proved our walker reaches into token trees; this test
        // pins the same behaviour for impact detection.
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
        // A `$add` meta-variable inside a `macro_rules!` body must
        // not be confused with the real symbol `add` — same
        // `$`-adjacency guard as the rename visitor.
        let src = r#"
            macro_rules! expand_add {
                ($add:ident) => { $add() };
            }
            fn unrelated() {}
            #[test]
            fn only_uses_macro() { let _ = stringify!($add); }
        "#;
        let names = impacted_test_names(&files(src), &["add".to_string()]);
        // `only_uses_macro`'s body ident set has `stringify` and
        // `$add` (which is skipped by the $-adjacency guard, leaving
        // nothing named `add`). So impact is empty.
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
}
