# Architecture — Prolog Forge

This document is the running reference for the internal architecture of the
Core. Its long-form, opinionated version (mission, design principles, MVP,
roadmap, risks, etc.) lives in the architecture blueprint; this file tracks
the *current* implementation state.

## Current state — Phase 1 step 12 (patch algebra expanded: `add_derive_to_struct`)

The Core is a Rust workspace split into focused crates. Nothing in the list
below depends on any editor; the entire product is reachable through
JSON-RPC.

| Crate | Role |
|---|---|
| `pf-protocol` | JSON-RPC 2.0 types, LSP-style framing, API contract. |
| `pf-csm` | Common Semantic Model v0 (minimal entity/relation types + `LanguageAnalyzer` trait). |
| `pf-graph` | In-memory knowledge graph — n-ary facts, layers, pattern matching. |
| `pf-rules` | Datalog-v1 engine — hand-rolled parser, naive bottom-up evaluator. |
| `pf-persist` | KV trait + in-memory backend. Disk-backed store lands in Phase 1 step 2. |
| `pf-ingest` | Filesystem walker, source-file dispatch. |
| `pf-lang-rust` | Rust analyzer backed by `syn`, lowers source to `CsmFragment`. |
| `pf-llm` | Bounded LLM orchestrator: `LlmProvider` trait, `MockProvider`, context selector (trusted layers only, deterministic ordering), prompt builder, content-addressed response cache, identifier-resolution guard. Three LLM modes: `propose` (fact candidates), `refine` (iterative revision with prior rejections + diagnostics), and `propose_patch` (typed `PatchPlan` candidates grounded against the op vocabulary). |
| `pf-patch` | Typed patch planner. Op vocabulary: `RenameFunction` (macro-aware, Phase 1.10), `RenameFunctionTyped` (scope-resolved via rust-analyzer, Phase 1.11), `AddDeriveToStruct` (merge-or-insert `#[derive(...)]` on struct/enum/union, Phase 1.12). `PatchPlan`, pure preview pipeline producing unified diffs via byte-accurate `syn`-driven span edits (comments preserved). |
| `pf-ra-client` | Minimal LSP client for rust-analyzer: Content-Length framing, spawn / initialize / rename / shutdown, in-process mock server for tests. Graceful degradation when RA is absent — the caller receives `ClientError::NotAvailable` and falls back to the syntactic path. |
| `pf-validate` | Pluggable validation pipeline: `ValidationStage` trait, `Pipeline` with fail-fast semantics, `SyntacticStage` re-parsing every changed `.rs` file with `syn`. Semantic stages (`RuleStage`, `CargoCheckStage`, `CargoTestStage`) live in `pf-core` where the dependencies they need are available. |
| `pf-explain` | Proof-carrying explainer: composes observed / inferred / candidate evidence, rule activations (head + premises via `pf_rules::trace_derivations`), and validation stage outcomes into a single `Explanation` with a synthesized verdict. Pure; no I/O. |
| `pf-core` | Session/workspace manager, API dispatcher, CSM→fact lowering, `workspace.index`, `llm.propose`, `llm.refine`, `patch.preview`, `patch.apply` (+ `RuleStage`, disk-persistent commit journal), `patch.rollback`, `explain.patch`. |
| `pf-daemon` | Binary: stdio JSON-RPC server wrapping the Core. |
| `pf-cli` | Binary: reference adapter, also used in CI. |

## Invariants

1. **No editor SDK in the Core.** `pf-protocol` and below must compile without
   any IDE dependency. Adapters live outside `crates/`.
2. **Epistemic layers are strictly disjoint.** `observed`, `inferred`,
   `candidate`, `validated`, `constraint`. They never collapse at the storage
   or API layer.
3. **The graph is canonical.** Every analyzer lowers to CSM, which flattens to
   facts. No querying goes around the graph.
4. **The rule engine writes only `inferred`.** Observed facts flow from
   analyzers; inferred facts from rules; neither path promotes the other.
5. **The protocol is versioned.** `pf-protocol::PROTOCOL_VERSION`. MAJOR
   breaks wire compat; MINOR is additive.

## Artifacts that must not churn after Phase 0

The three things that, once released, become expensive to change:

- the **Common Semantic Model** (shape of entities, relations, spans);
- the **graph schema** (predicate conventions, layer semantics);
- the **protocol** (method names, param shapes, error codes).

Everything else is substitutable.

## End-to-end flow (implemented today)

