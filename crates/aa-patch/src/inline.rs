//! `InlineFunction` — substitute each call site of a function with the
//! function's body wrapped in a block that binds every formal parameter to
//! its actual argument, then remove the function definition.
//!
//! Scope of Phase 1.21 (deliberately narrow):
//!
//! - The target function must be **free-standing** (not a method, not a
//!   trait impl, not nested inside another item).
//! - It must have exactly one definition across the files in scope.
//! - It must not be `async`, `const`, `unsafe`, generic, take `self`, or
//!   contain a `return` statement anywhere in its body (including
//!   closures — we don't look through lambda boundaries yet).
//! - It must not be recursive, directly or through a macro body.
//! - It must not be called from inside any macro body in the scope files
//!   (call-site substitution inside macro token streams is not yet
//!   supported; we refuse rather than produce half-inlined programs).
//!
//! The substitution shape, for a call `f(a1, a2)` inlining
//! `fn f(p1: T1, p2: T2) -> R { <body_inner> }`, is:
//!
//! ```text
//! { let p1 = a1; let p2 = a2; <body_inner> }
//! ```
//!
//! Wrapping the body in a block with `let` prelude gives three
//! properties for free:
//!
//! 1. **Single evaluation** of each argument — even when the argument
//!    is a side-effecting expression, it runs exactly once (as it would
//!    have if the function had been called).
//! 2. **No name capture** — a parameter `x` shadows any outer `x`
//!    cleanly through the `let` binding.
//! 3. **Expression position** — the result is a single `Expr::Block`,
//!    so the inlining is safe wherever the original call was (rvalue,
//!    argument, method receiver, …).
//!
//! The post-rewrite `syn::parse_file` call is the hard gate: an edit
//! that would produce invalid Rust is rejected and the source is
//! returned untouched.

use std::collections::BTreeMap;

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::visit::Visit;
use syn::{Expr, FnArg, ImplItem, Item, ReturnType, Visibility};

use crate::util::{line_starts, linecol_to_byte};

/// Rewritten file map plus per-file count of byte-level edits
/// (substituted call sites + 1 for the file that lost the definition).
pub type InlineResult = (BTreeMap<String, String>, BTreeMap<String, usize>);

