//! `ExtractFunction` — Phase 1.22.
//!
//! Lift a contiguous run of statements out of a free-standing fn body
//! into a new free-standing helper, replacing the original site with a
//! call to that helper.
//!
//! See [`crate::ops::PatchOp::ExtractFunction`] for the full contract.
//! Implementation grows incrementally — the entry-point shape is fixed
//! so the wire layer ([`crate::plan::apply_op`]) and the validators
//! ([`crate::ops`], `aa_llm::propose_patch`) can be wired ahead of the
//! transform itself.

use std::collections::BTreeSet;

use syn::visit::Visit;
use syn::{FnArg, Item, ReturnType, Stmt, Visibility};

use crate::ops::ExtractParam;
use crate::util::{line_starts, linecol_to_byte};

/// Apply the extract-function transform to `source` in-place.
///
/// Returns `(new_source, n)` where `n` is the number of byte-level
/// edits performed (1 for the call-site replacement + 1 for the new
/// fn insertion = 2 on success, or 0 when the op is a no-op). Any
/// reason the transform refuses is returned as `Err(msg)` — the
/// planner forwards it to [`crate::plan::PreviewError`].
pub fn extract_function(
    source: &str,
    start_line: u32,
    end_line: u32,
    new_name: &str,
    params: &[ExtractParam],
) -> Result<(String, usize), String> {
    // 1. Cheap shape checks before we touch syn.
    if !is_valid_ident(new_name) {
        return Err(format!(
            "extract_function: `{new_name}` is not a valid Rust identifier"
        ));
    }
    if start_line == 0 || end_line == 0 {
        return Err("extract_function: line numbers are 1-indexed".into());
    }
    if end_line < start_line {
        return Err(format!(
            "extract_function: end_line {end_line} < start_line {start_line}"
        ));
    }
    for (i, p) in params.iter().enumerate() {
        if !is_valid_ident(&p.name) {
            return Err(format!(
                "extract_function: params[{i}] name `{}` is not a valid Rust identifier",
                p.name
            ));
        }
        if syn::parse_str::<syn::Type>(&p.ty).is_err() {
            return Err(format!(
                "extract_function: params[{i}] type `{}` does not parse as a Rust type",
                p.ty
            ));
        }
    }
    // Reject duplicate param names — a parameter list `(x: i32, x: u8)`
    // is not legal and would surface as a confusing post-parse error.
    {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (i, p) in params.iter().enumerate() {
            if !seen.insert(p.name.as_str()) {
                return Err(format!(
                    "extract_function: params[{i}] name `{}` is duplicated",
                    p.name
                ));
            }
        }
    }

    // 2. Pre-parse the source. Same posture as every other op — bail
    // early if the file is already broken so the user gets a real
    // syn diagnostic instead of a confusing "no enclosing fn" error.
    let file = syn::parse_file(source).map_err(|e| format!("extract_function: pre-parse: {e}"))?;
    let line_starts = line_starts(source);

    // 3. Locate the unique enclosing free-standing fn whose body
    //    brackets `start_line..=end_line`.
    let parent = find_enclosing_fn(&file, start_line, end_line).ok_or_else(|| {
        format!(
            "extract_function: lines {start_line}..={end_line} are not inside a free-standing \
                 fn body in this file"
        )
    })?;
    validate_parent_shape(&parent)?;

    // 4. Collect the contiguous run of stmts whose union of spans
    //    matches `start_line..=end_line` exactly. The selection must
    //    end on a non-tail statement (no `Stmt::Expr(_, None)` last).
    let selection = collect_selection(&parent, start_line, end_line)?;

    // 5. Walk the selection: refuse control-flow leaks + macro bodies.
    walk_selection_or_refuse(&selection)?;

    // 6. Each declared param must be mentioned at least once in the
    //    selection — otherwise it's dead weight in the new fn's
    //    signature, which usually means the caller misread the code.
    let selection_idents = collect_idents(&selection);
    for (i, p) in params.iter().enumerate() {
        if !selection_idents.contains(p.name.as_str()) {
            return Err(format!(
                "extract_function: params[{i}] name `{}` does not appear in the selected lines",
                p.name
            ));
        }
    }

    // 7. Compute byte spans for the selection (replace) and for the
    //    end of the parent fn (insert the helper after).
    let (sel_start, sel_end) = selection_byte_span(&selection, source, &line_starts)?;
    let parent_end = fn_close_brace_end(&parent, source, &line_starts)?;

    // 8. Render the call-site replacement and the new helper, then
    //    splice both into the source. We splice the *later* edit first
    //    (helper insertion) so the earlier byte offsets stay valid.
    let indent = leading_indent(source, sel_start);
    let arg_list = params
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let call_site = format!("{indent}{new_name}({arg_list});\n");
    let helper = render_helper(source, sel_start, sel_end, new_name, params);

    let mut out = source.to_string();
    out.insert_str(parent_end, &helper);
    // Replace the selection, including its trailing newline if any.
    let sel_end_with_nl = extend_to_newline(source, sel_end);
    out.replace_range(sel_start..sel_end_with_nl, &call_site);

    // 9. Hard gate: re-parse. Refuse the op if the rewrite isn't
    //    syntactically valid Rust.
    syn::parse_file(&out)
        .map_err(|e| format!("extract_function: post-parse: would produce invalid Rust: {e}"))?;

    // Two byte-level edits: call-site replace + helper insertion.
    Ok((out, 2))
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

/// Walk the file's top-level items and find the unique free-standing
/// `fn` whose body brackets the requested line range. Nested fns
/// inside `mod`s count too. Returns `None` if no candidate brackets
/// the range, or if more than one does (ambiguous; refuse rather than
/// guess).
fn find_enclosing_fn(file: &syn::File, start_line: u32, end_line: u32) -> Option<syn::ItemFn> {
    let mut hits: Vec<syn::ItemFn> = Vec::new();
    for item in &file.items {
        collect_brackets(item, start_line, end_line, &mut hits);
    }
    if hits.len() == 1 {
        Some(hits.into_iter().next().unwrap())
    } else {
        None
    }
}

fn collect_brackets(item: &Item, start: u32, end: u32, hits: &mut Vec<syn::ItemFn>) {
    match item {
        Item::Fn(f) => {
            let open = f.block.brace_token.span.open().start();
            let close = f.block.brace_token.span.close().end();
            // Selection must be strictly inside the braces (not on the
            // braces themselves, not outside the body).
            if open.line < start as usize && (end as usize) < close.line {
                hits.push(f.clone());
            }
        }
        Item::Mod(m) => {
            if let Some((_, items)) = &m.content {
                for inner in items {
                    collect_brackets(inner, start, end, hits);
                }
            }
        }
        _ => {}
    }
}

/// Same shape constraints as [`crate::inline`]'s validator: refuse
/// any signature feature whose semantics we can't safely preserve
/// across the extraction (no `self`, no generics, no `async`/`const`/
/// `unsafe`).
fn validate_parent_shape(item: &syn::ItemFn) -> Result<(), String> {
    let name = item.sig.ident.to_string();
    if item.sig.asyncness.is_some() {
        return Err(format!(
            "extract_function: refusing to extract from `async fn {name}`"
        ));
    }
    if item.sig.constness.is_some() {
        return Err(format!(
            "extract_function: refusing to extract from `const fn {name}`"
        ));
    }
    if item.sig.unsafety.is_some() {
        return Err(format!(
            "extract_function: refusing to extract from `unsafe fn {name}`"
        ));
    }
    if !item.sig.generics.params.is_empty() || item.sig.generics.where_clause.is_some() {
        return Err(format!(
            "extract_function: refusing to extract from generic fn `{name}`"
        ));
    }
    for arg in &item.sig.inputs {
        if let FnArg::Receiver(_) = arg {
            return Err(format!(
                "extract_function: refusing to extract from method `{name}` (takes `self`)"
            ));
        }
    }
    Ok(())
}

/// Collect the contiguous prefix/suffix of stmts from `parent.block`
/// whose first stmt starts on `start_line` *or later within the
/// same line range* and whose last stmt ends on `end_line` or
/// earlier. The selection must be non-empty, contiguous, and must
/// not include the function's tail expression (a `Stmt::Expr(_,
/// None)` in last position) — extracting a tail expression would
/// change the parent fn's return value, which is exactly the kind
/// of ambiguity Phase 1.x ops refuse.
fn collect_selection(
    parent: &syn::ItemFn,
    start_line: u32,
    end_line: u32,
) -> Result<Vec<Stmt>, String> {
    use syn::spanned::Spanned;
    let stmts = &parent.block.stmts;
    let mut selected: Vec<Stmt> = Vec::new();
    let mut last_stmt_was_tail_expr = false;
    let mut tail_index: Option<usize> = None;
    // Identify the trailing expression position (if any). A
    // trailing expression is a `Stmt::Expr(_, None)` that's the
    // very last stmt — anything else, including a `Stmt::Expr(_,
    // Some(_))` (semicolon-terminated), is a regular statement.
    if let Some(last) = stmts.last() {
        if matches!(last, Stmt::Expr(_, None)) {
            tail_index = Some(stmts.len() - 1);
        }
    }
    for (idx, stmt) in stmts.iter().enumerate() {
        let span = stmt.span();
        let s = span.start().line as u32;
        let e = span.end().line as u32;
        // Strict containment: every selected stmt must be entirely
        // within `[start_line, end_line]`. Stmts that cross the
        // boundary in either direction are refused — partial
        // selections are exactly the case that would produce silently
        // wrong code.
        if e < start_line || s > end_line {
            continue;
        }
        if s < start_line || e > end_line {
            return Err(format!(
                "extract_function: lines {start_line}..={end_line} cut a statement at line \
                 {s}..={e}; selection must align with whole statements"
            ));
        }
        if Some(idx) == tail_index {
            last_stmt_was_tail_expr = true;
        }
        selected.push(stmt.clone());
    }
    if selected.is_empty() {
        return Err(format!(
            "extract_function: no statements found in lines {start_line}..={end_line}"
        ));
    }
    if last_stmt_was_tail_expr {
        return Err(
            "extract_function: selection ends on the parent fn's tail expression; refusing \
             to change the parent's return value (extract one fewer line, or add `;`)"
                .into(),
        );
    }
    Ok(selected)
}

/// Refuse any control-flow construct that would leak out of the
/// extracted body once it lives in its own fn (`return`/`break`/
/// `continue` would break `?` would propagate from the wrong fn,
/// `await`/`yield` would change effect kind). Also refuse macro
/// invocations — we can't reason about identifiers inside their
/// token streams without descending into provider-specific grammar.
fn walk_selection_or_refuse(selection: &[Stmt]) -> Result<(), String> {
    let mut g = SelectionGuard {
        has_return: false,
        has_break: false,
        has_continue: false,
        has_try: false,
        has_await: false,
        has_yield: false,
        has_macro: false,
    };
    for s in selection {
        g.visit_stmt(s);
    }
    if g.has_return {
        return Err("extract_function: selection contains `return`; refuse to lift".into());
    }
    if g.has_break {
        return Err("extract_function: selection contains `break`; refuse to lift".into());
    }
    if g.has_continue {
        return Err("extract_function: selection contains `continue`; refuse to lift".into());
    }
    if g.has_try {
        return Err("extract_function: selection contains the `?` operator; refuse to lift".into());
    }
    if g.has_await {
        return Err("extract_function: selection contains `.await`; refuse to lift".into());
    }
    if g.has_yield {
        return Err("extract_function: selection contains `yield`; refuse to lift".into());
    }
    if g.has_macro {
        return Err(
            "extract_function: selection contains a macro invocation; the transform cannot \
             reason about token-stream bodies — refuse rather than miss a free identifier"
                .into(),
        );
    }
    Ok(())
}

struct SelectionGuard {
    has_return: bool,
    has_break: bool,
    has_continue: bool,
    has_try: bool,
    has_await: bool,
    has_yield: bool,
    has_macro: bool,
}

impl<'ast> Visit<'ast> for SelectionGuard {
    fn visit_expr_return(&mut self, _: &'ast syn::ExprReturn) {
        self.has_return = true;
    }
    fn visit_expr_break(&mut self, _: &'ast syn::ExprBreak) {
        self.has_break = true;
    }
    fn visit_expr_continue(&mut self, _: &'ast syn::ExprContinue) {
        self.has_continue = true;
    }
    fn visit_expr_try(&mut self, _: &'ast syn::ExprTry) {
        self.has_try = true;
    }
    fn visit_expr_await(&mut self, _: &'ast syn::ExprAwait) {
        self.has_await = true;
    }
    fn visit_expr_yield(&mut self, _: &'ast syn::ExprYield) {
        self.has_yield = true;
    }
    fn visit_macro(&mut self, _: &'ast syn::Macro) {
        self.has_macro = true;
    }
    // Don't descend into closures: a `return` *inside* a closure exits
    // the closure, not the enclosing fn, so it's safe to lift.
    fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
    // Same for nested fn items inside the selection (rare but legal).
    fn visit_item_fn(&mut self, _: &'ast syn::ItemFn) {}
}

/// Collect every plain `Ident` mentioned anywhere in the selection,
/// including inside macro bodies. Used to verify that each declared
/// `param.name` actually appears in the lifted code.
fn collect_idents(selection: &[Stmt]) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut v = IdentVisitor { out: &mut out };
    for s in selection {
        v.visit_stmt(s);
    }
    out
}

struct IdentVisitor<'a> {
    out: &'a mut BTreeSet<String>,
}

