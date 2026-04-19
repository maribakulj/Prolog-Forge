//! Bounded LLM orchestrator.
//!
//! The Core never lets an LLM touch anything directly. Every interaction
//! flows through this crate, which enforces the non-negotiables:
//!
//! - **Typed I/O.** The LLM is asked to produce JSON that conforms to a
//!   declared shape; non-conforming output is rejected.
//! - **Context comes from the graph.** Free text prompts are prohibited in
//!   the public API — the caller hands in a sub-graph (a set of facts) and
//!   an intent.
//! - **Outputs never become observed or inferred.** They land at the
//!   `candidate` epistemic layer, and only after their identifiers have
//!   been resolved against the graph.
//! - **Cached and accountable.** Identical requests return identical
//!   responses; every call is hashed and (in later phases) persisted as
//!   provenance.
//!
//! Phase 1.2 ships the trait shape and a `MockProvider`. Network providers
//! (Anthropic, OpenAI, local llama.cpp) slot in behind the same trait.

pub mod cache;
pub mod context;
pub mod prompt;
pub mod propose;
pub mod provider;

pub use cache::ResponseCache;
pub use context::ContextSelector;
pub use prompt::PromptBuilder;
pub use propose::{propose, ProposalOutcome, ProposeRequest, ProposeResult};
pub use provider::{LlmError, LlmProvider, LlmRequest, LlmResponse, MockProvider};
