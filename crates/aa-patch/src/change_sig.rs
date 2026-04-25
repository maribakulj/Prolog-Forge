//! `ChangeSignature` — Phase 1.23.
//!
//! Reorder a free-standing function's parameters and optionally
//! rename them. The transform rewrites the signature and every bare
//! call site so the arguments at each call line up with the param
//! they were originally bound to.
//!
//! See [`crate::ops::PatchOp::ChangeSignature`] for the full
//! contract. Implementation grows incrementally — the entry point
//! shape is fixed so the wire layer ([`crate::plan::apply_op`]) and
//! the validators (`aa-llm::propose_patch`, `aa-core::handlers`)
//! can be wired ahead of the transform itself.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::visit::Visit;
use syn::{Expr, FnArg, ImplItem, Item};

use crate::ops::ParamReorder;
use crate::util::{line_starts, linecol_to_byte};

/// Rewritten file map plus per-file count of byte-level edits
/// (1 for the signature rewrite + 1 per substituted call site +
/// 1 per renamed-param body site). Returns `Ok((files.clone(), {}))`
/// when the target is not found in scope — same convention as
/// the per-file ops.
pub type ChangeSigResult = (BTreeMap<String, String>, BTreeMap<String, usize>);

/// Apply the change-signature transform across `files`. Returns the
/// rewritten file map and the per-file count of byte-level edits.
pub fn change_signature(
    files: &BTreeMap<String, String>,
    function: &str,
    new_params: &[ParamReorder],
    file_filter: &[String],
) -> Result<ChangeSigResult, String> {
    if !is_valid_ident(function) {
        return Err(format!("invalid Rust identifier `{function}`"));
    }
    if new_params.is_empty() {
        return Err("change_signature: `new_params` must be non-empty".into());
    }

    // 1. Narrow scope to the requested files (empty filter = all `.rs`).
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
    if in_scope.is_empty() {
        return Ok((files.clone(), BTreeMap::new()));
    }

    // 2. Pre-parse every scope file once.
    let mut parsed: BTreeMap<String, syn::File> = BTreeMap::new();
    for path in &in_scope {
        let src = files.get(path).cloned().unwrap_or_default();
        let file = syn::parse_file(&src).map_err(|e| format!("pre-parse {path}: {e}"))?;
        parsed.insert(path.clone(), file);
    }

    // 3. Locate the unique free-standing fn definition.
    let mut matches: Vec<(String, syn::ItemFn)> = Vec::new();
    for (path, file) in &parsed {
        for item in &file.items {
            if let Item::Fn(f) = item {
                if f.sig.ident == function {
                    matches.push((path.clone(), f.clone()));
                }
            }
            if let Item::Mod(m) = item {
                collect_mod_fns(m, function, path, &mut matches);
            }
        }
    }
    if matches.is_empty() {
        return Ok((files.clone(), BTreeMap::new()));
    }
    if matches.len() > 1 {
        return Err(format!(
            "change_signature `{function}`: ambiguous — {} definitions found across scope; \
             narrow `files` to disambiguate",
            matches.len()
        ));
    }
    let (def_file, item_fn) = matches.into_iter().next().unwrap();

    // 4. Refuse method collision (same posture as InlineFunction):
    //    a method on an `impl` block sharing the name is a different
    //    symbol our reordering would silently leave untouched.
    for file in parsed.values() {
        for item in &file.items {
            if let Item::Impl(imp) = item {
                for ii in &imp.items {
                    if let ImplItem::Fn(m) = ii {
                        if m.sig.ident == function {
                            return Err(format!(
                                "change_signature `{function}`: name collides with a method \
                                 in an `impl` block; refuse rather than reorder ambiguously"
                            ));
                        }
                    }
                }
            }
        }
    }

    // 5. Validate the fn's signature shape (free-standing only).
    validate_fn_shape(&item_fn, function)?;

    // 6. Validate `new_params` is a permutation of [0, n).
    let arity = item_fn.sig.inputs.len();
    if new_params.len() != arity {
        return Err(format!(
            "change_signature `{function}`: new_params has {} entries but the function takes {arity} parameters; \
             1.23 is permutation-only — adding or removing parameters is not supported",
            new_params.len()
        ));
    }
    let mut seen_indices: BTreeSet<usize> = BTreeSet::new();
    for (i, p) in new_params.iter().enumerate() {
        if p.from_index >= arity {
            return Err(format!(
                "change_signature `{function}`: new_params[{i}].from_index {} is out of range \
                 (function arity is {arity})",
                p.from_index
            ));
        }
        if !seen_indices.insert(p.from_index) {
            return Err(format!(
                "change_signature `{function}`: from_index {} listed twice; new_params must be \
                 a permutation",
                p.from_index
            ));
        }
        if let Some(new_name) = &p.rename {
            if !is_valid_ident(new_name) {
                return Err(format!(
                    "change_signature `{function}`: new_params[{i}].rename `{new_name}` is not \
                     a valid Rust identifier"
                ));
            }
        }
    }

    // 7. Refuse macro-body call sites and qualified-path uses anywhere
    //    in scope. Same gate as `inline_function::5.5` — reordering
    //    only the bare calls would silently desync the rest.
    for (path, file) in &parsed {
        let mut scan = MacroScan {
            target: function,
            hit: false,
        };
        scan.visit_file(file);
        if scan.hit {
            return Err(format!(
                "change_signature `{function}`: appears inside a macro body in `{path}`. \
                 Reordering across macro token streams is not supported — refuse rather \
                 than produce a half-reordered program."
            ));
        }
    }
    for path in parsed.keys() {
        let src = files.get(path).cloned().unwrap_or_default();
        let total = count_ident_occurrences(&src, function)?;
        let bare_call_sites_here = {
            let parsed_file = parsed.get(path).unwrap();
            let src_line_starts = line_starts(&src);
            let mut c = CallCollector {
                target: function,
                call_sites: Vec::new(),
                line_starts: &src_line_starts,
                source: &src,
            };
            c.visit_file(parsed_file);
            c.call_sites.len()
        };
        let def_here = if path == &def_file { 1 } else { 0 };
        let expected = bare_call_sites_here + def_here;
        if total > expected {
            return Err(format!(
                "change_signature `{function}`: `{path}` references the function {total} time(s) \
                 but only {expected} of those are bare call sites or definitions. Reordering \
                 would leave non-bare references (qualified-path calls, `use` re-exports) \
                 silently aligned to the old signature. Refuse rather than half-reorder."
            ));
        }
    }

    // 8. Collect each existing parameter's source span + name. We
    //    need both: the spans drive the signature rewrite (we
    //    *replace bytes*, never re-print), and the names drive the
    //    body rename. Refusing every shape but `ident: Type` keeps
    //    the analysis simple and matches `inline_function`'s param
    //    posture.
    let def_src = files.get(&def_file).cloned().unwrap_or_default();
    let def_line_starts = line_starts(&def_src);
    let original_params: Vec<OriginalParam> =
        collect_original_params(&item_fn, &def_src, &def_line_starts)?;

    // Detect renames that would collide with a different param's
    // existing name — that's a swap, which is well-defined under
    // permutation (the FROM index of the rename target is moving
    // anyway). What we must refuse is a rename to a name that's
    // *not* in the original list and clashes with anything else
    // in the body. We delegate the collision check to the body
    // rename below; here we just make sure the renames-list
    // itself doesn't ask us to map two params to the same new
    // name.
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for (i, p) in new_params.iter().enumerate() {
        let final_name = p
            .rename
            .clone()
            .unwrap_or_else(|| original_params[p.from_index].name.clone());
        if !taken.insert(final_name.clone()) {
            return Err(format!(
                "change_signature `{function}`: new_params[{i}] resolves to name `{final_name}` \
                 which is already taken by another slot; the resulting signature would be \
                 syntactically invalid"
            ));
        }
    }

    // 9. Render the new params list source. Each param's text is
    //    either the original verbatim (no rename) or the original
    //    with the ident swapped (rename). We splice them in the
    //    `new_params` order separated by `, `. Comma handling: the
    //    new list is always rendered without a trailing comma, even
    //    if the original had one, to match rustfmt convention.
    let mut new_params_src = String::new();
    for (i, p) in new_params.iter().enumerate() {
        if i > 0 {
            new_params_src.push_str(", ");
        }
        let original = &original_params[p.from_index];
        match &p.rename {
            None => new_params_src.push_str(&original.text),
            Some(new_name) => {
                // Rewrite the param's source text so the ident is
                // replaced. We re-find the ident inside the param's
                // own bytes rather than tracking another span — the
                // param text is small and the ident is unique within
                // it (we refused complex Pat shapes upstream).
                let ident_start = original.text.find(&original.name).ok_or_else(|| {
                    format!(
                        "change_signature internal: ident `{}` not found in param text `{}`",
                        original.name, original.text
                    )
                })?;
                let ident_end = ident_start + original.name.len();
                let mut replaced = original.text.clone();
                replaced.replace_range(ident_start..ident_end, new_name);
                new_params_src.push_str(&replaced);
            }
        }
    }

    // 10. Build per-file edit lists. Edits within a file are applied
    //     descending so earlier byte offsets stay valid.
    let mut result_files = files.clone();
    let mut per_file_count: BTreeMap<String, usize> = BTreeMap::new();

    // 10a. Call-site rewrites in every scope file. Each call site
    //      gets its argument list permuted to match `new_params`.
    let perm: Vec<usize> = new_params.iter().map(|p| p.from_index).collect();
    for path in &in_scope {
        let src = result_files.get(path).cloned().unwrap_or_default();
        let parsed_file = parsed.get(path).unwrap();
        let path_line_starts = line_starts(&src);
        let mut collector = CallCollector {
            target: function,
            call_sites: Vec::new(),
            line_starts: &path_line_starts,
            source: &src,
        };
        collector.visit_file(parsed_file);
        if collector.call_sites.is_empty() {
            continue;
        }
        // Sanity: every call site must have the right arity. Anything
        // else is either a different fn shadowing the name or a
        // partial-application macro game we don't support.
        for site in &collector.call_sites {
            if site.args.len() != arity {
                return Err(format!(
                    "change_signature `{function}`: call site in `{path}` has {} argument(s) but \
                     the function takes {arity} parameter(s); refuse rather than misalign",
                    site.args.len()
                ));
            }
        }
        // Rewrite descending.
        let mut sites = collector.call_sites;
        sites.sort_by_key(|s| std::cmp::Reverse(s.args_open));
        let mut out = src.clone();
        let mut count = 0usize;
        for site in &sites {
            let arg_texts: Vec<String> = site
                .args
                .iter()
                .map(|(s, e)| src[*s..*e].to_string())
                .collect();
            let mut new_args = String::new();
            for (i, &from_idx) in perm.iter().enumerate() {
                if i > 0 {
                    new_args.push_str(", ");
                }
                new_args.push_str(arg_texts[from_idx].trim());
            }
            out.replace_range(site.args_open..site.args_close, &new_args);
            count += 1;
        }
        result_files.insert(path.clone(), out);
        *per_file_count.entry(path.clone()).or_insert(0) += count;
    }

    // 10b. Signature rewrite in the def file. Run *after* the
    //      call-site pass since the def-file may itself contain
    //      call sites; the sig span still applies because we
    //      computed it from the original source and the call-site
    //      rewrite in the def-file happens at strictly lower
    //      offsets only when the calls are *before* the def — a
    //      situation that's possible (`fn main(){f(1,2)} fn f(...)`),
    //      so we recompute the span from the post-call-site source
    //      to be safe.
    {
        let src = result_files.get(&def_file).cloned().unwrap_or_default();
        let refreshed = syn::parse_file(&src).map_err(|e| format!("pre-sig {def_file}: {e}"))?;
        let line_starts_now = line_starts(&src);
        let item_fn_now = locate_item_fn(&refreshed, function).ok_or_else(|| {
            format!("change_signature internal: could not relocate fn `{function}` in `{def_file}`")
        })?;
        let (open_now, close_now) = signature_paren_span(&item_fn_now, &src, &line_starts_now)?;
        let mut out = src.clone();
        out.replace_range(open_now..close_now, &new_params_src);
        result_files.insert(def_file.clone(), out);
        *per_file_count.entry(def_file.clone()).or_insert(0) += 1;
    }

    // 10c. Body renames. For each ParamReorder with `rename = Some`,
    //      replace every use of the *original* name in the fn body
    //      with the new name. We refuse to proceed when shadowing
    //      would change semantics (a `let old_name = ...` or an
    //      inner closure parameter named `old_name` rebinds the
    //      symbol mid-body).
    let renames: Vec<(String, String)> = new_params
        .iter()
        .filter_map(|p| {
            p.rename
                .clone()
                .map(|new| (original_params[p.from_index].name.clone(), new))
        })
        .filter(|(old, new)| old != new)
        .collect();
    if !renames.is_empty() {
        let src = result_files.get(&def_file).cloned().unwrap_or_default();
        let refreshed = syn::parse_file(&src).map_err(|e| format!("pre-body {def_file}: {e}"))?;
        let item_fn_now = locate_item_fn(&refreshed, function).ok_or_else(|| {
            format!(
                "change_signature internal: could not relocate fn `{function}` after sig rewrite"
            )
        })?;
        let line_starts_now = line_starts(&src);
        // Collect rename edits (descending by byte offset for safe
        // splicing) across all renames in one pass. Tuples carry an
        // owned `String` for the replacement so we don't need to
        // juggle borrow lifetimes across the multi-rename loop.
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        for (old_name, new_name) in &renames {
            let mut walker = BodyRenameWalker {
                old_name,
                new_name,
                source: &src,
                line_starts: &line_starts_now,
                edits: &mut edits,
                error: None,
            };
            walker.visit_block(&item_fn_now.block);
            if let Some(err) = walker.error.take() {
                return Err(format!(
                    "change_signature `{function}`: rename `{old_name}` -> `{new_name}` refused: {err}"
                ));
            }
        }
        edits.sort_by_key(|(s, _, _)| std::cmp::Reverse(*s));
        let mut out = src.clone();
        for (s, e, new) in &edits {
            out.replace_range(*s..*e, new);
        }
        let n = edits.len();
        result_files.insert(def_file.clone(), out);
        *per_file_count.entry(def_file.clone()).or_insert(0) += n;
    }

    // 11. Post-parse every changed file. Rejection is the safety net
    //     for any rewrite that would not be valid Rust.
    for path in &in_scope {
        let new_src = result_files.get(path).cloned().unwrap_or_default();
        let old_src = files.get(path).cloned().unwrap_or_default();
        if new_src == old_src {
            continue;
        }
        syn::parse_file(&new_src).map_err(|e| {
            format!("post-parse {path}: change_signature would produce invalid Rust: {e}")
        })?;
    }

    Ok((result_files, per_file_count))
}