/// Apply the inline-function transform across `files`. Returns the
/// rewritten file map and the per-file count of byte-level edits (call
/// sites substituted + 1 for the file that lost the definition).
///
/// Returns `Ok((files.clone(), {}))` when the target is not found in
/// scope — same convention as the per-file ops.
pub fn inline_function(
    files: &BTreeMap<String, String>,
    function: &str,
    file_filter: &[String],
) -> Result<InlineResult, String> {
    if !is_valid_ident(function) {
        return Err(format!("invalid Rust identifier `{function}`"));
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

    // 2. Locate the single fn definition. Parse every scope file once.
    let mut parsed: BTreeMap<String, syn::File> = BTreeMap::new();
    for path in &in_scope {
        let src = files.get(path).cloned().unwrap_or_default();
        let file = syn::parse_file(&src).map_err(|e| format!("pre-parse {path}: {e}"))?;
        parsed.insert(path.clone(), file);
    }

    let mut matches: Vec<(String, syn::ItemFn)> = Vec::new();
    for (path, file) in &parsed {
        for item in &file.items {
            if let Item::Fn(f) = item {
                if f.sig.ident == function {
                    matches.push((path.clone(), f.clone()));
                }
            }
            // Also catch free-standing fns nested inside `mod foo { ... }` at
            // file level — they're still free-standing, just with a path.
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
            "inline_function `{function}`: ambiguous — {} definitions found across scope; \
             narrow `files` to disambiguate",
            matches.len()
        ));
    }
    let (def_file, item_fn) = matches.into_iter().next().unwrap();

    // 3. Validate the fn shape.
    validate_fn_shape(&item_fn, function)?;

    // 4. Also forbid the fn appearing as a method (`impl X { fn <name> }`)
    //    anywhere in scope — that's a different symbol the LLM might not
    //    realise is distinct, so we refuse ambiguity.
    for file in parsed.values() {
        for item in &file.items {
            if let Item::Impl(imp) = item {
                for ii in &imp.items {
                    if let ImplItem::Fn(m) = ii {
                        if m.sig.ident == function {
                            return Err(format!(
                                "inline_function `{function}`: name collides with a \
                                 method in an `impl` block; refuse rather than \
                                 inline ambiguously"
                            ));
                        }
                    }
                }
            }
        }
    }

    // 5. Walk every scope file for macro bodies mentioning `function` as a
    //    call (`function(...)` token pair) — refuse if any hit. This is
    //    the "no half-inlined programs" gate.
    for (path, file) in &parsed {
        let mut scan = MacroScan {
            target: function,
            hit: false,
        };
        scan.visit_file(file);
        if scan.hit {
            return Err(format!(
                "inline_function `{function}`: appears inside a macro body in \
                 `{path}` (e.g. `{function}(...)` as a macro argument). Inlining \
                 across macro token streams is not supported — refuse rather \
                 than produce a half-inlined program."
            ));
        }
    }

    // 5.5. Refuse if any scope file references the target through something
    //      that isn't a bare `target(...)` call — qualified path calls
    //      (`crate::X(...)`, `mod::X(...)`), `use` statements, type aliases,
    //      function pointers, etc. The Phase-1.21 contract inlines *bare*
    //      calls only, and removing the definition would break any other
    //      reference. Refuse is safer than silent partial inlining.
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
                "inline_function `{function}`: `{path}` references the function \
                 {total} time(s) but only {expected} of those are bare call sites / \
                 definitions. Inlining would leave dangling references (e.g. a \
                 qualified-path call `mod::{function}(...)`, a `use` re-export, or \
                 a function-pointer use). Refuse rather than half-inline."
            ));
        }
    }

    // 6. Extract the body's inner bytes (between the braces, exclusive).
    let def_src = files.get(&def_file).cloned().unwrap_or_default();
    let def_line_starts = line_starts(&def_src);
    let body_brace = &item_fn.block.brace_token;
    let open_end = linecol_to_byte(
        &def_line_starts,
        &def_src,
        body_brace.span.open().end().line,
        body_brace.span.open().end().column,
    )
    .ok_or("inline_function: could not locate body opening brace")?;
    let close_start = linecol_to_byte(
        &def_line_starts,
        &def_src,
        body_brace.span.close().start().line,
        body_brace.span.close().start().column,
    )
    .ok_or("inline_function: could not locate body closing brace")?;
    if close_start < open_end {
        return Err("inline_function: body span inverted (syn regression?)".into());
    }
    let body_inner = def_src[open_end..close_start].to_string();

    // 7. Build the list of parameter names, in order. All must be simple
    //    `ident: Type` (typed pattern with Ident pattern) — `self`,
    //    destructuring, `_`, and `mut` are refused.
    let params = collect_param_names(&item_fn)?;

    // 8. For each scope file, collect ExprCall sites whose callee is a
    //    single-segment path equal to `function`. Substitute them
    //    descending by byte offset so earlier spans stay stable.
    let mut result_files = files.clone();
    let mut per_file_count: BTreeMap<String, usize> = BTreeMap::new();
    for path in &in_scope {
        let src = result_files.get(path).cloned().unwrap_or_default();
        let file = parsed.get(path).unwrap();
        let src_line_starts = line_starts(&src);
        let mut collector = CallCollector {
            target: function,
            call_sites: Vec::new(),
            line_starts: &src_line_starts,
            source: &src,
        };
        collector.visit_file(file);
        if collector.call_sites.is_empty() {
            continue;
        }
        let mut out = src.clone();
        // Descending replacement.
        let mut sites = collector.call_sites;
        sites.sort_by_key(|s| std::cmp::Reverse(s.start));
        for site in &sites {
            if site.args.len() != params.len() {
                return Err(format!(
                    "inline_function `{function}`: call site in `{path}` has {} \
                     argument(s) but fn takes {} parameter(s)",
                    site.args.len(),
                    params.len()
                ));
            }
            // Wrap the whole inlined block in parens so that the result
            // is unambiguously an expression even in statement position.
            // Without parens, `{ 42 } + 1` parses as two statements
            // (`{42};` + `+1`) per Rust's statement-disambiguation rule;
            // wrapping forces the binary-operator parse the caller
            // originally wrote.
            let mut replacement = String::from("({ ");
            for (pname, arg_src) in params.iter().zip(&site.args) {
                replacement.push_str("let ");
                replacement.push_str(pname);
                replacement.push_str(" = ");
                replacement.push_str(arg_src.trim());
                replacement.push_str("; ");
            }
            replacement.push_str(body_inner.trim());
            replacement.push_str(" })");
            out.replace_range(site.start..site.end, &replacement);
        }
        *per_file_count.entry(path.clone()).or_insert(0) += sites.len();
        result_files.insert(path.clone(), out);
    }

    // 9. Finally remove the fn definition from its file.
    {
        let src = result_files.get(&def_file).cloned().unwrap_or_default();
        // We need to re-find the fn in the (possibly already-rewritten) source
        // in case a call site sat above the definition. The fn is free-standing
        // and its ident is unique among top-level items by validation; a fresh
        // parse lets us locate the current byte span robustly.
        let refreshed =
            syn::parse_file(&src).map_err(|e| format!("pre-remove re-parse {def_file}: {e}"))?;
        let (start, end) = locate_item_fn_span(&refreshed, function, &src).ok_or_else(|| {
            "inline_function: could not relocate definition after \
                            call-site rewrite"
                .to_string()
        })?;
        // Extend `end` forward past trailing whitespace up to and including
        // one newline, so the file does not keep an orphan blank line.
        let bytes = src.as_bytes();
        let mut real_end = end;
        while real_end < bytes.len() && (bytes[real_end] == b' ' || bytes[real_end] == b'\t') {
            real_end += 1;
        }
        if real_end < bytes.len() && bytes[real_end] == b'\n' {
            real_end += 1;
        }
        let mut out = src.clone();
        out.replace_range(start..real_end, "");
        *per_file_count.entry(def_file.clone()).or_insert(0) += 1;
        result_files.insert(def_file.clone(), out);
    }

    // 10. Post-parse every changed file. Reject the whole op on failure.
    for path in &in_scope {
        let new_src = result_files.get(path).cloned().unwrap_or_default();
        let old_src = files.get(path).cloned().unwrap_or_default();
        if new_src == old_src {
            continue;
        }
        syn::parse_file(&new_src)
            .map_err(|e| format!("post-parse {path}: inline would produce invalid Rust: {e}"))?;
    }

    Ok((result_files, per_file_count))
}