impl<'a, 'ast> Visit<'ast> for IdentVisitor<'a> {
    fn visit_ident(&mut self, i: &'ast proc_macro2::Ident) {
        self.out.insert(i.to_string());
    }
}

/// Byte span `[start, end)` covering every selected statement's
/// source. The selection is contiguous (validated upstream) so the
/// span is just the first stmt's start through the last stmt's end.
fn selection_byte_span(
    selection: &[Stmt],
    source: &str,
    line_starts: &[usize],
) -> Result<(usize, usize), String> {
    use syn::spanned::Spanned;
    let first = selection.first().expect("non-empty selection");
    let last = selection.last().expect("non-empty selection");
    let s = first.span().start();
    let e = last.span().end();
    // Walk back to the start of the line so we capture leading
    // indentation — the call-site replacement keeps that indentation.
    let raw_start = linecol_to_byte(line_starts, source, s.line, s.column)
        .ok_or_else(|| "extract_function: could not resolve selection start byte".to_string())?;
    let line_start = line_starts.get(s.line - 1).copied().unwrap_or(raw_start);
    let end = linecol_to_byte(line_starts, source, e.line, e.column)
        .ok_or_else(|| "extract_function: could not resolve selection end byte".to_string())?;
    Ok((line_start, end))
}

/// Byte offset just past the parent fn's closing `}` — where we
/// splice the new helper.
fn fn_close_brace_end(
    parent: &syn::ItemFn,
    source: &str,
    line_starts: &[usize],
) -> Result<usize, String> {
    let close = parent.block.brace_token.span.close().end();
    linecol_to_byte(line_starts, source, close.line, close.column)
        .ok_or_else(|| "extract_function: could not resolve parent fn close-brace byte".into())
}