/// Source-text view of one parameter in the *original* signature.
struct OriginalParam {
    /// The bound name, e.g. `a` in `a: i32`. Refusing patterns more
    /// complex than `Pat::Ident` upstream lets us rely on a single
    /// ident here.
    name: String,
    /// Verbatim source text of the whole `pat: type` slice
    /// (including any leading attributes — none in practice in the
    /// shapes we accept).
    text: String,
}

fn collect_original_params(
    item: &syn::ItemFn,
    src: &str,
    line_starts_v: &[usize],
) -> Result<Vec<OriginalParam>, String> {
    use syn::spanned::Spanned;
    let mut out = Vec::with_capacity(item.sig.inputs.len());
    for (i, arg) in item.sig.inputs.iter().enumerate() {
        let FnArg::Typed(pat) = arg else {
            return Err(format!(
                "change_signature: param[{i}] is a receiver (`self`); refuse"
            ));
        };
        let name = match pat.pat.as_ref() {
            syn::Pat::Ident(pi) => {
                if pi.by_ref.is_some() {
                    return Err(format!(
                        "change_signature: param[{i}] uses `ref` binding; refuse"
                    ));
                }
                if pi.subpat.is_some() {
                    return Err(format!(
                        "change_signature: param[{i}] has a sub-pattern; refuse"
                    ));
                }
                pi.ident.to_string()
            }
            _ => {
                return Err(format!(
                    "change_signature: param[{i}] is not a simple `ident: Type` pattern; refuse"
                ));
            }
        };
        let s_loc = arg.span().start();
        let e_loc = arg.span().end();
        let s = linecol_to_byte(line_starts_v, src, s_loc.line, s_loc.column).ok_or_else(|| {
            format!("change_signature internal: could not resolve param[{i}] start byte")
        })?;
        let e = linecol_to_byte(line_starts_v, src, e_loc.line, e_loc.column).ok_or_else(|| {
            format!("change_signature internal: could not resolve param[{i}] end byte")
        })?;
        out.push(OriginalParam {
            name,
            text: src[s..e].to_string(),
        });
    }
    Ok(out)
}

