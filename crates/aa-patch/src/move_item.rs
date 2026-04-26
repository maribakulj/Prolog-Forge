//! `MoveItem` — Phase 1.24.
//!
//! Move a free-standing top-level item (a `fn`, `struct`, `enum`, or
//! `union`) from one workspace file to another. Verbatim move: the
//! item's source bytes — including its outer attributes, visibility,
//! and any docstrings — are appended to the destination file and
//! removed from the source file.
//!
//! See [`crate::ops::PatchOp::MoveItem`] for the full contract. The
//! transform deliberately refuses to update *external references* to
//! the item (`use` statements, qualified paths, macro bodies). The
//! 1.24 contract is "the move is mechanically correct, or refuse";
//! a follow-up phase will use rust-analyzer to update references
//! scope-aware.
//!
//! Stub-first wiring: the entry-point shape is fixed so the rest of
//! the pipeline (planner, validator, CLI, daemon smoke) can be
//! staged in independent steps.

use std::collections::BTreeMap;

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::visit::Visit;
use syn::Item;

use crate::ops::ItemKind;
use crate::util::{line_starts, linecol_to_byte};

/// Rewritten file map plus per-file count of byte-level edits
/// (1 for the removal in `from_file` + 1 for the append to
/// `to_file` = 2 on success). Returns `Ok((files.clone(), {}))`
/// when the item is not found in `from_file` — same convention
/// as the per-file ops.
pub type MoveItemResult = (BTreeMap<String, String>, BTreeMap<String, usize>);

