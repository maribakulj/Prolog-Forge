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

    pub fn build(&self, intent: &str, facts: &[Fact]) -> (String, String) {
        let context = render_facts(facts);
        let user = self
            .user_template
            .replace("{{intent}}", intent)
            .replace("{{context}}", &context);
        (self.system_template.to_string(), user)
    }
}

pub fn render_facts(facts: &[Fact]) -> String {
    let mut out = String::new();
    for f in facts {
        let args = f.args.join(", ");
        out.push_str(&format!("{}({}).\n", f.predicate, args));
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
}