fn validate_fn_shape(item: &syn::ItemFn, name: &str) -> Result<(), String> {
    if item.sig.asyncness.is_some() {
        return Err(format!(
            "inline_function `{name}`: refusing `async` function"
        ));
    }
    if item.sig.constness.is_some() {
        return Err(format!(
            "inline_function `{name}`: refusing `const` function"
        ));
    }
    if item.sig.unsafety.is_some() {
        return Err(format!(
            "inline_function `{name}`: refusing `unsafe` function"
        ));
    }
    if !item.sig.generics.params.is_empty() {
        return Err(format!(
            "inline_function `{name}`: refusing generic function (type / lifetime / const params)"
        ));
    }
    if item.sig.generics.where_clause.is_some() {
        return Err(format!(
            "inline_function `{name}`: refusing function with where-clause"
        ));
    }
    if let Some(variadic) = &item.sig.variadic {
        let _ = variadic;
        return Err(format!(
            "inline_function `{name}`: refusing variadic function"
        ));
    }
    // Recursion and `return` inside the body.
    let mut guard = BodyGuard {
        target: name,
        has_return: false,
        has_self_ref: false,
    };
    guard.visit_block(&item.block);
    if guard.has_return {
        return Err(format!(
            "inline_function `{name}`: body contains a `return` expression; \
             inlining would change its meaning (the `return` would exit the caller)"
        ));
    }
    if guard.has_self_ref {
        return Err(format!(
            "inline_function `{name}`: function is recursive (body calls `{name}`)"
        ));
    }
    // Also scan macro bodies inside the function for recursion.
    let mut mscan = MacroScan {
        target: name,
        hit: false,
    };
    mscan.visit_block(&item.block);
    if mscan.hit {
        return Err(format!(
            "inline_function `{name}`: function body calls `{name}` through a macro; \
             refuse rather than inline recursively"
        ));
    }
    let _ = &item.vis; // any visibility is fine; it's dropped with the def
    let _ = item.sig.output.clone(); // return type is irrelevant to inlining
    match item.sig.output {
        ReturnType::Default | ReturnType::Type(..) => {}
    }
    Ok(())
}