/// Return the bytes of `source[..sel_start]` after the last `\n`,
/// trimmed to whitespace only — that's the indent we must reuse on
/// the call-site so the replacement aligns with neighbouring code.
fn leading_indent(source: &str, sel_start: usize) -> String {
    let bytes = source.as_bytes();
    let mut i = sel_start;
    while i > 0 {
        if bytes[i - 1] == b'\n' {
            break;
        }
        i -= 1;
    }
    let raw = &source[i..sel_start];
    let mut out = String::new();
    for c in raw.chars() {
        if c == ' ' || c == '\t' {
            out.push(c);
        } else {
            break;
        }
    }
    out
}

/// If `end` is followed by `\n` (possibly preceded by spaces), advance
/// past it so the call-site replacement consumes the trailing newline
/// of the selection — otherwise we'd leave a blank line behind.
fn extend_to_newline(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut e = end;
    while e < bytes.len() && (bytes[e] == b' ' || bytes[e] == b'\t') {
        e += 1;
    }
    if e < bytes.len() && bytes[e] == b'\n' {
        e += 1;
    }
    e
}

/// Render the new helper fn's source. Uses the *raw bytes* of the
/// selection as the body — preserves comments, formatting, and
/// macro-free Rust verbatim. Signature: `fn new_name(p: T, ...) {`
/// always returns `()` (Phase 1.22 narrow contract).
fn render_helper(
    source: &str,
    sel_start: usize,
    sel_end: usize,
    new_name: &str,
    params: &[ExtractParam],
) -> String {
    let body = &source[sel_start..sel_end];
    // The body bytes start at the line's leading indent because of
    // how `selection_byte_span` walks back to `\n`. Reuse that indent
    // as the helper's body indent for natural-looking output.
    let mut sig = format!("\nfn {new_name}(");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        sig.push_str(&p.name);
        sig.push_str(": ");
        sig.push_str(&p.ty);
    }
    sig.push_str(") {\n");
    let mut out = sig;
    out.push_str(body);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

