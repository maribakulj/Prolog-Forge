//! Datalog-v1 rule engine.
//!
//! The surface syntax is Prolog-flavored for ergonomics:
//!
//! ```text
//! parent(alice, bob).
//! parent(bob, carol).
//! ancestor(X, Y) :- parent(X, Y).
//! ancestor(X, Z) :- parent(X, Y), ancestor(Y, Z).
//! ```
//!
//! The semantics are pure Datalog v1:
//!   - no function symbols,
//!   - no negation (stratification trivially holds),
//!   - no aggregates.
//!
//! Evaluation is bottom-up to a fixpoint. The Phase 0 evaluator is the naive
//! variant — correct and terminating; semi-naive / incremental comes in
//! Phase 1.

pub mod ast;
pub mod eval;
pub mod parser;

pub use ast::{Atom, Program, Rule, Term};
pub use eval::{evaluate, EvalStats};
pub use parser::{parse, ParseError};