fn signature_paren_span(
    item: &syn::ItemFn,
    src: &str,
    line_starts_v: &[usize],
) -> Result<(usize, usize), String> {
    let open_loc = item.sig.paren_token.span.open().end();
    let close_loc = item.sig.paren_token.span.close().start();
    let open =
        linecol_to_byte(line_starts_v, src, open_loc.line, open_loc.column).ok_or_else(|| {
            "change_signature internal: could not resolve sig paren-open byte".to_string()
        })?;
    let close =
        linecol_to_byte(line_starts_v, src, close_loc.line, close_loc.column).ok_or_else(|| {
            "change_signature internal: could not resolve sig paren-close byte".to_string()
        })?;
    Ok((open, close))
}

fn locate_item_fn(file: &syn::File, name: &str) -> Option<syn::ItemFn> {
    for item in &file.items {
        if let Item::Fn(f) = item {
            if f.sig.ident == name {
                return Some(f.clone());
            }
        }
        if let Item::Mod(m) = item {
            if let Some(f) = locate_item_fn_in_mod(m, name) {
                return Some(f);
            }
        }
    }
    None
}

fn locate_item_fn_in_mod(m: &syn::ItemMod, name: &str) -> Option<syn::ItemFn> {
    if let Some((_, items)) = &m.content {
        for item in items {
            if let Item::Fn(f) = item {
                if f.sig.ident == name {
                    return Some(f.clone());
                }
            }
            if let Item::Mod(inner) = item {
                if let Some(f) = locate_item_fn_in_mod(inner, name) {
                    return Some(f);
                }
            }
        }
    }
    None
}

