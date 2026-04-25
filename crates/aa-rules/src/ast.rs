//! Datalog AST.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Term {
    Const(String),
    Var(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Atom {
    pub predicate: String,
    pub args: Vec<Term>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub head: Atom,
    pub body: Vec<Atom>,
}

/// A parsed Datalog source file yields a mix of ground facts (rules with an
/// empty body and no variables in the head) and proper rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Program {
    pub rules: Vec<Rule>,
    /// Ground facts, i.e. clauses of the form `p(a, b).` with no variables.
    pub facts: Vec<Atom>,
}

impl Rule {
    pub fn is_ground_fact(&self) -> bool {
        self.body.is_empty() && self.head.args.iter().all(|t| matches!(t, Term::Const(_)))
    }
}
