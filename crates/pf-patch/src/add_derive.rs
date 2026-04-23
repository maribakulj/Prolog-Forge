//! `AddDeriveToStruct` — add traits to a type's `#[derive(...)]` attribute.
//!
//! The op is the first non-rename entry in the patch algebra. Its role in
//! the `pf-patch` story is to prove the pipeline tolerates ops of a
//! different shape end-to-end (preview → validate → apply → explain →
//! `llm.propose_patch`): the planner does not care what the op *does*,
//! only that `apply_op` returns a transformed file map.
//!
//! Semantics, in order:
//!
//! 1. Parse the file with `syn`.
//! 2. Walk items for a `struct` / `enum` / `union` whose ident matches
//!    `type_name`.
//! 3. Among that item's outer attributes, find the *first* `#[derive(...)]`.
//!    - If found: append the new derives that are not already listed,
//!      inside the existing parenthesised list, preserving the
//!      surrounding formatting.
//!    - If not: insert `#[derive(Trait1, Trait2, ...)]\n` on a new line
//!      immediately above the item, using the item's leading
//!      indentation.
//! 4. Re-parse the result. Reject if the rewrite would break syntax.
//!
//! Multiple `#[derive]` attributes on the same item are tolerated but we
//! only touch the first; splitting derives across attributes is legal
//! and some crates do it on purpose (e.g. to isolate a cfg-gated
//! derive). Being conservative here keeps the op idempotent without
//! needing to reason about why the original author chose their split.

use proc_macro2::{Delimiter, TokenTree};
use syn::{visit::Visit, Item};

/// The return shape matches `rust_rename::rename`: `(new_source, n)`
/// where `n` is the number of *new* derives actually added (duplicates
/// skipped). Returns `Ok((source, 0))` when the target type is not
/// present in the file — the planner uses that to decide whether to
/// advance to the next file in scope.
pub fn add_derive(
    source: &str,
    type_name: &str,
    derives: &[String],
) -> Result<(String, usize), String> {
    if derives.is_empty() {
        return Ok((source.to_string(), 0));
    }
    for d in derives {
        if !is_valid_derive_path(d) {
            return Err(format!("invalid derive path `{d}`"));
        }
    }
    let file = syn::parse_file(source).map_err(|e| format!("pre-parse: {e}"))?;

    let line_starts = line_starts(source);
    let mut finder = Finder {
        type_name,
        target: None,
        line_starts: &line_starts,
        source,
    };
    finder.visit_file(&file);
    let Some(target) = finder.target else {
        return Ok((source.to_string(), 0));
    };

    let (rewritten, added) = match target {
        Target::ExistingDerive {
            inner_range,
            listed,
        } => {
            let new_list: Vec<&str> = derives
                .iter()
                .filter(|d| !listed.iter().any(|e| same_derive(e, d)))
                .map(|s| s.as_str())
                .collect();
            if new_list.is_empty() {
                return Ok((source.to_string(), 0));
            }
            let (start, end) = inner_range;
            let mut out = source.to_string();
            let existing_text = &source[start..end];
            let trimmed = existing_text.trim_end();
            let trailing_ws = &existing_text[trimmed.len()..];
            let mut rewritten_inner = trimmed.to_string();
            for d in &new_list {
                if rewritten_inner.trim().is_empty() {
                    rewritten_inner.push_str(d);
                } else {
                    rewritten_inner.push_str(", ");
                    rewritten_inner.push_str(d);
                }
            }
            rewritten_inner.push_str(trailing_ws);
            out.replace_range(start..end, &rewritten_inner);
            (out, new_list.len())
        }
        Target::InsertAbove { line_start, indent } => {
            let joined = derives.join(", ");
            let attr_line = format!("{indent}#[derive({joined})]\n");
            let mut out = source.to_string();
            out.insert_str(line_start, &attr_line);
            (out, derives.len())
        }
    };

    syn::parse_file(&rewritten)
        .map_err(|e| format!("post-parse: rewrite would produce invalid Rust: {e}"))?;
    Ok((rewritten, added))
}

enum Target {
    /// The item already has `#[derive(...)]`; `inner_range` is the byte
    /// range *inside* the parentheses (excluding the parens themselves)
    /// and `listed` is the set of already-listed derive paths rendered
    /// as strings.
    ExistingDerive {
        inner_range: (usize, usize),
        listed: Vec<String>,
    },
    /// The item has no `#[derive]`; we will insert a fresh attribute on
    /// a new line immediately above the item. `line_start` is the byte
    /// offset of that line's start; `indent` is the leading whitespace
    /// (spaces or tabs) we must replicate on the inserted line.
    InsertAbove { line_start: usize, indent: String },
}

