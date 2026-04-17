# Prolog Forge

**A neuro-symbolic development runtime with an autonomous core.**
Editor-agnostic. Rust. JSON-RPC 2.0. Apache-2.0.

Prolog Forge is **not** a VS Code plugin. It is a headless daemon that
ingests a repository, builds a knowledge graph of its code, runs a Datalog
rule engine over that graph, and — in later phases — orchestrates LLMs
inside that structured frame to plan, apply, and explain patches.

Editors, CLIs, CI systems, and autonomous agents are all **thin clients** of
the same local protocol. The core never imports an editor SDK.

> Status: **Phase 0 (foundations)** — the rule engine, graph store, protocol,
> daemon, and reference CLI are implemented and tested end-to-end. Language
> analyzers, LLM orchestration, and patch planning land in the phases that
> follow. See [Roadmap](#roadmap).

---

## Why the name

The inspiration is Prolog — declarative logic programming, facts + rules,
unification as the atom of reasoning. The *implementation* is Datalog, in
Rust, chosen for termination guarantees, bottom-up incremental evaluation,
and scalable static analysis over large code graphs. See
[`docs/rules-dsl.md`](docs/rules-dsl.md) for the surface syntax.

Prolog Forge forges **with** the spirit of Prolog, not **in** Prolog.

---

## The central hypothesis

LLMs alone are unreliable at code. Symbolic analyzers alone are blind to
intent. The promising middle is a system where:

- **facts** are extracted deterministically from source code by language
  analyzers,
- **rules** derive more facts and flag violations symbolically,
- an **LLM** works only inside this structured frame — its outputs are
  typed, constrained, resolved against the graph, and always produce
  candidates, never validated truths,
- every fact, patch, and decision is **traceable** to its causes.

The five epistemic layers are first-class, strictly disjoint:

| Layer | Source | Trust |
|---|---|---|
| `observed` | Analyzer parsing | Ground truth, may be stale |
| `inferred` | Validated rules over observed | As strong as premises |
| `candidate` | LLM / pattern miner | Hypothesis, never autoritative |
| `validated` | Candidate promoted by a human | As strong as reviewer |
| `constraint` | Project invariants | Violation = error |

Phase 0 implements `observed` and `inferred` end-to-end.

---

## Architecture at a glance

```
 adapters ──► JSON-RPC (stdio) ──► Core
                                    ├── ingestion       (Phase 1)
                                    ├── CSM             (v0 shipped)
                                    ├── knowledge graph (Phase 0 ✓)
                                    ├── rule engine     (Phase 0 ✓)
                                    ├── LLM orchestrator (Phase 1)
                                    ├── patch planner    (Phase 1)
                                    ├── validator        (Phase 1)
                                    └── explainer        (Phase 2)
```

The Core is a Rust workspace. Every module has a single role and a narrow
interface. See [`docs/architecture.md`](docs/architecture.md).

| Crate | Role |
|---|---|
| [`pf-protocol`](crates/pf-protocol) | JSON-RPC types, LSP-style framing, API contract |
| [`pf-csm`](crates/pf-csm) | Common Semantic Model (v0) |
| [`pf-graph`](crates/pf-graph) | In-memory knowledge graph (facts, layers, pattern matching) |
| [`pf-rules`](crates/pf-rules) | Datalog-v1 parser + evaluator |
| [`pf-persist`](crates/pf-persist) | KV trait + in-memory backend |
| [`pf-core`](crates/pf-core) | Session manager + API dispatcher |
| [`pf-daemon`](crates/pf-daemon) | Binary: stdio JSON-RPC server |
| [`pf-cli`](crates/pf-cli) | Binary: reference adapter + CI tool (`pf`) |

Adapters (VS Code, Emacs, Neovim, JetBrains, …) live in a separate
`adapters/` tree, intentionally kept thin. They are not part of Phase 0.

---

## Quick start

### Build

```bash
cargo build --workspace
cargo test --workspace
```

You get two binaries:

- `target/debug/pf` — the reference CLI.
- `target/debug/pf-daemon` — the stdio JSON-RPC server.

### Try the rule engine

Create a Datalog file:

```prolog
% family.pfr
parent(alice, bob).
parent(bob, carol).
parent(carol, dan).

ancestor(X, Y) :- parent(X, Y).
ancestor(X, Z) :- parent(X, Y), ancestor(Y, Z).
```

Run it:

```bash
$ pf run family.pfr
derived: 6
iterations: 4

$ pf query family.pfr 'ancestor(alice, X)'
3 result(s)
  {"X":"bob"}
  {"X":"carol"}
  {"X":"dan"}
```

### Talk to the daemon

The daemon speaks JSON-RPC 2.0 with LSP-style `Content-Length` framing on
stdio. Logs go to stderr so the stdout channel stays clean.

```bash
cargo run -q --bin pf-daemon
```

A full end-to-end smoke test lives at
[`tooling/smoke/daemon_smoke.py`](tooling/smoke/daemon_smoke.py). It runs in
CI and is the minimal reference client.

### Methods available today

| Method | Purpose |
|---|---|
| `session.initialize` | Handshake, returns server capabilities. |
| `session.shutdown` | Terminate cleanly. |
| `workspace.open` | Register a workspace root. |
| `workspace.status` | Counts of facts, rules, derived. |
| `graph.ingestFact` | Insert facts programmatically. |
| `graph.query` | Pattern-match an atom against the graph. |
| `rules.load` | Parse and register a Datalog source block. |
| `rules.evaluate` | Run the engine to fixpoint. |

Typed schemas live in [`schemas/protocol.json`](schemas/protocol.json). The
Rust wire types in `pf-protocol` are kept in sync with that file by hand in
Phase 0; schema-first codegen is a Phase 1 item.

---

## Design principles

1. **Editor-agnostic by construction.** Any IDE dependency inside
   `crates/` is a bug.
2. **Symbolic first, neural second.** The LLM is a bounded collaborator,
   never the authority.
3. **The common semantic model is the unit of truth.** Languages are
   dialects; the graph is canonical.
4. **Epistemic layers do not mix.** Observed ≠ inferred ≠ candidate ≠
   validated ≠ constraint.
5. **The graph is a compute substrate, not a visualization.**
6. **The logic engine is a back-end capability.**
7. **Determinism of the symbolic, constrained stochasticity of the
   neural.** LLM outputs are typed, schema-validated, cached.
8. **Provenance is end-to-end.** Every fact, inference, and patch is
   traceable to its causes.
9. **Incrementality.** Re-parse / re-inference / re-validation at file-level
   granularity.
10. **Read locally, write explicitly.** The core never mutates the filesystem
    without an approved, validated patch.

---

## Phase 0 — what ships, what doesn't

### Shipping

- JSON-RPC 2.0 protocol with LSP-style framing, versioned.
- Knowledge graph store: n-ary facts, predicate/arity checking,
  epistemic layers, pattern-match query.
- Datalog-v1 engine: Prolog-flavored surface syntax, bottom-up evaluator,
  terminates on any program, full unit coverage of transitive closure
  semantics.
- Core dispatcher + session/workspace management.
- `pf-daemon` headless binary, stdio JSON-RPC.
- `pf` reference CLI (`info`, `check`, `run`, `query`).
- JSON Schemas for every method.
- CI: `cargo fmt`, `cargo clippy -D warnings`, `cargo test`, schema validation,
  daemon smoke test.

### Deliberately not shipping yet

- Language analyzers (Rust / TS / Python) — **MVP / Phase 1**.
- LLM orchestrator, tool-use, prompt graph — **Phase 1**.
- Patch planner and AST-level edit ops — **Phase 1**.
- Validation pipeline (syntactic / type / rule / behavioral) — **Phase 1**.
- Explainer / proof-tree renderer — **Phase 2**.
- Pattern mining and rule promotion (`candidate` → `validated`) — **Phase 3**.
- Disk-backed persistence (RocksDB / SQLite) — **Phase 1**.
- Notifications, streaming, cancellation — **Phase 1**.

Each item has a reserved crate or module; adding it should not require
touching any Phase 0 artifact beyond the API enum.

---

## Roadmap

| Phase | Scope | Approx. horizon |
|---|---|---|
| **0** | Contracts, JSON-RPC, CSM v0, graph, Datalog v1, CLI, CI | **shipped** |
| 1 (MVP) | Rust analyzer, LLM orchestrator, patch + validation loop, VS Code adapter minimal | 2–5 months |
| 2 | Multi-language (TS, Python), property-based validation, Emacs/Neovim, web explainer | 5–8 months |
| 3 | Pattern mining, rule marketplace, provenance export, candidate → validated workflow | 8–12 months |
| 4 | Agent mode, ML-assisted validation, cross-machine incrementality, gRPC transport | 12–18 months |
| 5 | Third-party analyzers SDK, vertical rule packs, CI/CD-native integrations | 18+ months |

---

## Repository layout

```
prolog-forge/
├── Cargo.toml                   # workspace
├── README.md
├── LICENSE                      # Apache-2.0
├── rust-toolchain.toml
├── rustfmt.toml
├── .github/workflows/ci.yml
├── docs/
│   ├── architecture.md
│   ├── protocol.md
│   └── rules-dsl.md
├── schemas/
│   └── protocol.json
├── crates/
│   ├── pf-protocol/
│   ├── pf-csm/
│   ├── pf-graph/
│   ├── pf-rules/
│   ├── pf-persist/
│   ├── pf-core/
│   ├── pf-daemon/
│   └── pf-cli/
└── tooling/
    └── smoke/
        └── daemon_smoke.py
```

Adapters live outside `crates/` (e.g. `adapters/vscode/`, `adapters/emacs/`)
and are added alongside their phase.

---

## Contributing

This is a research-grade codebase aiming at production-grade foundations.
Three contracts are *intentionally rigid* past Phase 0 and breaking them will
be challenged hard in review:

1. the **CSM** shape,
2. the **graph schema** (predicates + layer semantics),
3. the **protocol** (methods, param shapes, error codes).

Everything else is substitutable. Before opening a PR:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 tooling/smoke/daemon_smoke.py
```

Issues and PRs are welcome on
[GitHub](https://github.com/maribakulj/prolog-forge).

---

## License

Apache-2.0. See [`LICENSE`](LICENSE).
