//! Prompt construction.
//!
//! Prompts are never free text from a client. They are assembled here from a
//! (system template, context facts, intent) triple so the shape is auditable
//! and versioned. Templates are strings with explicit placeholders; a real
//! templating engine is unnecessary at this stage.

use aa_graph::Fact;

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

    pub fn propose_patch_v1() -> Self {
        Self {
            system_template: SYSTEM_PROPOSE_PATCH,
            user_template: USER_PROPOSE_PATCH,
        }
    }

    /// Memory-aware patch proposer. Identical response schema as v1
    /// but the user template carries a `Prior successes:` section so
    /// the model can condition on what has already landed on this
    /// repo. Callers build this variant via [`build_with_memory`].
    pub fn propose_patch_v2() -> Self {
        Self {
            system_template: SYSTEM_PROPOSE_PATCH_V2,
            user_template: USER_PROPOSE_PATCH_V2,
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

    /// Build a memory-aware `patch_propose.v2` prompt. `memory_hints`
    /// is rendered verbatim into a `Prior successes:` block so the
    /// model (and any mock or real provider) can condition its
    /// proposals on what has already landed here.
    pub fn build_with_memory(
        &self,
        intent: &str,
        facts: &[Fact],
        memory_hints: &[crate::propose_patch::MemoryHint<'_>],
    ) -> (String, String) {
        let context = render_facts(facts);
        let memory = render_memory(memory_hints);
        let user = self
            .user_template
            .replace("{{intent}}", intent)
            .replace("{{context}}", &context)
            .replace("{{memory}}", &memory);
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

fn render_memory(hints: &[crate::propose_patch::MemoryHint<'_>]) -> String {
    if hints.is_empty() {
        return "(none)\n".into();
    }
    let mut out = String::new();
    for h in hints {
        let ops = if h.ops_summary.is_empty() {
            "(unknown)".to_string()
        } else {
            h.ops_summary.join(",")
        };
        let profile = h.validation_profile.unwrap_or("default");
        out.push_str(&format!(
            "- ops=[{ops}] profile={profile} replacements={} label={}\n",
            h.total_replacements, h.label,
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

const SYSTEM_PROPOSE: &str = "You are a static-analysis proposer attached to the AYE-AYE \
neuro-symbolic runtime. Given a set of observed facts about a codebase and an intent, you \
return *candidate* hypothesis facts that a human reviewer might want to validate. You never \
invent identifiers that do not appear in the context. Your output MUST be valid JSON matching \
the schema { candidates: [{ predicate: string, args: [string], justification: string }] }.";

const USER_PROPOSE: &str = "Intent: {{intent}}\n\n\
Context (observed facts):\n{{context}}\n\
Respond with JSON only, no prose.";

const SYSTEM_REFINE: &str = "You are a static-analysis *refiner* attached to the AYE-AYE \
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

const SYSTEM_PROPOSE_PATCH: &str = "You are a static-analysis *patch proposer* attached to the \
AYE-AYE neuro-symbolic runtime. Given an intent and a set of observed facts about a \
codebase, you return *typed patch plans* that a human reviewer (or a downstream validator \
pipeline) will decide to apply. Plans are bounded by a strict op vocabulary: the only valid \
op today is `rename_function { old_name, new_name, files }`. More ops land in future phases. \
You never invent identifiers that do not appear in the context; the `old_name` of every \
rename op must correspond to an entity in the context. Your output MUST be valid JSON \
matching the schema: \
{ candidates: [ { plan: { ops: [ { op: string, ... } ], label: string }, \
justification: string } ] }.";

const USER_PROPOSE_PATCH: &str = "Intent: {{intent}}\n\n\
Context (observed facts):\n{{context}}\n\
Respond with JSON only, no prose.";

const SYSTEM_PROPOSE_PATCH_V2: &str = "You are a static-analysis *patch proposer* attached to the \
AYE-AYE neuro-symbolic runtime. Given an intent, a set of observed facts about a \
codebase, and a summary of previously-landed patches on this repository, you return typed \
patch plans biased toward shapes that have historically succeeded here. Bounded by the same \
strict op vocabulary as the v1 proposer; hallucinating identifiers that do not appear in the \
context is still rejected downstream. Same output schema: { candidates: [ { plan: { ops: \
[...], label: string }, justification: string } ] }.";

const USER_PROPOSE_PATCH_V2: &str = "Intent: {{intent}}\n\n\
Context (observed facts):\n{{context}}\n\
Prior successes (past commits on this repo — prefer shapes that have already worked):\n{{memory}}\n\
Respond with JSON only, no prose.";

#[cfg(test)]
mod tests {
    use super::*;
    use aa_protocol::FactLayer;

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
