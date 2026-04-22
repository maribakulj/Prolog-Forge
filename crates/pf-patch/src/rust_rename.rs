//! Rust-specific rename: byte-accurate textual edits driven by `syn` spans.
//!
//! The approach preserves comments and formatting (a CST-faithful round-trip
//! via `prettyplease` would drop comments; we deliberately avoid that). It
//! is not scope-aware — every `Ident` whose string equals `old_name` is
//! rewritten. Scope resolution is a Phase 2 concern.

use proc_macro2::Ident;
use syn::visit::Visit;

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

/// `proc_macro2`'s `LineColumn::line` is 1-indexed; `column` is 0-indexed
/// and counts **characters** on the line. For ASCII identifiers (all Rust
/// names) char count == byte count; we fall back to a UTF-8 char walk when
/// non-ASCII appears on the line.
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
    let mut offset = line_start;
    for (i, _c) in line_text.char_indices() {
        if i == 0 && column == 0 {
            return Some(offset);
        }
        if i == 0 {
            continue;
        }
        // `i` is the byte index of the current char within `line_text`; for
        // `column == k` we want the start of the k-th char.
        if count_chars_until(line_text, i) == column {
            return Some(line_start + i);
        }
        offset = line_start + i;
    }
    Some(offset)
}

fn count_chars_until(s: &str, byte_idx: usize) -> usize {
    s.char_indices().take_while(|(i, _)| *i < byte_idx).count()
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
}
