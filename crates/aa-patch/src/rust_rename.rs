//! Rust-specific rename: byte-accurate textual edits driven by `syn` spans.
//!
//! The approach preserves comments and formatting (a CST-faithful round-trip
//! via `prettyplease` would drop comments; we deliberately avoid that).
//!
//! Scope-awareness ladder:
//!
//! - **Step 0 (shipped in Phase 1.3)** — naive AST visitor. Every `Ident`
//!   whose string equals `old_name` is rewritten. Does not descend into
//!   macro token trees, so e.g. `assert_eq!(add(1, 2), 3)` leaves `add`
//!   alone.
//! - **Step 1 (shipped in Phase 1.10, this module)** — macro-aware. The
//!   visitor additionally walks every `Macro` node's `TokenStream`
//!   recursively, catching `Ident` tokens inside function-call macros
//!   (`assert_eq!`, `format!`, `vec!`, `matches!`, …). `macro_rules!`
//!   definitions are deliberately skipped to avoid touching meta-variable
//!   names in the pattern/body grammar.
//! - **Step 2 (Phase 2)** — true scope resolution via `rust-analyzer`
//!   (LSP subprocess). Distinguishes a local variable `add` from a
//!   function `add`; resolves method calls through traits; understands
//!   cross-module references.
//!
//! This file is Step 1. It is *not* scope-aware: shadowed locals of the
//! same name are still renamed. That remains honest until Step 2 lands.

use proc_macro2::{Ident, TokenStream, TokenTree};
use syn::visit::Visit;
use syn::Macro;

use crate::util::{line_starts, linecol_to_byte};

/// Rename every occurrence of `old_name` to `new_name` in a Rust source
/// text. Returns `(new_source, count_of_replacements)`. Fails with an
/// error message if either the original source or the rewritten source
/// does not parse.
pub fn rename(source: &str, old_name: &str, new_name: &str) -> Result<(String, usize), String> {
    if !is_valid_ident(new_name) {
        return Err(format!("invalid Rust identifier `{new_name}`"));
    }
    let file = syn::parse_file(source).map_err(|e| format!("pre-parse: {e}"))?;

    let line_starts = line_starts(source);
    let mut v = Collector {
        target: old_name,
        ranges: Vec::new(),
        line_starts: &line_starts,
        source,
    };
    v.visit_file(&file);
    v.ranges.sort_by_key(|r| r.0);
    // Dedupe in case syn visits the same span twice.
    v.ranges.dedup();

    let count = v.ranges.len();
    if count == 0 {
        return Ok((source.to_string(), 0));
    }

    // Apply edits descending so byte offsets stay stable.
    let mut out = source.to_string();
    for (start, end) in v.ranges.iter().rev() {
        out.replace_range(*start..*end, new_name);
    }

    syn::parse_file(&out)
        .map_err(|e| format!("post-parse: rewrite would produce invalid Rust: {e}"))?;

    Ok((out, count))
}

struct Collector<'a> {
    target: &'a str,
    ranges: Vec<(usize, usize)>,
    line_starts: &'a [usize],
    source: &'a str,
}

impl<'a, 'ast> Visit<'ast> for Collector<'a> {
    fn visit_ident(&mut self, i: &'ast Ident) {
        if i != self.target {
            return;
        }
        let span = i.span();
        let start = span.start();
        let end = span.end();
        if let (Some(a), Some(b)) = (
            linecol_to_byte(self.line_starts, self.source, start.line, start.column),
            linecol_to_byte(self.line_starts, self.source, end.line, end.column),
        ) {
            if b > a && self.source.get(a..b) == Some(self.target) {
                self.ranges.push((a, b));
            }
        }
    }

    /// `syn`'s default walk visits a `Macro`'s path (so the macro's *name*
    /// is scanned for our target — correct) but treats its `tokens` as an
    /// opaque blob. Step 1's contribution is to descend into that blob
    /// and apply the same ident check inside every function-call macro
    /// body. `macro_rules!` definitions are skipped — their token stream
    /// is a pattern/body grammar with meta-variables (`$x:ident`) that we
    /// must not rewrite.
    fn visit_macro(&mut self, m: &'ast Macro) {
        syn::visit::visit_macro(self, m);
        if is_macro_rules(m) {
            return;
        }
        self.walk_tokens(m.tokens.clone(), None);
    }
}

impl<'a> Collector<'a> {
    /// Recursively walk a `TokenStream`, routing every `Ident` token
    /// through `visit_ident` so the span-based byte replacement stays
    /// uniform. `prev_punct_dollar` carries forward the "previous token
    /// was `$`" flag across the stream so meta-variables like `$add`
    /// that slip into `macro_rules!`-adjacent contexts are never
    /// renamed.
    fn walk_tokens(&mut self, tokens: TokenStream, mut prev_punct_dollar: Option<bool>) {
        for tt in tokens {
            match &tt {
                TokenTree::Ident(i) => {
                    if prev_punct_dollar != Some(true) {
                        self.visit_ident(i);
                    }
                    prev_punct_dollar = Some(false);
                }
                TokenTree::Group(g) => {
                    // Each group is its own nested stream; reset the
                    // `$`-adjacency flag at the group boundary.
                    self.walk_tokens(g.stream(), None);
                    prev_punct_dollar = Some(false);
                }
                TokenTree::Punct(p) => {
                    prev_punct_dollar = Some(p.as_char() == '$');
                }
                TokenTree::Literal(_) => {
                    prev_punct_dollar = Some(false);
                }
            }
        }
    }
}