fn collect_param_names(item: &syn::ItemFn) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for (i, arg) in item.sig.inputs.iter().enumerate() {
        match arg {
            FnArg::Receiver(_) => {
                return Err(format!(
                    "inline_function `{}`: refusing function with `self` receiver \
                     (it's a method, not a free function)",
                    item.sig.ident
                ));
            }
            FnArg::Typed(pat) => {
                let name = match pat.pat.as_ref() {
                    syn::Pat::Ident(pi) => {
                        if pi.by_ref.is_some() {
                            return Err(format!(
                                "inline_function `{}`: param[{i}] uses `ref` binding — \
                                 refuse to reason about borrow semantics",
                                item.sig.ident
                            ));
                        }
                        if pi.subpat.is_some() {
                            return Err(format!(
                                "inline_function `{}`: param[{i}] has a sub-pattern — \
                                 only simple `ident: Type` patterns are supported",
                                item.sig.ident
                            ));
                        }
                        pi.ident.to_string()
                    }
                    other => {
                        let _ = other;
                        return Err(format!(
                            "inline_function `{}`: param[{i}] is not a simple ident \
                             pattern — refuse to inline (destructuring params unsupported)",
                            item.sig.ident
                        ));
                    }
                };
                out.push(name);
            }
        }
    }
    Ok(out)
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

fn locate_item_fn_span(file: &syn::File, name: &str, src: &str) -> Option<(usize, usize)> {
    let line_starts = line_starts(src);
    for item in &file.items {
        if let Item::Fn(f) = item {
            if f.sig.ident == name {
                return item_fn_byte_span(f, &line_starts, src);
            }
        }
        if let Item::Mod(m) = item {
            if let Some(sp) = locate_item_fn_in_mod(m, name, &line_starts, src) {
                return Some(sp);
            }
        }
    }
    None
}

fn locate_item_fn_in_mod(
    m: &syn::ItemMod,
    name: &str,
    line_starts: &[usize],
    src: &str,
) -> Option<(usize, usize)> {
    if let Some((_, items)) = &m.content {
        for item in items {
            if let Item::Fn(f) = item {
                if f.sig.ident == name {
                    return item_fn_byte_span(f, line_starts, src);
                }
            }
            if let Item::Mod(inner) = item {
                if let Some(sp) = locate_item_fn_in_mod(inner, name, line_starts, src) {
                    return Some(sp);
                }
            }
        }
    }
    None
}

fn item_fn_byte_span(f: &syn::ItemFn, line_starts: &[usize], src: &str) -> Option<(usize, usize)> {
    // Start = first attribute's `#` if any; otherwise visibility token;
    // otherwise the `fn` keyword. Back up to the start of the line if
    // that line is nothing but indentation + the start token, so the
    // indentation disappears with the definition.
    let start_span = if let Some(a) = f.attrs.first() {
        a.pound_token.span
    } else {
        match &f.vis {
            Visibility::Public(p) => p.span,
            Visibility::Restricted(r) => r.pub_token.span,
            Visibility::Inherited => f.sig.fn_token.span,
        }
    };
    let start_loc = start_span.start();
    let start = linecol_to_byte(line_starts, src, start_loc.line, start_loc.column)?;
    // Walk backward through spaces/tabs on the same line.
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

    let end_loc = f.block.brace_token.span.close().end();
    let end = linecol_to_byte(line_starts, src, end_loc.line, end_loc.column)?;
    Some((real_start, end))
}

