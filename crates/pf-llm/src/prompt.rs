//! Prompt construction.
//!
//! Prompts are never free text from a client. They are assembled here from a
//! (system template, context facts, intent) triple so the shape is auditable
//! and versioned. Templates are strings with explicit placeholders; a real
//! templating engine is unnecessary at this stage.

use pf_graph::Fact;

pub struct PromptBuilder {
    pub system_template: &'static str,
    pub user_template: &'static str,
}

impl PromptBuilder {
    pub fn propose_v1() -> Self {
        Self {
            system_template: SYSTEM_PROPOSE,
            user_template: USER_PROPOSE,
        }
    }

    pub fn refine_v1() -> Self {
        Self {
            system_template: SYSTEM_REFINE,
            user_template: USER_REFINE,
        }
    }

    pub fn build(&self, intent: &str, facts: &[Fact]) -> (String, String) {
        let context = render_facts(facts);
        let user = self
            .user_template
            .replace("{{intent}}", intent)
            .replace("{{context}}", &context);
        (self.system_template.to_string(), user)
    }

    /// Build a refinement prompt. `prior_rejections` is rendered as a
    /// structured block the provider can parse verbatim; `diagnostics` is
    /// the list of validator complaints from the previous round.
    pub fn build_refine(
        &self,
        intent: &str,
        facts: &[Fact],
        prior_rejections: &[RejectionLine<'_>],
        diagnostics: &[DiagnosticLine<'_>],
    ) -> (String, String) {
        let context = render_facts(facts);
        let rejections = render_rejections(prior_rejections);
        let diags = render_diagnostics(diagnostics);
        let user = self
            .user_template
            .replace("{{intent}}", intent)
            .replace("{{context}}", &context)
            .replace("{{rejections}}", &rejections)
            .replace("{{diagnostics}}", &diags);
        (self.system_template.to_string(), user)
    }
}

/// One prior rejection carried over to the refinement round.
#[derive(Debug, Clone, Copy)]
pub struct RejectionLine<'a> {
    pub predicate: &'a str,
    pub args: &'a [String],
    pub reason: &'a str,
}

/// One prior validation diagnostic carried over to the refinement round.
#[derive(Debug, Clone, Copy)]
pub struct DiagnosticLine<'a> {
    pub severity: &'a str,
    pub file: Option<&'a str>,
    pub message: &'a str,
}

pub fn render_facts(facts: &[Fact]) -> String {
    let mut out = String::new();
    for f in facts {
        let args = f.args.join(", ");
        out.push_str(&format!("{}({}).\n", f.predicate, args));
    }
    out
}

fn render_rejections(lines: &[RejectionLine<'_>]) -> String {
    if lines.is_empty() {
        return "(none)\n".into();
    }
    let mut out = String::new();
    for r in lines {
        out.push_str(&format!(
            "- {}({}) — {}\n",
            r.predicate,
            r.args.join(", "),
            r.reason,
        ));
    }
    out
}

fn render_diagnostics(lines: &[DiagnosticLine<'_>]) -> String {
    if lines.is_empty() {
        return "(none)\n".into();
    }
    let mut out = String::new();
    for d in lines {
        match d.file {
            Some(f) => out.push_str(&format!("- [{}] {}: {}\n", d.severity, f, d.message)),
            None => out.push_str(&format!("- [{}] {}\n", d.severity, d.message)),
        }
    }
    out
}

const SYSTEM_PROPOSE: &str = "You are a static-analysis proposer attached to the Prolog Forge \
neuro-symbolic runtime. Given a set of observed facts about a codebase and an intent, you \
return *candidate* hypothesis facts that a human reviewer might want to validate. You never \
invent identifiers that do not appear in the context. Your output MUST be valid JSON matching \
the schema { candidates: [{ predicate: string, args: [string], justification: string }] }.";

const USER_PROPOSE: &str = "Intent: {{intent}}\n\n\
Context (observed facts):\n{{context}}\n\
Respond with JSON only, no prose.";

const SYSTEM_REFINE: &str = "You are a static-analysis *refiner* attached to the Prolog Forge \
neuro-symbolic runtime. A previous round produced candidate facts that were rejected, either \
by the identifier resolver (unknown ids — hallucinations) or by downstream validators \
(rule / type / behavioral diagnostics). Your job is to produce a *revised* set of candidates \
that (1) avoids every identifier previously flagged as a hallucination, (2) addresses every \
diagnostic that can be addressed by changing the proposal, and (3) never invents new \
identifiers outside the provided context. Same output schema as the proposer: \
{ candidates: [{ predicate: string, args: [string], justification: string }] }.";

const USER_REFINE: &str = "Intent: {{intent}}\n\n\
Context (observed facts):\n{{context}}\n\
Prior rejections (do not repeat these):\n{{rejections}}\n\
Prior validator diagnostics (address these if your proposals caused them):\n{{diagnostics}}\n\
Respond with JSON only, no prose.";

#[cfg(test)]
mod tests {
    use super::*;
    use pf_protocol::FactLayer;

    #[test]
    fn builds_prompt() {
        let b = PromptBuilder::propose_v1();
        let facts = vec![Fact {
            predicate: "function".into(),
            args: vec!["id_a".into(), "a".into()],
            layer: FactLayer::Observed,
        }];
        let (sys, user) = b.build("propose purity", &facts);
        assert!(sys.contains("proposer"));
        assert!(user.contains("propose purity"));
        assert!(user.contains("function(id_a, a)."));
    }

    #[test]
    fn refine_prompt_includes_rejections_and_diagnostics() {
        let b = PromptBuilder::refine_v1();
        let facts = vec![Fact {
            predicate: "function".into(),
            args: vec!["id_a".into(), "a".into()],
            layer: FactLayer::Observed,
        }];
        let bogus = vec!["does_not_exist".to_string()];
        let rejs = vec![RejectionLine {
            predicate: "pure",
            args: &bogus,
            reason: "unknown identifier `does_not_exist` (hallucination)",
        }];
        let diags = vec![DiagnosticLine {
            severity: "error",
            file: Some("src/lib.rs"),
            message: "syn parse error: ...",
        }];
        let (sys, user) = b.build_refine("refine", &facts, &rejs, &diags);
        assert!(sys.contains("refiner"));
        assert!(user.contains("Prior rejections"));
        assert!(user.contains("pure(does_not_exist)"));
        assert!(user.contains("hallucination"));
        assert!(user.contains("syn parse error"));
    }
}
