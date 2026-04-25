//! Shared helpers for span → byte-offset arithmetic used by every
//! syn-driven transform in this crate.

/// Per-line starting byte offsets, including a synthetic entry for the
/// final newline so lookups past the last line don't panic.
pub(crate) fn line_starts(src: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// `proc_macro2::LineColumn` is 1-indexed for lines and 0-indexed for
/// columns counted in characters. Return the byte offset that
/// corresponds to `(line, column)` in `src`, with a UTF-8 fallback for
/// non-ASCII lines.
pub(crate) fn linecol_to_byte(
    line_starts: &[usize],
    src: &str,
    line: usize,
    column: usize,
) -> Option<usize> {
    if line == 0 || line > line_starts.len() {
        return None;
    }
    let line_start = line_starts[line - 1];
    let line_end = line_starts.get(line).copied().unwrap_or(src.len());
    let line_text = &src[line_start..line_end];
    if line_text.is_ascii() {
        return Some(line_start + column);
    }
    for (i, _c) in line_text.char_indices() {
        let prior_chars = line_text[..i].chars().count();
        if prior_chars == column {
            return Some(line_start + i);
        }
    }
    Some(line_end)
}