struct Finder<'a> {
    type_name: &'a str,
    target: Option<Target>,
    line_starts: &'a [usize],
    source: &'a str,
}

impl<'a, 'ast> Visit<'ast> for Finder<'a> {
    fn visit_item(&mut self, item: &'ast Item) {
        if self.target.is_some() {
            return;
        }
        match item {
            Item::Struct(s) if s.ident == self.type_name => {
                self.target = self.locate(&s.attrs, s.struct_token.span.start());
            }
            Item::Enum(e) if e.ident == self.type_name => {
                self.target = self.locate(&e.attrs, e.enum_token.span.start());
            }
            Item::Union(u) if u.ident == self.type_name => {
                self.target = self.locate(&u.attrs, u.union_token.span.start());
            }
            _ => {}
        }
        // Do not recurse into items once a match is found; the visitor
        // will otherwise descend into inner modules.
        if self.target.is_none() {
            syn::visit::visit_item(self, item);
        }
    }
}

impl<'a> Finder<'a> {
    fn locate(
        &self,
        attrs: &[syn::Attribute],
        keyword_start: proc_macro2::LineColumn,
    ) -> Option<Target> {
        // Scan for the first `#[derive(...)]` attribute.
        for attr in attrs {
            if !attr.path().is_ident("derive") {
                continue;
            }
            let syn::Meta::List(list) = &attr.meta else {
                continue;
            };
            // The list's `tokens` stream is whatever is inside the
            // parens; the enclosing Group's span covers the `(...)`
            // itself. We reach into `attr.meta.delimiter` via the
            // Group reconstruction below.
            // Turn the attribute's token stream into a Group so we can
            // recover its inner byte span.
            let group = attr_tokens_as_group(attr);
            let group = match group {
                Some(g) => g,
                None => continue,
            };
            let span = group.span();
            let outer_start = span.start();
            let outer_end = span.end();
            let outer_a = linecol_to_byte(
                self.line_starts,
                self.source,
                outer_start.line,
                outer_start.column,
            )?;
            let outer_b = linecol_to_byte(
                self.line_starts,
                self.source,
                outer_end.line,
                outer_end.column,
            )?;
            // Sanity: the group span's byte range must be bracketed by
            // parentheses in the source. Derive always uses `(...)`.
            let outer_slice = self.source.get(outer_a..outer_b)?;
            if !outer_slice.starts_with('(') || !outer_slice.ends_with(')') {
                continue;
            }
            let inner_range = (outer_a + 1, outer_b - 1);
            let listed = collect_listed_derives(&list.tokens);
            return Some(Target::ExistingDerive {
                inner_range,
                listed,
            });
        }
        // No existing derive — fall back to inserting above the
        // keyword. We locate the start of the keyword's line; that
        // line's leading whitespace is the indent we copy onto the
        // inserted attribute so the column stays consistent.
        let kw_byte = linecol_to_byte(
            self.line_starts,
            self.source,
            keyword_start.line,
            keyword_start.column,
        )?;
        let line_start = self
            .line_starts
            .iter()
            .copied()
            .rfind(|s| *s <= kw_byte)
            .unwrap_or(0);
        let indent: String = self.source[line_start..kw_byte]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        Some(Target::InsertAbove { line_start, indent })
    }
}

/// `Attribute::meta` when it's a `MetaList` gives us `tokens` (inside
/// parens) but the enclosing `(...)` group span is hidden. Recover it by
/// walking the attribute's parsed representation — the bracketed
/// `#[...]` outer group contains `derive` + the parenthesised group.
fn attr_tokens_as_group(attr: &syn::Attribute) -> Option<proc_macro2::Group> {
    let syn::Meta::List(list) = &attr.meta else {
        return None;
    };
    // Reconstruct a synthetic Group from the MetaList: the delimiter is
    // always `Parenthesis` for a derive, and the tokens are the inner
    // stream. `MacroDelimiter::span()` returns a `DelimSpan`; `.join()`
    // merges its open/close into a single `Span` whose byte range
    // covers the `(...)` block as it appears in the source.
    let span = list.delimiter.span().join();
    let mut g = proc_macro2::Group::new(Delimiter::Parenthesis, list.tokens.clone());
    g.set_span(span);
    Some(g)
}

/// Collect every top-level derive path present inside a `#[derive(...)]`
/// list, rendered as a whitespace-stripped string (e.g. `"Debug"`,
/// `"serde :: Serialize"` — we compare via [`same_derive`] which
/// normalises whitespace).
fn collect_listed_derives(tokens: &proc_macro2::TokenStream) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for tt in tokens.clone() {
        match tt {
            TokenTree::Punct(p) if p.as_char() == ',' => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    out.push(trimmed);
                }
                current.clear();
            }
            other => {
                current.push_str(&other.to_string());
            }
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
    out
}

