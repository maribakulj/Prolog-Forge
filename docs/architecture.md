# Architecture — Prolog Forge

This document is the running reference for the internal architecture of the
Core. Its long-form, opinionated version (mission, design principles, MVP,
roadmap, risks, etc.) lives in the architecture blueprint; this file tracks
the *current* implementation state.

## Current state — Phase 0

The Core is a Rust workspace split into focused crates. Nothing in the list
below depends on any editor; the entire product is reachable through
JSON-RPC.

| Crate | Role |
|---|---|
| `pf-protocol` | JSON-RPC 2.0 types, LSP-style framing, API contract. |
| `pf-csm` | Common Semantic Model v0 (minimal entity/relation types + `LanguageAnalyzer` trait). |
| `pf-graph` | In-memory knowledge graph — n-ary facts, layers, pattern matching. |
| `pf-rules` | Datalog-v1 engine — hand-rolled parser, naive bottom-up evaluator. |
| `pf-persist` | KV trait + in-memory backend. Disk-backed store lands in Phase 1. |
| `pf-core` | Session/workspace manager, API dispatcher (`dispatch`). |
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
client  ──►  rules.load(src)             ──►  {rules_added, facts_added}
client  ──►  rules.evaluate              ──►  {derived, iterations}
client  ──►  graph.query(pattern)        ──►  {count, bindings[]}
client  ──►  session.shutdown
```

This is the smallest coherent loop that demonstrates the neuro-symbolic
backbone: facts in → rules fire → derived facts → queryable graph. The LLM,
patch planner, validator, and explainer slot on top of this loop in the
following phases.

## What is deliberately missing in Phase 0

- Language analyzers (no Rust/TS/Python lowering yet — analyzers come in the
  MVP, Phase 1).
- LLM orchestration.
- Patch planning / application.
- Validation pipeline (syntactic, type, behavioral oracles).
- Explainer / proof-tree renderer.
- Persistence to disk.
- Notifications / streaming / cancellation.

Each of those has a crate slot or a module reserved for it; adding them
should not require touching any Phase 0 module beyond extending the API
enum.
