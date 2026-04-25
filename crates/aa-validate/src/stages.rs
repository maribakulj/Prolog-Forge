//! Built-in stages.

use crate::stage::{Diagnostic, Severity, StageReport, ValidationContext, ValidationStage};

/// Parses every `.rs` file in the shadow workspace with `syn`. A patch that
/// produces invalid Rust anywhere is rejected outright.
pub struct SyntacticStage;

impl ValidationStage for SyntacticStage {
    fn name(&self) -> &'static str {
        "syntactic"
    }

    fn validate(&self, ctx: &ValidationContext<'_>) -> StageReport {
        let mut diags: Vec<Diagnostic> = Vec::new();
        for (path, content) in ctx.shadow_files {
            if !path.ends_with(".rs") {
                continue;
            }
            if let Err(e) = syn::parse_file(content) {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    file: Some(path.clone()),
                    message: format!("syn parse error: {e}"),
                });
            }
        }
        StageReport::with_errors(self.name(), diags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn accepts_valid_rust() {
        let mut shadow = BTreeMap::new();
        shadow.insert("src/lib.rs".into(), "fn main(){}".into());
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &shadow,
        };
        let r = SyntacticStage.validate(&ctx);
        assert!(r.ok);
        assert_eq!(r.diagnostics.len(), 0);
    }

    #[test]
    fn rejects_broken_rust() {
        let mut shadow = BTreeMap::new();
        shadow.insert("src/lib.rs".into(), "fn main(){".into());
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &shadow,
        };
        let r = SyntacticStage.validate(&ctx);
        assert!(!r.ok);
        assert_eq!(r.diagnostics.len(), 1);
        assert_eq!(r.diagnostics[0].file.as_deref(), Some("src/lib.rs"));
    }
}
