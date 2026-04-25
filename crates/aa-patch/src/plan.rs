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

/// Apply every op in `plan` to a working copy of `files` and return the
/// resulting shadow map plus any per-op errors. Shared between
/// [`preview`] (which additionally renders diffs) and the `aa-core`
/// apply/explain paths (which need the shadow map directly and cannot
/// reconstruct it from a diff). Centralising op handling here keeps
/// typed-vs-syntactic rename semantics in one place.
///
/// Uses a fresh [`crate::OneShotResolver`] for any `RenameFunctionTyped`
/// ops — every such op spawns a fresh `rust-analyzer`. Call
/// [`apply_plan_with_resolver`] instead to plug in `aa-core`'s
/// persistent session pool.
pub fn apply_plan(
    plan: &PatchPlan,
    files: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<PreviewError>) {
    apply_plan_with_resolver(plan, files, &crate::OneShotResolver)
}

/// Same as [`apply_plan`] but lets the caller supply a persistent
/// [`crate::TypedRenameResolver`]. `aa-core` passes its session pool
/// here so successive typed-rename ops on the same workspace share a
/// single warm `rust-analyzer`.
pub fn apply_plan_with_resolver(
    plan: &PatchPlan,
    files: &BTreeMap<String, String>,
    resolver: &dyn crate::TypedRenameResolver,
) -> (BTreeMap<String, String>, Vec<PreviewError>) {
    let mut working: BTreeMap<String, String> = files.clone();
    let mut per_file_replacements: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors: Vec<PreviewError> = Vec::new();
    for op in &plan.ops {
        apply_op(
            op,
            &mut working,
            &mut per_file_replacements,
            &mut errors,
            resolver,
        );
    }
    let _ = per_file_replacements;
    (working, errors)
}

pub fn preview(
    plan: &PatchPlan,
    files: &BTreeMap<String, String>,
) -> Result<PatchPreview, PatchError> {
    preview_with_resolver(plan, files, &crate::OneShotResolver)
}

/// Variant of [`preview`] that takes a resolver for typed ops. Used
/// by `aa-core` when the session pool is available.
pub fn preview_with_resolver(
    plan: &PatchPlan,
    files: &BTreeMap<String, String>,
    resolver: &dyn crate::TypedRenameResolver,
) -> Result<PatchPreview, PatchError> {
    // Start from the input file set; each op produces a new set.
    let mut working: BTreeMap<String, String> = files.clone();
    let mut per_file_replacements: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors: Vec<PreviewError> = Vec::new();

    for op in &plan.ops {
        apply_op(
            op,
            &mut working,
            &mut per_file_replacements,
            &mut errors,
            resolver,
        );
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
    resolver: &dyn crate::TypedRenameResolver,
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
        PatchOp::RenameFunctionTyped {
            decl_file,
            decl_line,
            decl_character,
            new_name,
            old_name: _,
        } => {
            // Delegate to the resolver. When RA is unavailable, emit
            // a preview-level diagnostic and leave the shadow
            // untouched. The resolver may be the one-shot variant
            // (spawn per call) or the pool variant (persistent
            // session) — the planner doesn't care.
            match resolver.resolve(crate::typed_rename::TypedRenameRequest {
                files: working,
                decl_file,
                decl_line: *decl_line,
                decl_character: *decl_character,
                new_name,
                timeout: std::time::Duration::from_secs(60),
            }) {
                Ok(new_files) => {
                    // Count changed files so the preview's `replacements`
                    // total stays comparable to the syntactic path.
                    for (rel, content) in &new_files {
                        if working.get(rel) != Some(content) {
                            *replacements.entry(rel.clone()).or_insert(0) += 1;
                        }
                    }
                    *working = new_files;
                }
                Err(crate::typed_rename::TypedRenameError::Unavailable(msg)) => {
                    errors.push(PreviewError {
                        file: decl_file.clone(),
                        message: format!(
                            "rename_function_typed: rust-analyzer not available ({msg}); \
                             install rust-analyzer on PATH to use the scope-resolved variant, \
                             or fall back to `rename_function` for a macro-aware (but not \
                             scope-aware) rename"
                        ),
                    });
                }
                Err(e) => errors.push(PreviewError {
                    file: decl_file.clone(),
                    message: format!("rename_function_typed: {e}"),
                }),
            }
        }
        PatchOp::AddDeriveToStruct {
            type_name,
            derives,
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
                match crate::add_derive::add_derive(&src, type_name, derives) {
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
        PatchOp::RemoveDeriveFromStruct {
            type_name,
            derives,
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
                match crate::add_derive::remove_derive(&src, type_name, derives) {
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
        PatchOp::InlineFunction { function, files } => {
            // Cross-file op: the transform takes the entire working map
            // (it needs to locate the unique definition anywhere in
            // scope) and returns both a new map and per-file counts.
            match crate::inline::inline_function(working, function, files) {
                Ok((new_files, per_file)) => {
                    for (path, new_src) in new_files {
                        working.insert(path, new_src);
                    }
                    for (path, n) in per_file {
                        *replacements.entry(path).or_insert(0) += n;
                    }
                }
                Err(msg) => errors.push(PreviewError {
                    file: files.first().cloned().unwrap_or_default(),
                    message: msg,
                }),
            }
        }
        PatchOp::ExtractFunction {
            source_file,
            start_line,
            end_line,
            new_name,
            params,
            files,
        } => {
            // Single-file op: the transform rewrites only `source_file`
            // (the call site + the inserted helper). `files`, when
            // non-empty, is used to gate scope just like the per-file
            // ops above. When empty, the file is required to be in
            // `working`.
            if !files.is_empty() && !files.iter().any(|f| f == source_file) {
                return;
            }
            let Some(src) = working.get(source_file).cloned() else {
                errors.push(PreviewError {
                    file: source_file.clone(),
                    message: format!("extract_function: source_file `{source_file}` not in scope"),
                });
                return;
            };
            match crate::extract::extract_function(&src, *start_line, *end_line, new_name, params) {
                Ok((new_src, n)) => {
                    if n > 0 {
                        working.insert(source_file.clone(), new_src);
                        *replacements.entry(source_file.clone()).or_insert(0) += n;
                    }
                }
                Err(msg) => errors.push(PreviewError {
                    file: source_file.clone(),
                    message: msg,
                }),
            }
        }
        PatchOp::ChangeSignature {
            function,
            new_params,
            files,
        } => {
            // Cross-file op: the transform takes the entire working
            // map (it needs to locate the unique definition + every
            // bare call site anywhere in scope) and returns both a
            // new map and per-file counts. Same shape as
            // `InlineFunction`'s call.
            match crate::change_sig::change_signature(working, function, new_params, files) {
                Ok((new_files, per_file)) => {
                    for (path, new_src) in new_files {
                        working.insert(path, new_src);
                    }
                    for (path, n) in per_file {
                        *replacements.entry(path).or_insert(0) += n;
                    }
                }
                Err(msg) => errors.push(PreviewError {
                    file: files.first().cloned().unwrap_or_default(),
                    message: msg,
                }),
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
