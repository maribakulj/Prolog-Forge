//! Plan → Preview orchestration.
//!
//! Inputs:
//!   - a `PatchPlan` (ordered ops),
//!   - a map `relative_path -> source_text` for the files in scope.
//!
//! Output:
//!   - a `PatchPreview` containing the modified content and a unified diff
//!     per changed file, plus aggregate stats.
//!
//! Pure function: no filesystem access, no graph mutation. The caller is
//! expected to load source texts and decide what to do with the preview
//! (display, apply, discard).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use crate::ops::{PatchOp, PatchPlan};
use crate::rust_rename;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePatch {
    pub path: String,
    pub before_len: usize,
    pub after_len: usize,
    pub replacements: usize,
    /// Unified diff in the familiar `--- before / +++ after` shape. Empty
    /// when the file ends up byte-identical to its input.
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PatchPreview {
    pub files: Vec<FilePatch>,
    pub total_replacements: usize,
    pub errors: Vec<PreviewError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewError {
    pub file: String,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("unsupported op for this planner build")]
    Unsupported,
}

pub fn preview(
    plan: &PatchPlan,
    files: &BTreeMap<String, String>,
) -> Result<PatchPreview, PatchError> {
    // Start from the input file set; each op produces a new set.
    let mut working: BTreeMap<String, String> = files.clone();
    let mut per_file_replacements: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors: Vec<PreviewError> = Vec::new();

    for op in &plan.ops {
        apply_op(op, &mut working, &mut per_file_replacements, &mut errors);
    }

    let mut result = PatchPreview::default();
    for (path, new_content) in &working {
        let original = files.get(path).cloned().unwrap_or_default();
        if *new_content == original {
            continue;
        }
        let diff = render_unified_diff(path, &original, new_content);
        let replacements = per_file_replacements.get(path).copied().unwrap_or(0);
        result.total_replacements += replacements;
        result.files.push(FilePatch {
            path: path.clone(),
            before_len: original.len(),
            after_len: new_content.len(),
            replacements,
            diff,
        });
    }
    result.errors = errors;
    Ok(result)
}

fn apply_op(
    op: &PatchOp,
    working: &mut BTreeMap<String, String>,
    replacements: &mut BTreeMap<String, usize>,
    errors: &mut Vec<PreviewError>,
) {
    match op {
        PatchOp::RenameFunction {
            old_name,
            new_name,
            files,
        } => {
            let paths: Vec<String> = if files.is_empty() {
                working
                    .keys()
                    .filter(|p| p.ends_with(".rs"))
                    .cloned()
                    .collect()
            } else {
                files
                    .iter()
                    .filter(|p| working.contains_key(p.as_str()))
                    .cloned()
                    .collect()
            };
            for path in paths {
                let src = working.get(&path).cloned().unwrap_or_default();
                match rust_rename::rename(&src, old_name, new_name) {
                    Ok((new_src, n)) => {
                        if n > 0 {
                            working.insert(path.clone(), new_src);
                            *replacements.entry(path).or_insert(0) += n;
                        }
                    }
                    Err(msg) => errors.push(PreviewError {
                        file: path,
                        message: msg,
                    }),
                }
            }
        }
    }
}

fn render_unified_diff(path: &str, before: &str, after: &str) -> String {
    let diff = TextDiff::from_lines(before, after);
    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    for group in diff.grouped_ops(3).iter() {
        out.push_str("@@\n");
        for op in group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                };
                out.push(sign);
                out.push_str(change.value());
                if !change.value().ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_produces_diff() {
        let mut files = BTreeMap::new();
        files.insert(
            "src/lib.rs".into(),
            "fn add(a:i32,b:i32)->i32{a+b}\nfn main(){let _=add(1,2);}\n".into(),
        );
        let plan = PatchPlan::labelled(
            vec![PatchOp::RenameFunction {
                old_name: "add".into(),
                new_name: "sum".into(),
                files: vec![],
            }],
            "rename add->sum",
        );
        let out = preview(&plan, &files).unwrap();
        assert_eq!(out.files.len(), 1);
        assert_eq!(out.files[0].replacements, 2);
        assert_eq!(out.total_replacements, 2);
        assert!(out.files[0].diff.contains("-fn add"));
        assert!(out.files[0].diff.contains("+fn sum"));
    }

    #[test]
    fn empty_diff_when_no_match() {
        let mut files = BTreeMap::new();
        files.insert("src/lib.rs".into(), "fn foo(){}\n".into());
        let plan = PatchPlan::new(vec![PatchOp::RenameFunction {
            old_name: "nope".into(),
            new_name: "nope2".into(),
            files: vec![],
        }]);
        let out = preview(&plan, &files).unwrap();
        assert_eq!(out.files.len(), 0);
        assert_eq!(out.total_replacements, 0);
    }
}
