# Prolog Forge

**A neuro-symbolic development runtime with an autonomous core.**
Editor-agnostic. Rust. JSON-RPC 2.0. Apache-2.0.

Prolog Forge is **not** a VS Code plugin. It is a headless daemon that
ingests a repository, builds a knowledge graph of its code, runs a Datalog
rule engine over that graph, and — in later phases — orchestrates LLMs
inside that structured frame to plan, apply, and explain patches.

Editors, CLIs, CI systems, and autonomous agents are all **thin clients** of
the same local protocol. The core never imports an editor SDK.

> Status: **Phase 1, step 11** — scope-resolved rename (Step 2 of
> type-aware) via rust-analyzer. A new `pf-ra-client` crate ships a
> minimal LSP client (Content-Length framing, spawn / initialize /
> rename / shutdown) and the new `PatchOp::RenameFunctionTyped` variant
> routes renames through it: the pipeline mirrors the shadow to a temp
> directory, spawns rust-analyzer against it, asks for a
> `textDocument/rename`, and applies the returned `WorkspaceEdit` back
> to the in-memory shadow. A shadowed local variable of the same name
> as the function is now left alone; only real references to the
> symbol are renamed. When rust-analyzer is absent the op degrades
> gracefully (preview-level diagnostic, FS untouched), the same
> oracle-as-optional pattern `CargoCheckStage` uses when `cargo` is
> missing. `pf rename --scope-resolved` wires the flag on the CLI; the
> LLM orchestrator's `llm.propose_patch` validator also recognises the
> new op tag. See [Roadmap](#roadmap).

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
                                    ├── ingestion       (Phase 1.1 ✓, Rust)
                                    ├── CSM             (v0 shipped)
                                    ├── knowledge graph (Phase 0 ✓)
                                    ├── rule engine     (Phase 0 ✓)
                                    ├── LLM orchestrator (Phase 1.2 ✓, propose)
                                    │   ├── refinement  (Phase 1.6 ✓, iterative llm.refine)
                                    │   └── patch proposer (Phase 1.9 ✓, llm.propose_patch)
                                    ├── patch planner    (Phase 1.3 ✓)
                                    ├── validator        (Phase 1.4–5 ✓, syntactic + rule)
                                    │   ├── typed profile (Phase 1.7 ✓, cargo_check)
                                    │   └── tested profile (Phase 1.8 ✓, cargo_test)
                                    ├── commit journal   (Phase 1.5 ✓, JSON on disk)
                                    └── explainer        (Phase 1.6 ✓, proof-carrying patches)
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
| [`pf-ingest`](crates/pf-ingest) | Filesystem walker and source dispatch |
| [`pf-lang-rust`](crates/pf-lang-rust) | Rust analyzer (syn-based) emitting CSM fragments |
| [`pf-llm`](crates/pf-llm) | Bounded LLM orchestrator: provider trait, mock provider, trusted-only context, schema-validated I/O, response cache, anti-hallucination guard, one-shot `propose` + iterative `refine` loop |
| [`pf-patch`](crates/pf-patch) | Typed patch ops, `PatchPlan`, pure preview pipeline with byte-accurate Rust rename. Two variants: `RenameFunction` (Phase 1.10, macro-aware) and `RenameFunctionTyped` (Phase 1.11, scope-resolved via rust-analyzer) |
| [`pf-ra-client`](crates/pf-ra-client) | Minimal LSP client for `rust-analyzer`: Content-Length framing, spawn/initialize/rename/shutdown, in-process mock server for tests. Powers the Step 2 scope-resolved rename. |
| [`pf-validate`](crates/pf-validate) | Pluggable validation pipeline: `ValidationStage` trait, `SyntacticStage`, fail-fast `Pipeline`. Semantic stages (`RuleStage`, `CargoCheckStage`) live in `pf-core`. |
| [`pf-explain`](crates/pf-explain) | Proof-carrying explainer: composes observed / inferred / candidate evidence + rule activations + validation stages into a single `Explanation` with a synthesized verdict |
| [`pf-core`](crates/pf-core) | Session manager + API dispatcher + indexing pipeline + `llm.propose` + `llm.refine` + `patch.preview` + `patch.apply` + `explain.patch` |
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

### Index a real Rust project

```bash
$ pf index examples/rust-demo \
    --rules examples/rust-demo/rules.pfr \
    --query 'recursive(F)'
indexed: 1 file(s), 9 entity(ies), 21 relation(s), 34 fact(s); failed: 0
rule eval: derived 23 fact(s) in 4 iteration(s)
query: 1 result(s)
  {"F":"src/lib.rs#fn:countdown@src/lib.rs#file"}
```

The indexer lowers each `.rs` file to CSM via `syn`, flattens entities and
relations into graph facts (`function/2`, `calls/2`, `implements/2`, …), and
makes them queryable through the same Datalog surface used by ad-hoc rule
files. Fact schema: [`docs/rules-dsl.md`](docs/rules-dsl.md).

### Ask the bounded LLM orchestrator for candidates

```bash
$ pf propose examples/rust-demo \
    --anchor 'src/lib.rs#fn:add@src/lib.rs#file' \
    --intent 'propose purity invariants'
propose: accepted 1 / rejected 1 (cache_hit=false, tokens_in=393, tokens_out=65)
  ACCEPT pure(src/lib.rs#fn:add@src/lib.rs#file) — no side effects observed for add
  REJECT pure(does_not_exist_in_graph) — …  [why: unknown identifier `does_not_exist_in_graph` (hallucination)]
```

The orchestrator builds context from the graph (trusted layers only),
prompts the provider with a schema-constrained request, rejects any
proposal whose identifiers do not resolve against the graph, and inserts
the survivors at `FactLayer::Candidate`. Candidates are **never** promoted
automatically — a human (or a future validation pipeline) is required to
move them to `validated`. Default provider is the deterministic
`MockProvider`; network providers slot in behind the same trait.

### Close the loop: neuro-symbolic refinement

```bash
$ pf refine examples/rust-demo \
    --anchor 'src/lib.rs#fn:add@src/lib.rs#file' \
    --intent 'refine invariants after validator feedback' \
    --rounds 3
refine: 1 round(s), converged=true, accepted=1, rejected=0 (tokens_in=491, tokens_out=34)
  round 1: accepted=1 rejected=0 cache_hit=false tokens=491+34
  ACCEPT r1 pure(src/lib.rs#fn:add@src/lib.rs#file) — no side effects observed for add
```

`llm.refine` is the iterative counterpart of `llm.propose`. Each round
renders a `refine.v1` prompt that carries forward **every** prior
rejection reason and validator diagnostic as structured feedback, so the
next round is provably better-informed than the last. The loop converges
early when a round produces zero rejections and is capped by
`max_rounds`. Every surviving candidate is tagged with the round that
produced it. Callers typically wire the output of a rejected
`patch.apply` (validation diagnostics) or a rejected `llm.propose`
(hallucinated identifiers) into the next `llm.refine` call, closing the
feedback loop without any free-form prompting.

### Full neuro-symbolic loop: `pf propose-patch`

```bash
$ pf propose-patch examples/rust-demo \
    --anchor 'src/lib.rs#fn:useless@src/lib.rs#file' \
    --profile typed
propose_patch: accepted 1 / rejected 1 (cache_hit=false, tokens_in=478, tokens_out=118)
  [0] ACCEPT rename useless -> useless_renamed — mark useless as renamed for review
         verdict: accepted
           note: plan is a preview; no commit recorded
  [1] REJECT hallucinated rename (expected to be rejected) — intentional hallucination …
         why: op[0] rename_function: unknown identifier `not_a_real_function_anywhere` (hallucination)
```

`llm.propose_patch` is the LLM-proposes-transformations mode. The
orchestrator returns **typed `PatchPlan` candidates** (not fact
candidates) whose shape is exactly the one `patch.preview` / `patch.apply`
/ `explain.patch` accept — no translation layer. Each candidate is:

- **op-validated** — every op must be a known variant of `PatchOp`
  (currently `rename_function`; the vocabulary grows per phase); unknown
  ops are rejected with a structured reason that names the index and the
  bad tag.
- **identifier-grounded** — the `old_name` of every `rename_function` op
  must exist as a `function/2` fact in the graph. Hallucinated names are
  rejected with a reason that downstream adapters (and the web explainer,
  eventually) can display verbatim.
- **immediately explainable** — the CLI chains `explain.patch` on every
  accepted candidate, so the output is a full proof-carrying verdict in
  one command. The `--profile` flag selects the validation pipeline
  (`default` | `typed` | `tested`), making this the direct end-to-end
  demonstration of the thesis: LLM explores, symbolic constrains,
  validator oracles, explainer articulates.

### Proof-carrying patches: `explain.patch`

```bash
$ pf explain examples/rust-demo --from add --to sum --verbose
rename add -> sum: not proven — 2 observed · 0 inferred · 0 rule(s) · 0 candidate(s) · 1 stage(s)
verdict: not proven (syntactic validation only; no rule / type / behavioral evidence)
stats: 2 anchor(s), 2 observed, 0 inferred, 0 rule activation(s), 0 candidate(s), 1 stage(s)
evidence:
  observed[neighbor] function(src/lib.rs#fn:add@src/lib.rs#file, add)
  observed[neighbor] ref_name(src/lib.rs#ref:add, add)
  stage[PASS]    syntactic
```

`explain.patch` produces a structured **proof-carrying explanation** of
a typed plan without touching the filesystem: the observed facts cited,
the rule activations (with head + premises) that touch the plan's
anchors, the candidates considered (with their justifications and
rejection reasons, if any), each validation stage's verdict and
diagnostics, and a three-state final judgment:

- **`accepted`** — the pipeline produced real evidence (rules, types,
  behaviors) *and* every stage passed.
- **`rejected`** — at least one stage failed; the failing stage names
  and their diagnostics are listed.
- **`not_proven`** — every stage passed, but the pipeline was too thin
  to count as a proof (e.g. only the syntactic stage ran). The runtime
  prefers to admit ignorance rather than conflate "parses" with "safe".

This is what makes the project claim "neuro-symbolic runtime" legible:
the same patch is visible as (a) an LLM proposal, (b) a set of
symbolic constraints, (c) a validation trace, and (d) a verdict, all
in one artifact.

### Preview a structured patch

```bash
$ pf rename examples/rust-demo --from add --to sum
preview: 3 replacement(s) across 1 file(s)

# src/lib.rs (531 bytes -> 531 bytes, 3 replacements)
--- a/src/lib.rs
+++ b/src/lib.rs
@@
 //! Tiny fixture used by tests and docs. Intentionally trivial.

-pub fn add(a: i32, b: i32) -> i32 {
+pub fn sum(a: i32, b: i32) -> i32 {
     a + b
 }
…
```

The renamer parses each file with `syn`, collects the byte spans of every
`Ident` matching the source name, applies the edits descending so offsets
stay stable, and re-parses the result to guarantee syntactic validity.
Comments and formatting survive (no pretty-printer round-trip). The
filesystem is **not** touched — `patch.preview` is pure.

### Apply the patch transactionally

```bash
$ pf rename examples/rust-demo --from add --to sum --apply
# …preview diff…
applied: commit commit-18a8a3f598a1c2e1 (1 file(s), 531 bytes)
```

Every `patch.apply` goes through three gates:

1. **Validation pipeline** (`pf-validate`). The default profile runs
   `SyntacticStage` (re-parses every changed `.rs` with `syn`) and,
   when rules are loaded, `RuleStage` (re-evaluates the rule pack on
   the shadow graph and fails if any `violation/*` fact is derived).
   Passing `validation_profile = "typed"` additionally runs
   `CargoCheckStage`, which materialises the shadow files in a temp
   directory and shells out to `cargo check`; compiler errors become
   structured diagnostics. Passing `validation_profile = "tested"`
   layers `CargoTestStage` on top — `cargo test --no-fail-fast` against
   the shadow, with one diagnostic per failing test — so a patch only
   lands if the existing test suite still goes green under the
   proposed source.
2. **Preflight check**. Before writing, the current on-disk content of
   every target is compared against the bytes the plan was rendered
   against. If anything drifted in between, the apply is aborted with an
   optimistic-concurrency error; no file is touched.
3. **Atomic write with rollback**. Each file is written through a
   sibling temp and `rename`d in place (atomic on POSIX). If any step
   fails, files already renamed are restored from in-memory backups so
   the workspace never ends up half-patched.

When all three gates pass, a JSON commit entry is written to
`<root>/.prolog-forge/journal/<commit_id>.json` with the before/after
bytes of every changed file.

### Roll a commit back

```bash
$ pf rollback examples/rust-demo commit-18a8a527b73cc155
rolled back: commit commit-18a8a527b73cc155 (rename add -> sum), 1 file(s) restored
```

`patch.rollback` refuses if the on-disk content no longer matches what
was written at commit time (someone hand-edited the file — rollback is
not automatic conflict resolution). On success it uses the same atomic
write path as `apply`, then deletes the journal entry. Single-commit
rollback only at this stage; a redo stack and cross-commit undo come
later.

### Rule-pack apply-gate

A workspace whose `rules.load`ed pack defines `violation(...)` rules
turns every `patch.apply` into a guarded operation: the shadow source
is re-analyzed, the graph is rebuilt, the rules are re-evaluated, and
any derived `violation/*` fact rejects the apply with diagnostics. See
[`docs/rules-dsl.md`](docs/rules-dsl.md#the-violation1-convention-apply-gate).

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
| `workspace.index` | Walk the workspace, analyze every supported source file, emit observed facts. |
| `workspace.status` | Counts of facts, rules, derived. |
| `graph.ingestFact` | Insert facts programmatically. |
| `graph.query` | Pattern-match an atom against the graph. |
| `rules.load` | Parse and register a Datalog source block. |
| `rules.evaluate` | Run the engine to fixpoint. |
| `llm.propose` | Bounded LLM proposal → candidate facts with identifier resolution. |
| `llm.refine` | Iterative refinement loop: rejections + diagnostics fed back to the model round after round until convergence or `max_rounds`. |
| `llm.propose_patch` | Bounded LLM proposal of *typed `PatchPlan`s* grounded against the op vocabulary. Same wire shape as `patch.preview` / `explain.patch` — no translation step. |
| `patch.preview` | Simulate a typed `PatchPlan` against the workspace, return per-file unified diffs. FS untouched. |
| `patch.apply` | Validate + preflight + atomic write + journal. Returns `{applied, commit_id, validation, rejection_reason}`. |
| `patch.rollback` | Restore a committed patch. Preflight + atomic restore + journal delete. |
| `explain.patch` | Proof-carrying explanation for a plan: observed facts cited, rule activations with premises, candidates considered, validation stages, verdict. FS untouched. |

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
- Web proof-tree renderer (the `explain.patch` JSON is shipped; the UI that visualizes it is **Phase 2**).
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
| **1.1** | `pf-ingest`, `pf-lang-rust`, `workspace.index`, CSM→fact lowering, real-project demo | **shipped** |
| **1.2** | `pf-llm`, `llm.propose`, context from trusted layers only, schema-validated output, anti-hallucination guard | **shipped** |
| **1.3** | `pf-patch`, `patch.preview`, typed ops, byte-accurate Rust rename, unified diffs, `pf rename` CLI | **shipped** |
| **1.4** | `pf-validate` (pluggable stages), `patch.apply` with preflight + atomic write + rollback, `pf rename --apply` | **shipped (syntactic stage)** |
| **1.5** | `RuleStage` (`violation/*` apply-gate), disk-persistent commit journal, `patch.rollback`, `pf rollback` CLI | **shipped** |
| **1.6** | `pf-explain` + `explain.patch` (proof-carrying patches), `llm.refine` (iterative `candidate → diagnostics → revised candidate` loop), `pf explain` / `pf refine` CLI | **shipped** |
| **1.7** | `CargoCheckStage` (type-aware validation profile: shadow copy + `cargo check` + JSON-parsed diagnostics), `validation_profile = "typed"` on `patch.apply` / `explain.patch`, `pf rename --typecheck` | **shipped** |
| **1.8** | `CargoTestStage` (behavioral validation profile: shadow copy + `cargo test --no-fail-fast` + libtest-line parsing → one diagnostic per failing test), `validation_profile = "tested"`, `pf rename --run-tests` | **shipped** |
| **1.9** | `llm.propose_patch` (LLM emits typed `PatchPlan`s instead of fact candidates; op-registry + identifier grounding on every candidate), `pf propose-patch` CLI chains *propose → explain.patch* end-to-end | **shipped** |
| **1.10** | Macro-aware rename (Step 1 of type-aware): `syn` visitor descends into every function-call macro's token stream, skips `macro_rules!` meta-var grammar, uses a `$`-adjacency guard. Eliminates the most visible Step-0 bug (`assert_eq!(add(1,2), 3)` now renames `add`). | **shipped** |
| **1.11** | Scope-resolved rename (Step 2 of type-aware): new `pf-ra-client` crate (LSP client + in-process mock), new `PatchOp::RenameFunctionTyped` variant, `pf rename --scope-resolved` CLI flag. Graceful degradation when rust-analyzer isn't on `PATH`. | **shipped** |
| 1.12 (MVP rest) | Persistent rust-analyzer session (avoid re-indexing per request), impacted-tests selection, VS Code adapter minimal | 2–3 months |
| 2 | Multi-language (TS, Python), property-based validation, Emacs/Neovim, web explainer UI (renders `explain.patch` output) | 5–8 months |
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