/// Visit a function body and collect rename edits for every plain
/// `Path::Ident == old_name`. Refuses to proceed when:
///   - The body declares a `let old_name = ...` or any other binding
///     that shadows the parameter — the rename would change which
///     occurrence binds to which value.
///   - A binding for `new_name` already exists in the body — the
///     rewrite would produce a name collision.
struct BodyRenameWalker<'a> {
    old_name: &'a str,
    new_name: &'a str,
    source: &'a str,
    line_starts: &'a [usize],
    edits: &'a mut Vec<(usize, usize, String)>,
    error: Option<String>,
}

impl<'a, 'ast> Visit<'ast> for BodyRenameWalker<'a> {
    fn visit_pat_ident(&mut self, pi: &'ast syn::PatIdent) {
        if self.error.is_some() {
            return;
        }
        if pi.ident == self.old_name {
            self.error = Some(format!(
                "binding `{}` shadows the parameter inside the body; the rename would \
                 silently change which occurrence binds to which value",
                self.old_name
            ));
            return;
        }
        if pi.ident == self.new_name {
            self.error = Some(format!(
                "binding `{}` already exists in the body; the rename would create a \
                 name collision",
                self.new_name
            ));
            return;
        }
        syn::visit::visit_pat_ident(self, pi);
    }