fn same_derive(a: &str, b: &str) -> bool {
    normalise(a) == normalise(b)
}

fn normalise(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn is_valid_derive_path(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Accept identifiers or `::`-separated paths. Reject any other
    // punctuation so a malicious plan can't inject arbitrary tokens
    // into the derive list (belt-and-braces; the post-parse check would
    // catch most of these too).
    let mut chars = s.chars().peekable();
    let mut start_of_segment = true;
    while let Some(c) = chars.next() {
        if start_of_segment {
            if !(c.is_ascii_alphabetic() || c == '_') {
                return false;
            }
            start_of_segment = false;
        } else if c == ':' {
            if chars.next() != Some(':') {
                return false;
            }
            start_of_segment = true;
        } else if !(c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    !start_of_segment
}

fn line_starts(src: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

fn linecol_to_byte(line_starts: &[usize], src: &str, line: usize, column: usize) -> Option<usize> {
    if line == 0 || line > line_starts.len() {
        return None;
    }
    let line_start = line_starts[line - 1];
    let line_end = line_starts.get(line).copied().unwrap_or(src.len());
    let line_text = &src[line_start..line_end];
    if line_text.is_ascii() {
        return Some(line_start + column);
    }
    // Same UTF-8 fallback path as `rust_rename.rs`.
    for (i, _c) in line_text.char_indices() {
        let prior_chars = line_text[..i].chars().count();
        if prior_chars == column {
            return Some(line_start + i);
        }
    }
    Some(line_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_new_derive_above_struct_without_attribute() {
        let src = "pub struct Counter {\n    value: i32,\n}\n";
        let (out, n) = add_derive(src, "Counter", &["Debug".into(), "Clone".into()]).unwrap();
        assert_eq!(n, 2);
        assert!(
            out.starts_with("#[derive(Debug, Clone)]\npub struct Counter"),
            "{out}"
        );
    }

    #[test]
    fn merges_into_existing_derive_skipping_duplicates() {
        let src = "#[derive(Debug)]\npub struct Counter { v: i32 }\n";
        let (out, n) = add_derive(
            src,
            "Counter",
            &["Debug".into(), "Clone".into(), "PartialEq".into()],
        )
        .unwrap();
        // Debug already present → only two new derives.
        assert_eq!(n, 2);
        assert!(out.contains("#[derive(Debug, Clone, PartialEq)]"), "{out}");
    }

    #[test]
    fn idempotent_when_all_derives_already_present() {
        let src = "#[derive(Debug, Clone)]\nstruct X;\n";
        let (out, n) = add_derive(src, "X", &["Debug".into(), "Clone".into()]).unwrap();
        assert_eq!(n, 0);
        assert_eq!(out, src);
    }

    #[test]
    fn target_not_found_leaves_source_untouched() {
        let src = "struct Y;\n";
        let (out, n) = add_derive(src, "Z", &["Debug".into()]).unwrap();
        assert_eq!(n, 0);
        assert_eq!(out, src);
    }

    #[test]
    fn works_on_enum_and_union() {
        let src_e = "enum E { A, B }\n";
        let (out_e, n_e) = add_derive(src_e, "E", &["Debug".into()]).unwrap();
        assert_eq!(n_e, 1);
        assert!(out_e.contains("#[derive(Debug)]\nenum E"), "{out_e}");

        let src_u = "union U { a: u32 }\n";
        let (out_u, n_u) = add_derive(src_u, "U", &["Copy".into()]).unwrap();
        assert_eq!(n_u, 1);
        assert!(out_u.contains("#[derive(Copy)]\nunion U"), "{out_u}");
    }

    #[test]
    fn indentation_is_preserved_when_inserting() {
        let src = "mod inner {\n    pub struct Inside { v: i32 }\n}\n";
        let (out, n) = add_derive(src, "Inside", &["Debug".into()]).unwrap();
        assert_eq!(n, 1);
        assert!(
            out.contains("    #[derive(Debug)]\n    pub struct Inside"),
            "{out}"
        );
    }

    #[test]
    fn rejects_invalid_derive_path() {
        let src = "struct S;\n";
        let err = add_derive(src, "S", &["has space".into()]).unwrap_err();
        assert!(err.contains("invalid derive path"));
    }

    #[test]
    fn accepts_path_derives() {
        let src = "struct S;\n";
        // We don't bring `serde` into the fixture so the post-parse
        // check still succeeds (derive path resolution happens at
        // compile time, not parse time).
        let (out, n) = add_derive(src, "S", &["serde::Serialize".into()]).unwrap();
        assert_eq!(n, 1);
        assert!(out.contains("#[derive(serde::Serialize)]"), "{out}");
    }
}
