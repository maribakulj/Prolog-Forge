//! Core-owned concrete `ValidationStage` implementations that need to cross
//! multiple crate boundaries (rule engine + analyzer + CSM lowering) and
//! therefore cannot live inside `pf-validate` without pulling the dependency
//! graph upside-down.

use pf_graph::GraphStore;
use pf_lang_rust::RustAnalyzer;
use pf_rules::Rule;
use pf_validate::{Diagnostic, Severity, StageReport, ValidationContext, ValidationStage};

use pf_csm::LanguageAnalyzer;

/// Re-evaluates the workspace's rule set against the graph derived from the
/// **shadow** source files (not the on-disk ones). Any fact of predicate
/// `violation` derived from the shadow graph counts as a constraint
/// violation and fails the stage.
///
/// This is the Phase 1.5 convention: rule packs that want to gate applies
/// declare rules whose head is `violation(...)`. See `docs/rules-dsl.md`.
pub struct RuleStage {
    rules: Vec<Rule>,
}

impl RuleStage {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }
}

impl ValidationStage for RuleStage {
    fn name(&self) -> &'static str {
        "rules"
    }

    fn validate(&self, ctx: &ValidationContext<'_>) -> StageReport {
        if self.rules.is_empty() {
            return StageReport::ok(self.name());
        }

        // Build a fresh graph from the shadow sources.
        let analyzer = RustAnalyzer::new();
        let mut graph = GraphStore::new();
        let mut diags: Vec<Diagnostic> = Vec::new();
        for (path, src) in ctx.shadow_files {
            if !path.ends_with(".rs") {
                continue;
            }
            match analyzer.analyze(src, path) {
                Ok(frag) => {
                    for fact in crate::lower::lower(&frag) {
                        if let Err(e) = graph.insert(fact) {
                            diags.push(Diagnostic {
                                severity: Severity::Error,
                                file: Some(path.clone()),
                                message: format!("graph insert: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        file: Some(path.clone()),
                        message: format!("shadow analyze: {}", e.message),
                    });
                }
            }
        }

        // Run the rule engine to fixpoint on the shadow graph.
        if let Err(e) = pf_rules::evaluate(&self.rules, &mut graph) {
            diags.push(Diagnostic {
                severity: Severity::Error,
                file: None,
                message: format!("rule engine: {e}"),
            });
            return StageReport::with_errors(self.name(), diags);
        }

        // Collect every derived violation fact.
        for fact in graph.facts_of("violation") {
            diags.push(Diagnostic {
                severity: Severity::Error,
                file: None,
                message: format!("violation({})", fact.args.join(", ")),
            });
        }

        StageReport::with_errors(self.name(), diags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_rules::parse as parse_rules;
    use std::collections::BTreeMap;

    #[test]
    fn no_rules_is_a_pass() {
        let shadow = BTreeMap::new();
        let original = BTreeMap::new();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = RuleStage::new(Vec::new());
        let r = stage.validate(&ctx);
        assert!(r.ok);
        assert_eq!(r.diagnostics.len(), 0);
    }

    #[test]
    fn violation_rule_fires_on_shadow() {
        // Rule: forbid any top-level function named `forbidden`.
        let src = r#"
            violation(F) :- function(F, forbidden).
        "#;
        let program = parse_rules(src).unwrap();
        let mut shadow = BTreeMap::new();
        shadow.insert("src/lib.rs".into(), "pub fn forbidden() {}".into());
        let original = shadow.clone();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = RuleStage::new(program.rules);
        let r = stage.validate(&ctx);
        assert!(!r.ok, "violation rule should fail the stage");
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.message.starts_with("violation(")));
    }

    #[test]
    fn no_violation_is_a_pass() {
        let src = r#"
            violation(F) :- function(F, forbidden).
        "#;
        let program = parse_rules(src).unwrap();
        let mut shadow = BTreeMap::new();
        shadow.insert("src/lib.rs".into(), "pub fn ok_fn() {}".into());
        let original = shadow.clone();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = RuleStage::new(program.rules);
        let r = stage.validate(&ctx);
        assert!(r.ok);
    }
}