#[derive(Debug)]
struct CallSite {
    start: usize,
    end: usize,
    args: Vec<String>,
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
                    let call_span = node;
                    // Span the whole call expression from the callee's
                    // start to past the closing `)`.
                    let start_loc = seg.ident.span().start();
                    let end_loc = node.paren_token.span.close().end();
                    let start = match linecol_to_byte(
                        self.line_starts,
                        self.source,
                        start_loc.line,
                        start_loc.column,
                    ) {
                        Some(s) => s,
                        None => return,
                    };
                    let end = match linecol_to_byte(
                        self.line_starts,
                        self.source,
                        end_loc.line,
                        end_loc.column,
                    ) {
                        Some(s) => s,
                        None => return,
                    };
                    let mut args = Vec::new();
                    for a in &call_span.args {
                        let sp = a;
                        // Extract each argument's source text via its span.
                        let s_loc = syn::spanned::Spanned::span(sp).start();
                        let e_loc = syn::spanned::Spanned::span(sp).end();
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
                        if e <= s || e > self.source.len() {
                            return;
                        }
                        args.push(self.source[s..e].to_string());
                    }
                    self.call_sites.push(CallSite { start, end, args });
                }
            }
        }
    }
}

struct BodyGuard<'a> {
    target: &'a str,
    has_return: bool,
    has_self_ref: bool,
}

impl<'ast, 'a> Visit<'ast> for BodyGuard<'a> {
    fn visit_expr_return(&mut self, _: &'ast syn::ExprReturn) {
        self.has_return = true;
    }
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        syn::visit::visit_expr_call(self, node);
        if let Expr::Path(p) = node.func.as_ref() {
            if p.qself.is_none() && p.path.leading_colon.is_none() && p.path.segments.len() == 1 {
                let seg = p.path.segments.first().unwrap();
                if seg.ident == self.target {
                    self.has_self_ref = true;
                }
            }
        }
    }
}