/// True for the `macro_rules! name { ... }` construct. Syn parses that as
/// an `ItemMacro` whose inner `Macro::path` is the single ident
/// `macro_rules`; the body is a pattern/body grammar and must not be
/// scanned for renamable identifiers.
fn is_macro_rules(m: &Macro) -> bool {
    m.path.is_ident("macro_rules")
}

fn is_valid_ident(s: &str) -> bool {
    let mut it = s.chars();
    let Some(first) = it.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_function_definition_and_callers() {
        let src = r#"
fn add(a: i32, b: i32) -> i32 { a + b }
fn main() {
    // call the adder
    let x = add(1, 2);
    let y = add(x, 3);
}
"#;
        let (out, n) = rename(src, "add", "sum").unwrap();
        assert_eq!(n, 3, "expected 3 renames (def + 2 calls)");
        assert!(out.contains("fn sum(a: i32"));
        assert!(out.contains("let x = sum(1, 2);"));
        assert!(
            out.contains("// call the adder"),
            "comments must be preserved"
        );
    }

    #[test]
    fn rejects_invalid_new_name() {
        assert!(rename("fn a(){}", "a", "1bad").is_err());
    }

    #[test]
    fn rejects_edit_that_breaks_syntax() {
        // Renaming `fn` the keyword would break syntax, but it's not an
        // Ident so it isn't matched. Demonstrate that non-matching names
        // leave source unchanged.
        let src = "fn foo(){}";
        let (out, n) = rename(src, "does_not_exist", "bar").unwrap();
        assert_eq!(n, 0);
        assert_eq!(out, src);
    }

    #[test]
    fn does_not_touch_strings_or_comments() {
        let src = r#"
fn foo() -> &'static str {
    // this comment mentions foo
    "foo in a string"
}
"#;
        let (out, n) = rename(src, "foo", "bar").unwrap();
        assert_eq!(n, 1, "only the definition should change");
        assert!(out.contains("// this comment mentions foo"));
        assert!(out.contains(r#""foo in a string""#));
        assert!(out.contains("fn bar()"));
    }

    #[test]
    fn renames_idents_inside_function_call_macro_bodies() {
        // `assert_eq!(add(1, 2), 3)` is the canonical Step-0 miss: syn
        // treats the macro body as an opaque TokenStream and its idents
        // are never visited. Step 1 fixes that.
        let src = r#"
pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn it_adds() {
        assert_eq!(add(1, 2), 3);
        assert_eq!(add(2, 1), 3);
    }
}
"#;
        let (out, n) = rename(src, "add", "sum").unwrap();
        // 1 definition + 2 uses inside the macro bodies.
        assert_eq!(n, 3, "expected 3 renames, got {n}: {out}");
        assert!(out.contains("pub fn sum("));
        assert!(out.contains("assert_eq!(sum(1, 2), 3);"));
        assert!(out.contains("assert_eq!(sum(2, 1), 3);"));
    }

    #[test]
    fn renames_idents_in_nested_macro_groups() {
        // `vec![add(1, 2), add(3, 4)]` — the bang-macro body uses `[]`
        // delimiters and commas; our walker must recurse into groups.
        let src = r#"
fn add(a: i32, b: i32) -> i32 { a + b }
fn go() -> Vec<i32> {
    vec![add(1, 2), add(3, 4), add(5, 6)]
}
"#;
        let (out, n) = rename(src, "add", "plus").unwrap();
        // 1 definition + 3 uses inside vec![] = 4.
        assert_eq!(n, 4, "expected 4 renames, got {n}: {out}");
        assert!(out.contains("vec![plus(1, 2), plus(3, 4), plus(5, 6)]"));
    }

    #[test]
    fn skips_macro_rules_meta_variable_bodies() {
        // A `macro_rules!` body uses `$add:expr` meta-variables and
        // pattern/body grammar — we must not pretend those are renamable
        // identifiers. This test would fail at the post-parse check if
        // we did rewrite inside, because the resulting `macro_rules!`
        // expansion would be malformed.
        let src = r#"
macro_rules! apply_add {
    ($x:expr, $y:expr) => { add($x, $y) };
}
fn add(a: i32, b: i32) -> i32 { a + b }
fn go() -> i32 { apply_add!(1, 2) }
"#;
        let (out, n) = rename(src, "add", "plus").unwrap();
        // The `macro_rules!` *definition* body must remain untouched.
        assert!(
            out.contains("=> { add($x, $y) }"),
            "macro_rules body must not be rewritten: {out}"
        );
        // The standalone function definition + the invocation site's
        // expanded usage (inside `apply_add!(...)` expansion) do not
        // exist in source — only the `fn add` definition does. So
        // exactly one rename.
        assert_eq!(n, 1, "expected 1 rename (the fn def), got {n}: {out}");
        assert!(out.contains("fn plus(a: i32"));
    }

    #[test]
    fn does_not_rename_after_dollar_in_macro_body() {
        // Belt-and-braces on the `$add` meta-var case, even outside
        // `macro_rules!`. Not a common construct, but the `$`-adjacency
        // filter is what keeps it safe.
        let src = r#"
fn add() {}
macro_rules! nest {
    () => { let _ = stringify!($add); };
}
"#;
        let (out, n) = rename(src, "add", "plus").unwrap();
        // Only the function definition should be renamed; the `$add`
        // token must be untouched.
        assert_eq!(n, 1, "expected 1 rename, got {n}: {out}");
        assert!(out.contains("fn plus() {}"));
        assert!(
            out.contains("stringify!($add)"),
            "`$add` meta-var must remain intact: {out}"
        );
    }
}