    fn visit_expr_path(&mut self, ep: &'ast syn::ExprPath) {
        if self.error.is_some() {
            return;
        }
        // Bare-ident references only — `mod::old_name` keeps its
        // segment unchanged, and we already refused qualified-path
        // call sites for the function name itself upstream.
        if ep.qself.is_none() && ep.path.leading_colon.is_none() && ep.path.segments.len() == 1 {
            let seg = ep.path.segments.first().unwrap();
            if seg.arguments.is_none() && seg.ident == self.old_name {
                let s_loc = seg.ident.span().start();
                let e_loc = seg.ident.span().end();
                if let (Some(s), Some(e)) = (
                    linecol_to_byte(self.line_starts, self.source, s_loc.line, s_loc.column),
                    linecol_to_byte(self.line_starts, self.source, e_loc.line, e_loc.column),
                ) {
                    self.edits.push((s, e, self.new_name.to_string()));
                }
            }
        }
        syn::visit::visit_expr_path(self, ep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(path: &str, src: &str) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert(path.into(), src.into());
        m
    }

    fn keep(idx: usize) -> ParamReorder {
        ParamReorder {
            from_index: idx,
            rename: None,
        }
    }
    fn keep_rename(idx: usize, new: &str) -> ParamReorder {
        ParamReorder {
            from_index: idx,
            rename: Some(new.to_string()),
        }
    }

    #[test]
    fn reorders_two_params_and_propagates_to_call_sites() {
        let src = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
                   pub fn caller() -> i32 { add(1, 2) + add(3, 4) }\n";
        let files = one("src/lib.rs", src);
        let (out, per_file) = change_signature(&files, "add", &[keep(1), keep(0)], &[]).unwrap();
        let new_src = &out["src/lib.rs"];
        assert!(
            new_src.contains("fn add(b: i32, a: i32)"),
            "signature must be swapped:\n{new_src}"
        );
        assert!(
            new_src.contains("add(2, 1)") && new_src.contains("add(4, 3)"),
            "every call site must be reordered:\n{new_src}"
        );
        // 2 call sites + 1 signature = 3 byte-level edits in the file.
        assert_eq!(per_file["src/lib.rs"], 3);
        syn::parse_file(new_src).expect("rewritten source must parse");
    }

    #[test]
    fn renames_a_param_and_rewrites_body_references() {
        let src = "pub fn double(x: i32) -> i32 { x + x }\n\
                   pub fn caller() -> i32 { double(7) }\n";
        let files = one("src/lib.rs", src);
        let (out, _) = change_signature(&files, "double", &[keep_rename(0, "n")], &[]).unwrap();
        let new_src = &out["src/lib.rs"];
        assert!(
            new_src.contains("fn double(n: i32)"),
            "signature must be renamed:\n{new_src}"
        );
        assert!(
            new_src.contains("n + n"),
            "body uses must be renamed:\n{new_src}"
        );
        // Call sites pass positional args, so they don't see the param
        // name change.
        assert!(new_src.contains("double(7)"));
        syn::parse_file(new_src).expect("rewritten source must parse");
    }

    #[test]
    fn refuses_when_new_params_changes_arity() {
        let src = "fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "add", &[keep(0)], &[]).unwrap_err();
        assert!(err.contains("permutation-only"), "got: {err}");
    }

    #[test]
    fn refuses_duplicate_from_index() {
        let src = "fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "add", &[keep(0), keep(0)], &[]).unwrap_err();
        assert!(err.contains("listed twice"), "got: {err}");
    }

    #[test]
    fn refuses_out_of_range_index() {
        let src = "fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "add", &[keep(0), keep(5)], &[]).unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn refuses_rename_to_an_existing_param_name() {
        // Renaming `a` -> `b` while `b` is also kept would produce
        // `fn f(b: i32, b: i32)` — invalid Rust. The transform must
        // catch this before splicing.
        let src = "fn f(a: i32, b: i32) -> i32 { a + b }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "f", &[keep_rename(0, "b"), keep(1)], &[]).unwrap_err();
        assert!(
            err.contains("already taken") || err.contains("invalid"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_rename_when_body_shadows_old_name() {
        // The body has `let a = ...` — renaming `a` -> `n` would
        // change which `a`-occurrence binds to which value. Refuse.
        let src = "fn f(a: i32) -> i32 { let a = a + 1; a }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "f", &[keep_rename(0, "n")], &[]).unwrap_err();
        assert!(err.contains("shadows"), "got: {err}");
    }

    #[test]
    fn refuses_rename_when_new_name_exists_as_local() {
        // The body has `let n = ...` already, and we want to rename
        // `a` -> `n`. Refuse: collision.
        let src = "fn f(a: i32) -> i32 { let n = 1; a + n }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "f", &[keep_rename(0, "n")], &[]).unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
    }

    #[test]
    fn refuses_generic_function() {
        let src = "fn id<T>(x: T) -> T { x }\nfn use_() -> i32 { id(3) }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "id", &[keep(0)], &[]).unwrap_err();
        assert!(err.contains("generic"), "got: {err}");
    }

    #[test]
    fn refuses_async_function() {
        let src = "async fn a(x: i32) -> i32 { x }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "a", &[keep(0)], &[]).unwrap_err();
        assert!(err.contains("async"), "got: {err}");
    }

    #[test]
    fn refuses_method_collision() {
        let src = "struct S;\n\
                   impl S { fn add(&self, a: i32, b: i32) -> i32 { a + b } }\n\
                   fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "add", &[keep(1), keep(0)], &[]).unwrap_err();
        assert!(err.contains("method"), "got: {err}");
    }

    #[test]
    fn refuses_macro_body_call_site() {
        let src = "fn add(a: i32, b: i32) -> i32 { a + b }\n\
                   fn go() { let _ = vec![add(1, 2)]; }\n";
        let files = one("src/lib.rs", src);
        let err = change_signature(&files, "add", &[keep(1), keep(0)], &[]).unwrap_err();
        assert!(err.contains("macro body"), "got: {err}");
    }

    #[test]
    fn refuses_qualified_path_call_site() {
        let mut files = BTreeMap::new();
        files.insert(
            "src/lib.rs".into(),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n".into(),
        );
        files.insert(
            "src/main.rs".into(),
            "fn main() { let _ = crate::add(1, 2); }\n".into(),
        );
        let err = change_signature(&files, "add", &[keep(1), keep(0)], &[]).unwrap_err();
        assert!(
            err.contains("non-bare references") || err.contains("Refuse"),
            "got: {err}"
        );
    }

    #[test]
    fn no_op_when_function_not_found() {
        let src = "fn keep() {}\n";
        let files = one("src/lib.rs", src);
        let (out, per_file) = change_signature(&files, "missing", &[keep(0)], &[]).unwrap();
        assert_eq!(out, files);
        assert!(per_file.is_empty());
    }

    #[test]
    fn refuses_self_taking_method_target() {
        // Direct hit on the impl-block fn isn't free-standing; we
        // only ever locate `Item::Fn`, so the method is never picked
        // up. We assert the no-op path here so the contract is
        // explicit.
        let src = "struct S;\n\
                   impl S { fn m(&self, x: i32) -> i32 { x } }\n";
        let files = one("src/lib.rs", src);
        let (out, per_file) = change_signature(&files, "m", &[keep(0)], &[]).unwrap();
        assert_eq!(out, files);
        assert!(per_file.is_empty());
    }

    #[test]
    fn three_param_permutation_preserves_argument_alignment() {
        let src = "fn f(a: i32, b: i32, c: i32) -> i32 { a + b + c }\n\
                   fn caller() -> i32 { f(10, 20, 30) }\n";
        let files = one("src/lib.rs", src);
        // new order = [c, a, b] -> from_index sequence [2, 0, 1].
        let (out, _) = change_signature(&files, "f", &[keep(2), keep(0), keep(1)], &[]).unwrap();
        let new_src = &out["src/lib.rs"];
        assert!(
            new_src.contains("fn f(c: i32, a: i32, b: i32)"),
            "signature: {new_src}"
        );
        assert!(
            new_src.contains("f(30, 10, 20)"),
            "call site must follow the permutation: {new_src}"
        );
    }
}

fn validate_fn_shape(item: &syn::ItemFn, name: &str) -> Result<(), String> {
    if item.sig.asyncness.is_some() {
        return Err(format!(
            "change_signature `{name}`: refusing `async` function"
        ));
    }
    if item.sig.constness.is_some() {
        return Err(format!(
            "change_signature `{name}`: refusing `const` function"
        ));
    }
    if item.sig.unsafety.is_some() {
        return Err(format!(
            "change_signature `{name}`: refusing `unsafe` function"
        ));
    }
    if !item.sig.generics.params.is_empty() {
        return Err(format!(
            "change_signature `{name}`: refusing generic function (type / lifetime / const params)"
        ));
    }
    if item.sig.generics.where_clause.is_some() {
        return Err(format!(
            "change_signature `{name}`: refusing function with where-clause"
        ));
    }
    if item.sig.variadic.is_some() {
        return Err(format!(
            "change_signature `{name}`: refusing variadic function"
        ));
    }
    for arg in &item.sig.inputs {
        if let FnArg::Receiver(_) = arg {
            return Err(format!(
                "change_signature `{name}`: refusing function with `self` receiver \
                 (it's a method, not a free function)"
            ));
        }
    }
    Ok(())
}

fn collect_mod_fns(
    m: &syn::ItemMod,
    target: &str,
    path: &str,
    acc: &mut Vec<(String, syn::ItemFn)>,
) {
    if let Some((_, items)) = &m.content {
        for item in items {
            if let Item::Fn(f) = item {
                if f.sig.ident == target {
                    acc.push((path.into(), f.clone()));
                }
            }
            if let Item::Mod(inner) = item {
                collect_mod_fns(inner, target, path, acc);
            }
        }
    }
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

/// Count every ident occurrence of `target` in `src`, including inside
/// (non-`macro_rules!`) macro bodies. Reuses `rust_rename` for
/// identical "what counts as an occurrence" semantics across ops.
fn count_ident_occurrences(src: &str, target: &str) -> Result<usize, String> {
    let (_, n) = crate::rust_rename::rename(src, target, target)?;
    Ok(n)
}

#[derive(Debug)]
struct CallSite {
    /// Byte offset of the start of the args list, exclusive of `(`.
    args_open: usize,
    /// Byte offset just before the closing `)`.
    args_close: usize,
    /// Source text of each argument (verbatim, with surrounding
    /// trivia trimmed by the caller).
    args: Vec<(usize, usize)>,
}

struct CallCollector<'a> {
    target: &'a str,
    call_sites: Vec<CallSite>,
    line_starts: &'a [usize],
    source: &'a str,
}

impl<'ast, 'a> Visit<'ast> for CallCollector<'a> {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        syn::visit::visit_expr_call(self, node);
        if let Expr::Path(path) = node.func.as_ref() {
            if path.qself.is_none()
                && path.path.leading_colon.is_none()
                && path.path.segments.len() == 1
            {
                let seg = path.path.segments.first().unwrap();
                if seg.ident == self.target && seg.arguments.is_none() {
                    let open_loc = node.paren_token.span.open().end();
                    let close_loc = node.paren_token.span.close().start();
                    let Some(args_open) = linecol_to_byte(
                        self.line_starts,
                        self.source,
                        open_loc.line,
                        open_loc.column,
                    ) else {
                        return;
                    };
                    let Some(args_close) = linecol_to_byte(
                        self.line_starts,
                        self.source,
                        close_loc.line,
                        close_loc.column,
                    ) else {
                        return;
                    };
                    let mut args: Vec<(usize, usize)> = Vec::new();
                    for a in &node.args {
                        let s_loc = syn::spanned::Spanned::span(a).start();
                        let e_loc = syn::spanned::Spanned::span(a).end();
                        let s = match linecol_to_byte(
                            self.line_starts,
                            self.source,
                            s_loc.line,
                            s_loc.column,
                        ) {
                            Some(s) => s,
                            None => return,
                        };
                        let e = match linecol_to_byte(
                            self.line_starts,
                            self.source,
                            e_loc.line,
                            e_loc.column,
                        ) {
                            Some(s) => s,
                            None => return,
                        };
                        args.push((s, e));
                    }
                    self.call_sites.push(CallSite {
                        args_open,
                        args_close,
                        args,
                    });
                }
            }
        }
    }
}

/// Detect whether a macro body anywhere in the visited subtree
/// contains `<target>(` — same shape as `inline::MacroScan`.
struct MacroScan<'a> {
    target: &'a str,
    hit: bool,
}

impl<'ast, 'a> Visit<'ast> for MacroScan<'a> {
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        syn::visit::visit_macro(self, m);
        if self.hit {
            return;
        }
        if m.path.is_ident("macro_rules") {
            return;
        }
        if token_stream_contains_call(m.tokens.clone(), self.target) {
            self.hit = true;
        }
    }
}

fn token_stream_contains_call(stream: TokenStream, target: &str) -> bool {
    let mut prev_ident: Option<String> = None;
    let mut prev_was_dollar = false;
    for tt in stream {
        match tt {
            TokenTree::Ident(i) => {
                prev_ident = if !prev_was_dollar {
                    Some(i.to_string())
                } else {
                    None
                };
                prev_was_dollar = false;
            }
            TokenTree::Group(g) => {
                if g.delimiter() == Delimiter::Parenthesis {
                    if let Some(name) = &prev_ident {
                        if name == target {
                            return true;
                        }
                    }
                }
                if token_stream_contains_call(g.stream(), target) {
                    return true;
                }
                prev_ident = None;
                prev_was_dollar = false;
            }
            TokenTree::Punct(p) => {
                prev_was_dollar = p.as_char() == '$' && p.spacing() == Spacing::Alone;
                prev_ident = None;
            }
            TokenTree::Literal(_) => {
                prev_ident = None;
                prev_was_dollar = false;
            }
        }
    }
    false
}