/// Detect whether a macro body anywhere in the visited subtree contains
/// `<target>(` — the giveaway shape for a call. We walk every macro's
/// token stream with a simple two-token sliding window (Ident followed
/// by Paren-delimited Group). `macro_rules!` definitions are skipped
/// so we don't misfire on their meta-var grammar.
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
                if !prev_was_dollar {
                    prev_ident = Some(i.to_string());
                } else {
                    prev_ident = None;
                }
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
                // Reset adjacency at group boundaries, but still descend
                // into the group's inner stream in case of nesting.
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
/// (non-`macro_rules!`) macro bodies. Implemented by piggy-backing on
/// `rust_rename::rename(src, target, target)`, which is a no-op on the
/// text but returns the number of ident matches it would have rewritten.
/// Using the rename pipeline here keeps the "what counts as an occurrence"
/// semantics identical between ops.
fn count_ident_occurrences(src: &str, target: &str) -> Result<usize, String> {
    let (_, n) = crate::rust_rename::rename(src, target, target)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(path: &str, src: &str) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert(path.into(), src.into());
        m
    }

    #[test]
    fn inlines_simple_one_arg_fn_and_removes_definition() {
        let src = "pub fn sq(x: i32) -> i32 { x * x }\n\
                   pub fn main_like() -> i32 { sq(3) + sq(4) }\n";
        let files = one("src/lib.rs", src);
        let (out, per_file) = inline_function(&files, "sq", &[]).unwrap();
        let new_src = &out["src/lib.rs"];
        assert!(
            !new_src.contains("pub fn sq"),
            "definition must be removed:\n{new_src}"
        );
        assert!(
            new_src.contains("({ let x = 3; x * x })"),
            "first call site should be substituted:\n{new_src}"
        );
        assert!(
            new_src.contains("({ let x = 4; x * x })"),
            "second call site should be substituted:\n{new_src}"
        );
        // 2 call sites + 1 definition removal = 3 edits on the single file.
        assert_eq!(per_file["src/lib.rs"], 3);
        syn::parse_file(new_src).expect("rewritten source must parse");
    }

    #[test]
    fn inlines_zero_arg_fn() {
        let src = "fn magic() -> i32 { 42 }\nfn use_() -> i32 { magic() + 1 }\n";
        let files = one("src/lib.rs", src);
        let (out, _) = inline_function(&files, "magic", &[]).unwrap();
        let new_src = &out["src/lib.rs"];
        assert!(!new_src.contains("fn magic"));
        assert!(new_src.contains("({ 42 }) + 1"));
    }

    #[test]
    fn refuses_recursive_fn() {
        let src = "fn fact(n: i32) -> i32 { if n <= 0 { 1 } else { n * fact(n - 1) } }\n";
        let files = one("src/lib.rs", src);
        let err = inline_function(&files, "fact", &[]).unwrap_err();
        assert!(err.contains("recursive"), "got: {err}");
    }

    #[test]
    fn refuses_return_in_body() {
        let src = "fn f(x: i32) -> i32 { if x < 0 { return 0; } x + 1 }\n\
                   fn g() -> i32 { f(3) }\n";
        let files = one("src/lib.rs", src);
        let err = inline_function(&files, "f", &[]).unwrap_err();
        assert!(err.contains("return"), "got: {err}");
    }

    #[test]
    fn refuses_self_method_collision() {
        let src = "struct S;\n\
                   impl S { fn add(&self, x: i32) -> i32 { x } }\n\
                   fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let files = one("src/lib.rs", src);
        let err = inline_function(&files, "add", &[]).unwrap_err();
        assert!(err.contains("method"), "got: {err}");
    }

    #[test]
    fn refuses_when_called_inside_macro() {
        let src = "fn add(a: i32, b: i32) -> i32 { a + b }\n\
                   fn go() { let _ = vec![add(1, 2)]; }\n";
        let files = one("src/lib.rs", src);
        let err = inline_function(&files, "add", &[]).unwrap_err();
        assert!(err.contains("macro body"), "got: {err}");
    }

    #[test]
    fn refuses_generic_fn() {
        let src = "fn id<T>(x: T) -> T { x }\nfn use_() -> i32 { id(3) }\n";
        let files = one("src/lib.rs", src);
        let err = inline_function(&files, "id", &[]).unwrap_err();
        assert!(err.contains("generic"), "got: {err}");
    }

    #[test]
    fn refuses_async_fn() {
        let src = "async fn a() -> i32 { 1 }\n";
        let files = one("src/lib.rs", src);
        let err = inline_function(&files, "a", &[]).unwrap_err();
        assert!(err.contains("async"), "got: {err}");
    }

    #[test]
    fn no_op_when_fn_not_found() {
        let src = "fn keep() {}\n";
        let files = one("src/lib.rs", src);
        let (out, per_file) = inline_function(&files, "does_not_exist", &[]).unwrap();
        assert_eq!(out, files);
        assert!(per_file.is_empty());
    }

    #[test]
    fn refuses_when_multiple_definitions_in_scope() {
        let mut files = BTreeMap::new();
        files.insert("src/a.rs".into(), "fn dup() -> i32 { 1 }\n".into());
        files.insert("src/b.rs".into(), "fn dup() -> i32 { 2 }\n".into());
        let err = inline_function(&files, "dup", &[]).unwrap_err();
        assert!(err.contains("ambiguous"), "got: {err}");
    }

    #[test]
    fn refuses_when_qualified_path_call_would_dangle() {
        let mut files = BTreeMap::new();
        files.insert(
            "src/lib.rs".into(),
            "pub fn double(x: i32) -> i32 { x + x }\n".into(),
        );
        files.insert(
            "src/main.rs".into(),
            "fn main() { let _ = crate::double(7); }\n".into(),
        );
        // `crate::double(7)` is a 2-segment path — the bare-call matcher
        // skips it, so silently removing the definition would break
        // `main`. The op refuses.
        let err = inline_function(&files, "double", &[]).unwrap_err();
        assert!(err.contains("dangling"), "got: {err}");
    }

    #[test]
    fn bare_call_in_sibling_file_is_inlined() {
        let mut files = BTreeMap::new();
        files.insert(
            "src/lib.rs".into(),
            "pub fn inc(x: i32) -> i32 { x + 1 }\n".into(),
        );
        // No `use crate::inc;` — that counts as a non-bare reference and
        // the guard (rightly) refuses to leave it dangling. The
        // cross-file test here exercises the planner's ability to
        // substitute a bare call site in one file while removing the
        // definition from another; a separate `use` statement is a
        // different symbol-resolution concern that `InlineFunction`
        // deliberately doesn't touch in Phase 1.21.
        files.insert("src/other.rs".into(), "fn run() -> i32 { inc(5) }\n".into());
        let (out, per_file) = inline_function(&files, "inc", &[]).unwrap();
        assert!(!out["src/lib.rs"].contains("pub fn inc"));
        assert!(out["src/other.rs"].contains("({ let x = 5; x + 1 })"));
        assert_eq!(per_file["src/other.rs"], 1);
        assert_eq!(per_file["src/lib.rs"], 1);
    }
}