// `ReturnType` and `Visibility` are imported for forward-compatible
// helpers (return-type inheritance + visibility propagation come in a
// later phase). Silence the unused-import lint until then.
#[allow(dead_code)]
fn _silence_unused_imports(_r: ReturnType, _v: Visibility) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, ty: &str) -> ExtractParam {
        ExtractParam {
            name: name.into(),
            ty: ty.into(),
        }
    }

    #[test]
    fn extracts_a_two_stmt_block_with_one_param() {
        let src = "fn parent(x: i32) {\n\
                   \x20   let a = x + 1;\n\
                   \x20   let b = a * 2;\n\
                   \x20   println!(\"{b}\");\n\
                   }\n";
        // Lines 2..=3 cover the two `let`s (we keep the println behind).
        let (out, n) = extract_function(src, 2, 3, "compute", &[p("x", "i32")]).unwrap();
        assert_eq!(n, 2);
        // The selection is replaced by a call to the new helper.
        assert!(out.contains("compute(x);"), "missing call site:\n{out}");
        // The new helper exists and carries the body bytes.
        assert!(
            out.contains("fn compute(x: i32) {"),
            "missing helper:\n{out}"
        );
        assert!(
            out.contains("let a = x + 1;") && out.contains("let b = a * 2;"),
            "helper body missing the lifted statements:\n{out}"
        );
        // The non-selected stmt stays in the parent.
        assert!(
            out.contains("println!(\"{b}\");"),
            "println dropped:\n{out}"
        );
        // And the rewrite must parse.
        syn::parse_file(&out).expect("rewritten source must parse");
    }

    #[test]
    fn refuses_when_param_not_used_in_selection() {
        let src = "fn parent(x: i32) {\n\
                   \x20   let a = 1;\n\
                   \x20   let b = a;\n\
                   }\n";
        let err = extract_function(src, 2, 3, "h", &[p("x", "i32")]).unwrap_err();
        assert!(err.contains("does not appear"), "got: {err}");
    }

    #[test]
    fn refuses_return_in_selection() {
        let src = "fn parent() -> i32 {\n\
                   \x20   let a = 1;\n\
                   \x20   if a < 0 { return 0; }\n\
                   \x20   a\n\
                   }\n";
        let err = extract_function(src, 2, 3, "h", &[]).unwrap_err();
        assert!(err.contains("return"), "got: {err}");
    }

    #[test]
    fn refuses_question_mark_in_selection() {
        let src = "fn parent() -> Result<i32, ()> {\n\
                   \x20   let a: Result<i32, ()> = Ok(1);\n\
                   \x20   let b = a?;\n\
                   \x20   Ok(b)\n\
                   }\n";
        let err = extract_function(src, 2, 3, "h", &[]).unwrap_err();
        assert!(
            err.contains("`?`") || err.contains("question"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_macro_in_selection() {
        let src = "fn parent() {\n\
                   \x20   println!(\"hi\");\n\
                   }\n";
        let err = extract_function(src, 2, 2, "h", &[]).unwrap_err();
        assert!(err.contains("macro"), "got: {err}");
    }

    #[test]
    fn refuses_when_selection_lands_on_tail_expression() {
        let src = "fn parent() -> i32 {\n\
                   \x20   let a = 1;\n\
                   \x20   a + 1\n\
                   }\n";
        // Line 3 is the parent's tail expression — refuse.
        let err = extract_function(src, 2, 3, "h", &[]).unwrap_err();
        assert!(err.contains("tail expression"), "got: {err}");
    }

    #[test]
    fn refuses_when_lines_cut_a_statement() {
        let src = "fn parent() {\n\
                   \x20   let v = vec![\n\
                   \x20       1, 2, 3,\n\
                   \x20   ];\n\
                   \x20   drop(v);\n\
                   }\n";
        // Lines 2..=2 only cover the opening of the let-stmt.
        let err = extract_function(src, 2, 2, "h", &[]).unwrap_err();
        assert!(err.contains("cut a statement"), "got: {err}");
    }

    #[test]
    fn refuses_invalid_param_type() {
        let src = "fn parent() {\n\
                   \x20   let a = 1;\n\
                   \x20   drop(a);\n\
                   }\n";
        let err = extract_function(src, 2, 3, "h", &[p("a", "not a type {{")]).unwrap_err();
        assert!(err.contains("parse"), "got: {err}");
    }

    #[test]
    fn refuses_self_taking_parent() {
        let src = "struct S;\n\
                   impl S {\n\
                   \x20   fn m(&self) {\n\
                   \x20       let a = 1;\n\
                   \x20       drop(a);\n\
                   \x20   }\n\
                   }\n";
        // Lines 4..=5 are inside `m`, but `m` takes `&self`.
        // Note: `m` is an impl-item fn, not a top-level Item::Fn, so
        // `find_enclosing_fn` returns None (we never descend into
        // `impl` blocks). The error message reflects that conservatively.
        let err = extract_function(src, 4, 5, "h", &[]).unwrap_err();
        assert!(err.contains("not inside a free-standing fn"), "got: {err}");
    }
}
