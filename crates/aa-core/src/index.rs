//! Workspace indexing pipeline: walk → analyze → lower → insert.
//!
//! Phase 1 step 1 wires exactly one analyzer (Rust, via `aa-lang-rust`); the
//! dispatch shape is already extensible — a new language is a new arm in
//! `analyze_file` plus a new crate under `aa-lang-*`.

use std::fs;
use std::path::Path;

use aa_csm::{CsmFragment, LanguageAnalyzer};
use aa_graph::GraphStore;
use aa_ingest::{walk, IngestOptions, SourceFile};
use aa_lang_rust::RustAnalyzer;
use aa_protocol::IndexingError;

use crate::lower::lower;

#[derive(Debug, Default)]
pub struct IndexReport {
    pub files_indexed: usize,
    pub files_failed: usize,
    pub entities: usize,
    pub relations: usize,
    pub facts_inserted: usize,
    pub errors: Vec<IndexingError>,
}

pub fn index_workspace(root: impl AsRef<Path>, graph: &mut GraphStore) -> IndexReport {
    let root = root.as_ref();
    let mut report = IndexReport::default();
    let files = walk(root, &IngestOptions::default());
    for sf in files {
        match analyze_file(&sf) {
            Ok(frag) => {
                report.files_indexed += 1;
                report.entities += frag.entities.len();
                report.relations += frag.relations.len();
                for fact in lower(&frag) {
                    match graph.insert(fact) {
                        Ok(true) => report.facts_inserted += 1,
                        Ok(false) => {}
                        Err(e) => report.errors.push(IndexingError {
                            file: sf.relative.display().to_string(),
                            message: e.to_string(),
                        }),
                    }
                }
            }
            Err(msg) => {
                report.files_failed += 1;
                report.errors.push(IndexingError {
                    file: sf.relative.display().to_string(),
                    message: msg,
                });
            }
        }
    }
    report
}

fn analyze_file(sf: &SourceFile) -> Result<CsmFragment, String> {
    let source = fs::read_to_string(&sf.path).map_err(|e| format!("read: {e}"))?;
    let path_str = sf.relative.display().to_string();
    match sf.language {
        "rust" => RustAnalyzer::new()
            .analyze(&source, &path_str)
            .map_err(|e| e.message),
        other => Err(format!("no analyzer for language `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn indexes_a_small_project() {
        let tmp = std::env::temp_dir().join(format!(
            "aa-core-index-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(
            tmp.join("src/lib.rs"),
            "pub fn add(a:i32,b:i32)->i32{a+b} pub fn main(){let _=add(1,2);}",
        )
        .unwrap();

        let mut g = GraphStore::new();
        let rep = index_workspace(&tmp, &mut g);
        assert_eq!(rep.files_indexed, 1);
        assert_eq!(rep.files_failed, 0);
        assert!(rep.facts_inserted > 0);
        // at least 2 `function` facts (add, main) and 1 `calls` fact
        let fn_count = g.facts_of("function").count();
        assert!(fn_count >= 2, "expected >=2 functions, got {fn_count}");
        assert!(g.facts_of("calls").count() >= 1);
    }
}