```
client  ──►  session.initialize          ──►  ServerCapabilities
client  ──►  workspace.open(root)        ──►  WorkspaceId
client  ──►  workspace.index             ──►  {files, entities, relations, facts}
client  ──►  rules.load(src)             ──►  {rules_added, facts_added}
client  ──►  rules.evaluate              ──►  {derived, iterations}
client  ──►  graph.query(pattern)        ──►  {count, bindings[]}
client  ──►  llm.propose(anchor, intent) ──►  {accepted, rejected, outcomes}
client  ──►  llm.refine(anchor, intent,
                         prior_outcomes,
                         prior_diagnostics,
                         max_rounds)      ──►  {rounds, converged, outcomes, rounds_summary[]}
client  ──►  llm.propose_patch(anchor,
                               intent)    ──►  {accepted, rejected, candidates[{plan, justification, accepted, rejection_reason}]}
client  ──►  patch.preview(plan)         ──►  {total_replacements, files[], errors[]}
client  ──►  patch.apply(plan)           ──►  {applied, commit_id, validation, …}
client  ──►  patch.rollback(commit_id)   ──►  {rolled_back, files_restored, …}
client  ──►  explain.patch(plan)         ──►  {verdict, evidence[], stats, summary}
client  ──►  session.shutdown
```

### Neuro-symbolic loop (Phase 1 step 6)

Phase 1 steps 2–4 gave the runtime a one-shot `propose → validate → apply`
path. Step 6 closes the loop on both sides:

- **`llm.refine`** turns the single prompt into a bounded iterative
  dialogue. Each round renders a `refine.v1` prompt carrying forward
  *every* prior rejection reason and validator diagnostic, calls the
  provider through the same trait as `propose` (caching identical prompts
  round-by-round), and filters the response through the same
  identifier-resolution guard. The loop exits early when a round produces
  zero rejections; otherwise it terminates at `max_rounds`. Outcomes are
  tagged with the round that produced them so callers can visualize how
  the hypothesis set tightened.

- **`explain.patch`** synthesizes a proof-carrying explanation for a
  typed plan: observed facts mentioning the plan's anchors, candidates
  considered (with justifications and rejection reasons), rule
  activations captured by `pf_rules::trace_derivations` (head + premises),
  validation stages with their diagnostics, and a three-state verdict
  (`accepted` / `rejected` / `not_proven`). The verdict is `NotProven`
  when only the syntactic stage is available — an honest acknowledgement
  that green syntax is not a proof of semantic safety.

### LLM orchestrator invariants

- The provider trait takes typed `LlmRequest { system, user, schema_id, ... }`. Nothing else.
- Context is extracted from the graph by `ContextSelector`, and **only from trusted layers** (`observed` ∪ `inferred`). `candidate`, `validated`, `constraint` never leak back into a prompt.
- Output is parsed against a strict `#[serde(deny_unknown_fields)]` schema. Non-conforming responses are rejected.
- Every identifier in a proposal is resolved against the set of entity ids in the graph. Unknown ids = hallucination → rejection.
- Accepted proposals are inserted at `FactLayer::Candidate` and **never** cross into `Inferred` or `Validated` without an explicit human promotion step (Phase 3).
- Every `(provider, request)` pair is cached; identical inputs yield byte-identical responses.

This demonstrates the neuro-symbolic backbone end-to-end on **real code**:
analyzer lowers Rust source → CSM → observed facts → rules fire → derived
facts → queryable graph. The LLM, patch planner, validator, and explainer
slot on top of this loop in the following steps.

## What is deliberately missing (still)

- TypeScript / Python analyzers.
- Type-aware Rust analysis (cross-module resolution via rust-analyzer).
- Network LLM providers (Anthropic, OpenAI, local llama.cpp) — the trait is ready; only the mock is wired in Phase 1.2.
- LLM modes beyond proposer / refiner: classifier, planner, oracle.
- NL rendering of proof trees (current explainer is structured JSON; the web renderer lands in Phase 2).
- Type-aware *rename* (scope resolution via rust-analyzer) — Phase 2. The type-aware *validation* stage (`cargo_check`) is shipped in Phase 1.7.
- Behavioral stage (run impacted tests).
- Content-addressed journal (current format is plain JSON and stores full before/after bytes per file — fine at MVP scale, compressed CAS coming with the disk-backed `pf-persist`).
- Cross-commit rollback (Phase 1.5 rollback is single-commit; a redo/undo stack arrives later).
- **Rust-analyzer in CI.** The `pf-ra-client` crate is end-to-end tested against the real `rust-analyzer` binary only when one is on `PATH` (the test self-skips otherwise). The CI host used while Phase 1.11 was shipped does not carry RA, so the typed-rename path is CI-verified through the in-process mock only; the real-binary round-trip is exercised locally. Persistent RA indexing (keep one session alive across requests) is a Phase 2 follow-up — today each typed rename spawns a fresh RA.
- Patch planning / application (minimal).
- Persistence to disk.
- Notifications / streaming / cancellation.

Each of those has a crate slot or a module reserved for it; adding them
should not require touching any Phase 0 module beyond extending the API
enum.