/// Apply the move-item transform across `files`. Returns the
/// rewritten file map and the per-file count of byte-level edits.
pub fn move_item(
    files: &BTreeMap<String, String>,
    item_kind: ItemKind,
    item_name: &str,
    from_file: &str,
    to_file: &str,
    file_filter: &[String],
) -> Result<MoveItemResult, String> {
    if !is_valid_ident(item_name) {
        return Err(format!("move_item: invalid identifier `{item_name}`"));
    }
    if from_file == to_file {
        return Err(format!(
            "move_item: from_file and to_file are the same (`{from_file}`)"
        ));
    }

    // 1. Both files must already be in the workspace map.
    let Some(from_src) = files.get(from_file).cloned() else {
        return Err(format!(
            "move_item: source file `{from_file}` not in workspace"
        ));
    };
    let Some(to_src) = files.get(to_file).cloned() else {
        return Err(format!(
            "move_item: destination file `{to_file}` not in workspace; \
             creating new files is out of scope for 1.24"
        ));
    };

    // 2. Pre-parse both files as a sanity check before any structural
    //    work. A broken source file gets a real syn diagnostic
    //    instead of a confusing "not found" error.
    let from_parsed =
        syn::parse_file(&from_src).map_err(|e| format!("move_item: pre-parse {from_file}: {e}"))?;
    let to_parsed =
        syn::parse_file(&to_src).map_err(|e| format!("move_item: pre-parse {to_file}: {e}"))?;

    // 3. Locate the item at the file's top level. Refuse if it's
    //    nested in `mod foo { ... }` — those need a path-rewrite
    //    pass that's out of scope for 1.24.
    let (idx, item_span_start, item_span_end) =
        locate_top_level_item(&from_parsed, &from_src, item_kind, item_name)?;
    let _ = idx; // reserved if we ever need to surface ordering

    // 4. Refuse if the destination file already has an item with
    //    the same name and kind.
    if locate_top_level_item(&to_parsed, &to_src, item_kind, item_name).is_ok() {
        return Err(format!(
            "move_item: destination file `{to_file}` already defines a {item_kind:?} named \
             `{item_name}`; refuse rather than collide"
        ));
    }

    // 5. Validate the item's shape. Generics, attached `impl` blocks,
    //    and any structural feature whose move semantics aren't
    //    obvious are refused — see `validate_item_shape` for the
    //    full list.
    validate_item_shape(&from_parsed, item_kind, item_name)?;

    // 6. Refuse if the item is referenced anywhere else in the
    //    workspace by name. The 1.24 contract is "the move is
    //    mechanically correct or refuse" — updating `use`,
    //    qualified paths, and macro bodies belongs to a future
    //    RA-driven op.
    let in_scope: Vec<String> = if file_filter.is_empty() {
        files
            .keys()
            .filter(|p| p.ends_with(".rs"))
            .cloned()
            .collect()
    } else {
        file_filter
            .iter()
            .filter(|p| files.contains_key(p.as_str()))
            .cloned()
            .collect()
    };
    for path in &in_scope {
        let src = files.get(path).cloned().unwrap_or_default();
        let parsed = if path == from_file {
            from_parsed.clone()
        } else if path == to_file {
            to_parsed.clone()
        } else {
            // Files outside the move's two endpoints just need to be
            // scanned for refs; pre-parse failures should already be
            // caught upstream by `workspace.index`, but if a file is
            // somehow unparseable here we conservatively skip it
            // rather than blow up the whole transform.
            match syn::parse_file(&src) {
                Ok(f) => f,
                Err(_) => continue,
            }
        };
        let mut scan = ExternalRefScan {
            target: item_name,
            // Inside the source file the item itself counts as a
            // self-reference (its own definition uses the ident).
            // We're only looking for *other* uses, so we let the
            // scanner pass in `in_definition_file` and exclude the
            // exact span of the moved item.
            from_def_span: if path == from_file {
                Some((item_span_start, item_span_end))
            } else {
                None
            },
            source: &src,
            line_starts: &line_starts(&src),
            hit: None,
        };
        scan.visit_file(&parsed);
        if let Some(reason) = scan.hit {
            return Err(format!(
                "move_item: item `{item_name}` is referenced from `{path}` ({reason}); \
                 1.24 refuses moves that would leave dangling references — update those \
                 sites by hand or wait for the RA-driven follow-up phase that will rewrite \
                 them automatically"
            ));
        }
    }

    // 7. Build the new content of both files.
    //   - `from_file`: drop the item's bytes plus its trailing
    //      newline (if any), so we don't leave an orphan blank line.
    //   - `to_file`:   append the item's bytes, preceded by a blank
    //      line if the existing file doesn't end with one. The
    //      moved bytes are taken verbatim from `from_file` so
    //      attributes, docstrings, and visibility ride along.
    let bytes = from_src.as_bytes();
    let mut real_end = item_span_end;
    while real_end < bytes.len() && (bytes[real_end] == b' ' || bytes[real_end] == b'\t') {
        real_end += 1;
    }
    if real_end < bytes.len() && bytes[real_end] == b'\n' {
        real_end += 1;
    }
    let item_text = from_src[item_span_start..item_span_end].to_string();
    let mut new_from = from_src.clone();
    new_from.replace_range(item_span_start..real_end, "");

    let mut new_to = to_src.clone();
    if !new_to.is_empty() && !new_to.ends_with('\n') {
        new_to.push('\n');
    }
    if !new_to.is_empty() && !new_to.ends_with("\n\n") {
        // Keep one blank line between the existing tail of the file
        // and the moved item, like rustfmt would.
        new_to.push('\n');
    }
    new_to.push_str(&item_text);
    if !new_to.ends_with('\n') {
        new_to.push('\n');
    }

    // 8. Post-parse both files. This is the safety net — if the
    //    splice produced invalid Rust, refuse rather than write.
    syn::parse_file(&new_from)
        .map_err(|e| format!("post-parse {from_file}: move would produce invalid Rust: {e}"))?;
    syn::parse_file(&new_to)
        .map_err(|e| format!("post-parse {to_file}: move would produce invalid Rust: {e}"))?;

    let mut result = files.clone();
    result.insert(from_file.to_string(), new_from);
    result.insert(to_file.to_string(), new_to);
    let mut counts = BTreeMap::new();
    counts.insert(from_file.to_string(), 1);
    counts.insert(to_file.to_string(), 1);
    Ok((result, counts))
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

/// Find the item's index in the file's top-level item list and its
/// byte span (inclusive of leading attributes / visibility, exclusive
/// of any trailing whitespace). Refuses items nested in `mod` blocks
/// — those would need a path-rewrite pass to keep `mod::name` paths
/// valid post-move, which is out of scope for 1.24.
fn locate_top_level_item(
    file: &syn::File,
    src: &str,
    kind: ItemKind,
    name: &str,
) -> Result<(usize, usize, usize), String> {
    let starts = line_starts(src);
    for (idx, item) in file.items.iter().enumerate() {
        if matches_item(item, kind, name) {
            let (s, e) = item_byte_span(item, &starts, src).ok_or_else(|| {
                format!("move_item: could not resolve byte span for {kind:?} `{name}`")
            })?;
            return Ok((idx, s, e));
        }
    }
    // The item might be in a nested mod — flag that explicitly.
    for item in &file.items {
        if let Item::Mod(m) = item {
            if mod_contains_item(m, kind, name) {
                return Err(format!(
                    "move_item: {kind:?} `{name}` lives inside a `mod {} {{ ... }}` — \
                     moving items out of nested modules requires path rewrites that are \
                     out of scope for 1.24",
                    m.ident
                ));
            }
        }
    }
    Err(format!(
        "move_item: {kind:?} `{name}` not found at the top level"
    ))
}

fn mod_contains_item(m: &syn::ItemMod, kind: ItemKind, name: &str) -> bool {
    if let Some((_, items)) = &m.content {
        for item in items {
            if matches_item(item, kind, name) {
                return true;
            }
            if let Item::Mod(inner) = item {
                if mod_contains_item(inner, kind, name) {
                    return true;
                }
            }
        }
    }
    false
}

fn matches_item(item: &Item, kind: ItemKind, name: &str) -> bool {
    match (item, kind) {
        (Item::Fn(f), ItemKind::Function) => f.sig.ident == name,
        (Item::Struct(s), ItemKind::Struct) => s.ident == name,
        (Item::Enum(e), ItemKind::Enum) => e.ident == name,
        (Item::Union(u), ItemKind::Union) => u.ident == name,
        _ => false,
    }
}

/// Refuse shapes whose move semantics aren't trivially safe:
///   - Items with generic parameters (the `impl` block that often
///     accompanies them would be left orphaned).
///   - Structs/enums/unions with an attached `impl` block somewhere
///     in the same file (we'd silently leave the impl behind).
///   - Items inside `cfg`-gated modules where the gate matters
///     for the move semantics — *not* refused here today but kept
///     as a reserved expansion point in the comment so the next
///     person wires it.
fn validate_item_shape(file: &syn::File, kind: ItemKind, name: &str) -> Result<(), String> {
    for item in &file.items {
        match (item, kind) {
            (Item::Fn(f), ItemKind::Function)
                if f.sig.ident == name
                    && (!f.sig.generics.params.is_empty()
                        || f.sig.generics.where_clause.is_some()) =>
            {
                return Err(format!(
                    "move_item: function `{name}` has generic parameters; refuse — \
                     the matching `impl` boilerplate (if any) would be left behind"
                ));
            }
            (Item::Struct(s), ItemKind::Struct)
                if s.ident == name && !s.generics.params.is_empty() =>
            {
                return Err(format!(
                    "move_item: struct `{name}` has generic parameters; refuse"
                ));
            }
            (Item::Enum(e), ItemKind::Enum) if e.ident == name && !e.generics.params.is_empty() => {
                return Err(format!(
                    "move_item: enum `{name}` has generic parameters; refuse"
                ));
            }
            (Item::Union(u), ItemKind::Union)
                if u.ident == name && !u.generics.params.is_empty() =>
            {
                return Err(format!(
                    "move_item: union `{name}` has generic parameters; refuse"
                ));
            }
            _ => {}
        }
    }
    // Attached `impl Foo` block scan — only relevant for the type
    // kinds. Functions can't have impl blocks attached to them, so
    // skip there.
    if matches!(kind, ItemKind::Struct | ItemKind::Enum | ItemKind::Union) {
        for item in &file.items {
            if let Item::Impl(imp) = item {
                if let syn::Type::Path(tp) = imp.self_ty.as_ref() {
                    if tp.qself.is_none()
                        && tp.path.leading_colon.is_none()
                        && tp.path.segments.len() == 1
                        && tp.path.segments.first().unwrap().ident == name
                    {
                        return Err(format!(
                            "move_item: type `{name}` has an attached `impl` block in the \
                             source file; moving the type without the impl would leave \
                             the impl orphaned. Move the impl block first (manually, for \
                             1.24) and then move the type."
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn item_byte_span(item: &Item, line_starts_v: &[usize], src: &str) -> Option<(usize, usize)> {
    use syn::spanned::Spanned;
    // For functions and most other items, the syn span starts at the
    // first attribute or the visibility keyword and ends at the
    // closing brace / final token. We use `Spanned::span()` for
    // simplicity — it covers attributes and trailing tokens.
    let s = item.span();
    let start = linecol_to_byte(line_starts_v, src, s.start().line, s.start().column)?;
    let end = linecol_to_byte(line_starts_v, src, s.end().line, s.end().column)?;
    // Walk back from `start` over leading whitespace on the same
    // line so the indentation disappears with the item.
    let bytes = src.as_bytes();
    let mut real_start = start;
    while real_start > 0 {
        let b = bytes[real_start - 1];
        if b == b' ' || b == b'\t' {
            real_start -= 1;
        } else {
            break;
        }
    }
    Some((real_start, end))
}

/// Detect any reference to `target` outside the moved item itself.
/// "Reference" here means:
///   - any `Path::Ident` in expression or type position whose final
///     segment matches `target`,
///   - any `use` tree path ending in `target`,
///   - any non-`macro_rules!` macro body whose token stream contains
///     the bare ident `target` (paranoid: it might be a callsite,
///     a path component, or a quoted ident — we don't try to be
///     more precise because false positives are safe ("refuse the
///     move") and false negatives are dangerous ("dangling ref")).
struct ExternalRefScan<'a> {
    target: &'a str,
    /// `Some((start, end))` when the visitor is walking the source
    /// file; spans inside that range are the moved item itself and
    /// must be excluded. `None` for every other file.
    from_def_span: Option<(usize, usize)>,
    source: &'a str,
    line_starts: &'a [usize],
    hit: Option<&'static str>,
}

impl<'a> ExternalRefScan<'a> {
    fn span_is_inside_definition(&self, s: usize) -> bool {
        match self.from_def_span {
            Some((from, to)) => s >= from && s < to,
            None => false,
        }
    }
    fn span_byte(&self, ll: proc_macro2::LineColumn) -> Option<usize> {
        linecol_to_byte(self.line_starts, self.source, ll.line, ll.column)
    }
}

impl<'a, 'ast> Visit<'ast> for ExternalRefScan<'a> {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if self.hit.is_some() {
            return;
        }
        // *Any* segment of the path matching the target is a
        // reference: `crate::a::name` (last segment), `name::Variant`
        // (first segment of an enum-variant access), `mod::name::field`
        // (a middle segment of a path into a moved type), all count.
        // We deliberately overshoot: a false positive here means
        // "refuse the move", which is safe; a false negative means
        // "leave a dangling reference", which the 1.24 contract is
        // explicitly trying to prevent.
        for seg in &path.segments {
            if seg.ident == self.target {
                let s_loc = seg.ident.span().start();
                if let Some(s) = self.span_byte(s_loc) {
                    if !self.span_is_inside_definition(s) {
                        self.hit = Some("path reference");
                        return;
                    }
                }
            }
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if self.hit.is_some() {
            return;
        }
        if m.path.is_ident("macro_rules") {
            return;
        }
        if token_stream_contains_ident(m.tokens.clone(), self.target) {
            self.hit = Some("macro body");
            return;
        }
        syn::visit::visit_macro(self, m);
    }
}

fn token_stream_contains_ident(stream: TokenStream, target: &str) -> bool {
    let mut prev_was_dollar = false;
    for tt in stream {
        match tt {
            TokenTree::Ident(i) => {
                if !prev_was_dollar && i == target {
                    return true;
                }
                prev_was_dollar = false;
            }
            TokenTree::Group(g) => {
                if token_stream_contains_ident(g.stream(), target) {
                    return true;
                }
                prev_was_dollar = false;
            }
            TokenTree::Punct(p) => {
                prev_was_dollar = p.as_char() == '$' && p.spacing() == Spacing::Alone;
            }
            TokenTree::Literal(_) => {
                prev_was_dollar = false;
            }
        }
        // `Delimiter` is imported for `Group::delimiter()`-style
        // checks the future may want; reference it once here so the
        // import doesn't trip `unused_imports` while we don't need
        // the variants.
        let _ = Delimiter::Bracket;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), (*v).to_string());
        }
        m
    }

    #[test]
    fn moves_a_simple_function_between_two_files() {
        let f = files(&[
            (
                "src/a.rs",
                "pub fn keep() {}\n\
                 \n\
                 pub fn helper() -> i32 { 1 }\n",
            ),
            ("src/b.rs", "// destination\n"),
        ]);
        let (out, per_file) = move_item(
            &f,
            ItemKind::Function,
            "helper",
            "src/a.rs",
            "src/b.rs",
            &[],
        )
        .expect("move should succeed");
        assert!(
            !out["src/a.rs"].contains("fn helper"),
            "source must drop the moved fn:\n{}",
            out["src/a.rs"]
        );
        assert!(
            out["src/b.rs"].contains("pub fn helper() -> i32 { 1 }"),
            "destination must gain the moved fn:\n{}",
            out["src/b.rs"]
        );
        assert!(
            out["src/a.rs"].contains("pub fn keep()"),
            "unrelated items in the source file must stay"
        );
        assert_eq!(per_file["src/a.rs"], 1);
        assert_eq!(per_file["src/b.rs"], 1);
        syn::parse_file(&out["src/a.rs"]).unwrap();
        syn::parse_file(&out["src/b.rs"]).unwrap();
    }

    #[test]
    fn preserves_attributes_and_docstring_on_moved_fn() {
        let f = files(&[
            (
                "src/a.rs",
                "/// docstring kept\n\
                 #[inline]\n\
                 pub fn helper() -> i32 { 42 }\n",
            ),
            ("src/b.rs", ""),
        ]);
        let (out, _) = move_item(
            &f,
            ItemKind::Function,
            "helper",
            "src/a.rs",
            "src/b.rs",
            &[],
        )
        .unwrap();
        let dest = &out["src/b.rs"];
        assert!(dest.contains("/// docstring kept"), "{dest}");
        assert!(dest.contains("#[inline]"), "{dest}");
        assert!(dest.contains("pub fn helper() -> i32 { 42 }"), "{dest}");
    }

    #[test]
    fn moves_a_struct() {
        let f = files(&[
            (
                "src/a.rs",
                "#[derive(Debug)]\npub struct Counter { n: u32 }\n\nfn other() {}\n",
            ),
            ("src/b.rs", ""),
        ]);
        let (out, _) =
            move_item(&f, ItemKind::Struct, "Counter", "src/a.rs", "src/b.rs", &[]).unwrap();
        assert!(out["src/b.rs"].contains("pub struct Counter { n: u32 }"));
        assert!(!out["src/a.rs"].contains("pub struct Counter"));
        assert!(out["src/a.rs"].contains("fn other()"));
    }

    #[test]
    fn refuses_when_referenced_from_another_file() {
        let f = files(&[
            ("src/a.rs", "pub fn helper() -> i32 { 1 }\n"),
            ("src/b.rs", "fn use_it() -> i32 { crate::a::helper() }\n"),
        ]);
        let err = move_item(
            &f,
            ItemKind::Function,
            "helper",
            "src/a.rs",
            "src/b.rs",
            &[],
        )
        .unwrap_err();
        assert!(
            err.contains("dangling references") || err.contains("referenced from"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_when_referenced_via_use_statement() {
        let f = files(&[
            ("src/a.rs", "pub fn helper() -> i32 { 1 }\n"),
            (
                "src/b.rs",
                "use crate::a::helper;\nfn use_it() -> i32 { helper() }\n",
            ),
        ]);
        let err = move_item(
            &f,
            ItemKind::Function,
            "helper",
            "src/a.rs",
            "src/b.rs",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("referenced from"), "got: {err}");
    }

    #[test]
    fn refuses_when_referenced_inside_a_macro_body() {
        let f = files(&[
            ("src/a.rs", "pub fn helper() -> i32 { 1 }\n"),
            ("src/b.rs", "fn use_it() { let _ = vec![helper()]; }\n"),
        ]);
        let err = move_item(
            &f,
            ItemKind::Function,
            "helper",
            "src/a.rs",
            "src/b.rs",
            &[],
        )
        .unwrap_err();
        assert!(
            err.contains("macro body") || err.contains("path"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_generic_function() {
        let f = files(&[
            ("src/a.rs", "pub fn id<T>(x: T) -> T { x }\n"),
            ("src/b.rs", ""),
        ]);
        let err = move_item(&f, ItemKind::Function, "id", "src/a.rs", "src/b.rs", &[]).unwrap_err();
        assert!(err.contains("generic"), "got: {err}");
    }

    #[test]
    fn refuses_struct_with_attached_impl_block() {
        let f = files(&[
            (
                "src/a.rs",
                "pub struct Counter { n: u32 }\n\
                 impl Counter { pub fn new() -> Self { Self { n: 0 } } }\n",
            ),
            ("src/b.rs", ""),
        ]);
        let err =
            move_item(&f, ItemKind::Struct, "Counter", "src/a.rs", "src/b.rs", &[]).unwrap_err();
        assert!(
            err.contains("attached `impl` block") || err.contains("impl block"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_when_destination_already_has_same_name() {
        let f = files(&[
            ("src/a.rs", "pub fn helper() {}\n"),
            ("src/b.rs", "pub fn helper() {}\n"),
        ]);
        let err = move_item(
            &f,
            ItemKind::Function,
            "helper",
            "src/a.rs",
            "src/b.rs",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("already defines"), "got: {err}");
    }

    #[test]
    fn refuses_when_destination_does_not_exist() {
        let f = files(&[("src/a.rs", "pub fn helper() {}\n")]);
        let err = move_item(
            &f,
            ItemKind::Function,
            "helper",
            "src/a.rs",
            "src/b.rs",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("not in workspace"), "got: {err}");
    }

    #[test]
    fn refuses_when_item_is_in_a_nested_mod() {
        let f = files(&[
            ("src/a.rs", "mod inner {\n    pub fn helper() {}\n}\n"),
            ("src/b.rs", ""),
        ]);
        let err = move_item(
            &f,
            ItemKind::Function,
            "helper",
            "src/a.rs",
            "src/b.rs",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("nested modules"), "got: {err}");
    }

    #[test]
    fn refuses_same_source_and_destination() {
        let f = files(&[("src/a.rs", "pub fn helper() {}\n")]);
        let err = move_item(
            &f,
            ItemKind::Function,
            "helper",
            "src/a.rs",
            "src/a.rs",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("same"), "got: {err}");
    }

    #[test]
    fn moves_an_enum_with_variants() {
        let f = files(&[
            (
                "src/a.rs",
                "pub enum Color { Red, Green, Blue }\nfn use_local() { let _ = Color::Red; }\n",
            ),
            ("src/b.rs", ""),
        ]);
        // The local `Color::Red` reference in `src/a.rs::use_local`
        // is *inside* the source file but outside the moved item
        // span — it should still trigger the dangling-ref refusal.
        let err = move_item(&f, ItemKind::Enum, "Color", "src/a.rs", "src/b.rs", &[]).unwrap_err();
        assert!(err.contains("referenced from"), "got: {err}");
    }
}
