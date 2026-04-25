# AYE-AYE

> **A**nalysis **Y**ielding **E**videntiary **A**ssertions **Y**ielding
> **E**xplanations.

**A neuro-symbolic development runtime with a proof-carrying core.**
Editor-agnostic. Rust. JSON-RPC 2.0. Apache-2.0.

AYE-AYE is **not** a VS Code plugin. It is a headless daemon that
ingests a repository, lowers source files to a knowledge graph, runs a
Datalog rule engine over that graph, and orchestrates LLMs inside that
structured frame so every candidate patch arrives with a chain of
evidence — observed facts, rule activations, validation diagnostics,
and a synthesized verdict.

Editors, CLIs, CI systems, and autonomous agents are all **thin clients**
of the same local protocol. The core never imports an editor SDK.

> Status: **Phase 1, step 23** — `ChangeSignature` op:
> reorder a free-standing function's parameters and
> optionally rename them, propagating the permutation to
> every bare call site. Permutation-only: adding or removing
> params is refused (separate ops for those, future phases).
> Renames are scope-aware syntactically — refused when the
> body shadows the old name or already binds the new name.
> Narrow contract refuses generic / `async` / `const` /
> `unsafe` / `self`-taking functions, macro-body call
> sites, qualified-path call sites (`mod::f(...)`), and any
> non-bare reference that would silently desync. Wire
> shape: `PatchOp::ChangeSignature { function, new_params,
> files }` with `ParamReorder { from_index, rename }`,
> recognised by `patch.preview` / `patch.apply` /
> `explain.patch` / `llm.propose_patch`. New `aa
> change-signature --order '1,0' --rename '0=left'` CLI.
> Journal tracks the `change_signature` op tag (visible
> via `memory.stats by_op_kind`). 16 unit tests cover the
> happy paths and the refusal cases; daemon smoke exercises
> preview → apply → arity-refusal end-to-end. See
> [Roadmap](#roadmap).

---

## What the name says

**AYE-AYE** = **A**nalysis **Y**ielding **E**videntiary **A**ssertions
**Y**ielding **E**xplanations.

The runtime is a two-step yield,
symbolic then neural, both feeding the same explainer.

See [`docs/rules-dsl.md`](docs/rules-dsl.md). 

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
| [`aa-protocol`](crates/aa-protocol) | JSON-RPC types, LSP-style framing, API contract |
| [`aa-csm`](crates/aa-csm) | Common Semantic Model (v0) |
| [`aa-graph`](crates/aa-graph) | In-memory knowledge graph (facts, layers, pattern matching) |
| [`aa-rules`](crates/aa-rules) | Datalog-v1 parser + evaluator |
| [`aa-persist`](crates/aa-persist) | KV trait + in-memory backend |
| [`aa-ingest`](crates/aa-ingest) | Filesystem walker and source dispatch |
| [`aa-lang-rust`](crates/aa-lang-rust) | Rust analyzer (syn-based) emitting CSM fragments |
| [`aa-llm`](crates/aa-llm) | Bounded LLM orchestrator: provider trait, mock provider, trusted-only context, schema-validated I/O, response cache, anti-hallucination guard, one-shot `propose` + iterative `refine` loop |
| [`aa-patch`](crates/aa-patch) | Typed patch ops, `PatchPlan`, pure preview pipeline with byte-accurate `syn`-driven span edits. Op vocabulary: `RenameFunction` (Phase 1.10), `RenameFunctionTyped` (1.11), `AddDeriveToStruct` (1.12), `RemoveDeriveFromStruct` (1.18), `InlineFunction` (1.21), `ExtractFunction` (1.22), `ChangeSignature` (1.23) |
| [`aa-ra-client`](crates/aa-ra-client) | Minimal LSP client for `rust-analyzer`: Content-Length framing, `Client` (one-shot) + `Session` (persistent tempdir + versioned `didChange` sync), in-process mock server for tests. Powers Step 2 scope-resolved rename + the Phase 1.13 session pool. |
| [`aa-validate`](crates/aa-validate) | Pluggable validation pipeline: `ValidationStage` trait, `SyntacticStage`, fail-fast `Pipeline`. Semantic stages (`RuleStage`, `CargoCheckStage`) live in `aa-core`. |
| [`aa-explain`](crates/aa-explain) | Proof-carrying explainer: composes observed / inferred / candidate evidence + rule activations + validation stages into a single `Explanation` with a synthesized verdict |
| [`aa-core`](crates/aa-core) | Session manager + API dispatcher + indexing pipeline + `llm.propose` + `llm.refine` + `patch.preview` + `patch.apply` + `explain.patch` |
| [`aa-daemon`](crates/aa-daemon) | Binary: stdio JSON-RPC server |
| [`aa-cli`](crates/aa-cli) | Binary: reference adapter + CI tool (`aa`) |

Adapters (VS Code, Emacs, Neovim, JetBrains, …) live in a separate
`adapters/` tree, intentionally kept thin. A VS Code client shipped
in Phase 1.19 and was extended in 1.20 with LLM-driven commands — see
[`adapters/vscode`](adapters/vscode) — and is the reference
implementation of the protocol for any future editor integration.

---

## Quick start

### Build

```bash
cargo build --workspace
cargo test --workspace
```

You get two binaries:

- `target/debug/aa` — the reference CLI.
- `target/debug/aa-daemon` — the stdio JSON-RPC server.

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
$ aa run family.pfr
derived: 6
iterations: 4

$ aa query family.pfr 'ancestor(alice, X)'
3 result(s)
  {"X":"bob"}
  {"X":"carol"}
  {"X":"dan"}
```

### Index a real Rust project

```bash
$ aa index examples/rust-demo \
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
$ aa propose examples/rust-demo \
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
$ aa refine examples/rust-demo \
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

### Full neuro-symbolic loop: `aa propose-patch`

```bash
$ aa propose-patch examples/rust-demo \
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
$ aa explain examples/rust-demo --from add --to sum --verbose
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
$ aa rename examples/rust-demo --from add --to sum
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
$ aa rename examples/rust-demo --from add --to sum --apply
# …preview diff…
applied: commit commit-18a8a3f598a1c2e1 (1 file(s), 531 bytes)
```

Every `patch.apply` goes through three gates:

1. **Validation pipeline** (`aa-validate`). The default profile runs
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
`<root>/.aye-aye/journal/<commit_id>.json` with the before/after
bytes of every changed file.

### Roll a commit back

```bash
$ aa rollback examples/rust-demo commit-18a8a527b73cc155
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
cargo run -q --bin aa-daemon
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
Rust wire types in `aa-protocol` are kept in sync with that file by hand in
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
- `aa-daemon` headless binary, stdio JSON-RPC.
- `aa` reference CLI (`info`, `check`, `run`, `query`).
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
| **1.1** | `aa-ingest`, `aa-lang-rust`, `workspace.index`, CSM→fact lowering, real-project demo | **shipped** |
| **1.2** | `aa-llm`, `llm.propose`, context from trusted layers only, schema-validated output, anti-hallucination guard | **shipped** |
| **1.3** | `aa-patch`, `patch.preview`, typed ops, byte-accurate Rust rename, unified diffs, `aa rename` CLI | **shipped** |
| **1.4** | `aa-validate` (pluggable stages), `patch.apply` with preflight + atomic write + rollback, `aa rename --apply` | **shipped (syntactic stage)** |
| **1.5** | `RuleStage` (`violation/*` apply-gate), disk-persistent commit journal, `patch.rollback`, `aa rollback` CLI | **shipped** |
| **1.6** | `aa-explain` + `explain.patch` (proof-carrying patches), `llm.refine` (iterative `candidate → diagnostics → revised candidate` loop), `aa explain` / `aa refine` CLI | **shipped** |
| **1.7** | `CargoCheckStage` (type-aware validation profile: shadow copy + `cargo check` + JSON-parsed diagnostics), `validation_profile = "typed"` on `patch.apply` / `explain.patch`, `aa rename --typecheck` | **shipped** |
| **1.8** | `CargoTestStage` (behavioral validation profile: shadow copy + `cargo test --no-fail-fast` + libtest-line parsing → one diagnostic per failing test), `validation_profile = "tested"`, `aa rename --run-tests` | **shipped** |
| **1.9** | `llm.propose_patch` (LLM emits typed `PatchPlan`s instead of fact candidates; op-registry + identifier grounding on every candidate), `aa propose-patch` CLI chains *propose → explain.patch* end-to-end | **shipped** |
| **1.10** | Macro-aware rename (Step 1 of type-aware): `syn` visitor descends into every function-call macro's token stream, skips `macro_rules!` meta-var grammar, uses a `$`-adjacency guard. Eliminates the most visible Step-0 bug (`assert_eq!(add(1,2), 3)` now renames `add`). | **shipped** |
| **1.11** | Scope-resolved rename (Step 2 of type-aware): new `aa-ra-client` crate (LSP client + in-process mock), new `PatchOp::RenameFunctionTyped` variant, `aa rename --scope-resolved` CLI flag. Graceful degradation when rust-analyzer isn't on `PATH`. | **shipped** |
| **1.12** | First non-rename op: `PatchOp::AddDeriveToStruct`. Syn-based merge-or-insert of `#[derive(...)]` on struct/enum/union, idempotent, grounded by `struct_def`/`enum_def`/`union_def`/`type_def` facts. MockProvider emits `add_derive` candidates; `llm.propose_patch` validator recognises the op. `aa add-derive` CLI. | **shipped** |
| **1.13** | Persistent rust-analyzer session pool: `aa-ra-client::Session` (tempdir + versioned `didChange` sync), `aa-core::RaSessionPool` keyed by workspace root, `aa-patch::TypedRenameResolver` trait + `OneShotResolver` fallback. Successive typed renames share one warm RA instead of paying the indexing cost each call. | **shipped** |
| **1.14** | Repo memory surface: `memory.history` / `memory.get` / `memory.stats` methods, enriched `CommitEntry` (op tags, validation profile, replacement count), `aa history` / `aa show` / `aa stats` CLI. | **shipped** |
| **1.15** | Memory-biased LLM proposer: `llm.propose_patch` `include_memory: N` field, `patch_propose.v2` prompt variant with `Prior successes:` block, `MemoryHint` plumbing through aa-core to the orchestrator, `aa propose-patch --include-memory N` CLI. | **shipped** |
| **1.16** | Impacted-tests selection (direct): `aa-core::test_impact` scans the workspace with `syn` + macro-aware walker, returns `#[test]`-annotated fns whose body mentions any plan anchor. `CargoTestStage.with_selection(names)` feeds them to `cargo test` as a substring filter; empty selection falls back to full suite. First graph-driven runtime decision. | **shipped** |
| **1.17** | Transitive test-impact: same module now builds a per-function ident catalog and walks it with a cycle-safe BFS, so `test_X → helper Y → anchor Z` is picked up (the `double_uses_add → double → add` case direct impact missed). Same wire shape; pure narrowing upgrade. | **shipped** |
| **1.18** | `PatchOp::RemoveDeriveFromStruct`: dual of Phase 1.12's add-op. Filters listed derives; when the list empties, strips the whole `#[derive(...)]` attribute line. `add → remove` round-trips byte-for-byte. `aa remove-derive` CLI. | **shipped** |
| **1.19** | VS Code adapter minimal: `adapters/vscode/` pure-JS extension (no `npm install` step), JSON-RPC client speaking the daemon's stdio protocol, four commands (Rename Function, Show History, Show Stats, Daemon Info). First non-CLI client of the protocol. | **shipped** |
| **1.20** | LLM-driven VS Code commands: **Propose Patch (LLM)** — function quick-pick → intent → memory-depth → `llm.propose_patch` → per-candidate **Apply** (preview + validated apply) or **Explain** (`explain.patch`) with a chosen profile (`default` / `typed` / `tested`). **Explain Rename** — `explain.patch` dry-run with full verdict + stats. Auto-`workspace.index` on activation so `graph.query` resolves function entities. Closes the editor ↔ neuro-symbolic loop end-to-end. | **shipped** |
| **1.21** | `PatchOp::InlineFunction` — first op that *deletes* code as well as rewriting it. Substitutes every bare call site `f(a1, a2)` with `({ let p1 = a1; let p2 = a2; <body_inner> })` (paren-wrap defeats the statement-disambiguation rule `{…} + 1 → {…}; +1`), then removes the fn definition. Narrow contract refuses recursion, `return`, `async`/`const`/`unsafe`, generics, `self`, macro-body call sites, and any non-bare reference in scope. `aa inline-function` CLI subcommand. MockProvider validator + `explain.patch` anchors wired; `memory.stats by_op_kind.inline_function` tracked. Shared span-arithmetic helpers extracted to `aa-patch::util`. | **shipped** |
| **1.22** | `PatchOp::ExtractFunction` — dual of 1.21. Lift a contiguous run of stmts out of a free-standing fn body into a new helper; replace the original site with a call. Selection by 1-indexed inclusive line range; helper params listed explicitly as `(name, type)` pairs (no type inference — that's a later-phase RA job). Refuses `return`/`break`/`continue`/`?`/`await`/`yield`, macro invocations, partial-statement selections, selections ending on the parent's tail expression, and `async`/`const`/`unsafe`/generic/`self`-taking parents. `aa extract-function` CLI subcommand. `memory.stats by_op_kind.extract_function` tracked. | **shipped** |
| **1.23** | `PatchOp::ChangeSignature` — reorder a free-standing function's parameters, optionally renaming them, and propagate the permutation to every bare call site. Permutation-only: arity changes refused (additions/removals get separate ops in later phases). Renames are syn-driven and refuse to proceed when the body would shadow the old name or already binds the new name. Same refusal posture as Inline/Extract for shape (no generics / `async` / `const` / `unsafe` / `self`), call sites (no macro-body, no qualified path), and non-bare references that would silently desync. `aa change-signature --order '1,0' --rename '0=left'` CLI. `memory.stats by_op_kind.change_signature` tracked. | **shipped** |
| 1.24 (MVP rest) | More editing ops (move-item, add/remove param), multi-language analyzers (TS / Python), dedicated `llm.refine` UI (multi-round dialogue) | 2–3 months |
| 2 | Multi-language (TS, Python), property-based validation, Emacs/Neovim, web explainer UI (renders `explain.patch` output) | 5–8 months |
| 3 | Pattern mining, rule marketplace, provenance export, candidate → validated workflow | 8–12 months |
| 4 | Agent mode, ML-assisted validation, cross-machine incrementality, gRPC transport | 12–18 months |
| 5 | Third-party analyzers SDK, vertical rule packs, CI/CD-native integrations | 18+ months |

---

## Repository layout

```
aye-aye/
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
│   ├── aa-protocol/
│   ├── aa-csm/
│   ├── aa-graph/
│   ├── aa-rules/
│   ├── aa-persist/
│   ├── aa-core/
│   ├── aa-daemon/
│   └── aa-cli/
└── tooling/
    └── smoke/
        └── daemon_smoke.py
```

Adapters live outside `crates/` (e.g. `adapters/vscode/`, `adapters/emacs/`)
and are added alongside their phase.

---

## Releases

Pre-built binaries (`aa` + `aa-daemon`) and the VS Code `.vsix` are
published to [GitHub Releases](https://github.com/maribakulj/AYE-AYE/releases)
on every annotated `v*` tag. Targets: `x86_64-linux-gnu`,
`aarch64-linux-gnu`, `x86_64-darwin`, `aarch64-darwin`. Each release
ships `SHA256SUMS` for verification.

Quick install (linux x86_64):

```bash
curl -L https://github.com/maribakulj/AYE-AYE/releases/latest/download/aa-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz
./aa-x86_64-unknown-linux-gnu/aa --version
```

Cutting a release is documented in
[`docs/RELEASING.md`](docs/RELEASING.md).

---

## Contributing

This is a research-grade codebase aiming at production-grade foundations.
Three contracts are *intentionally rigid* past Phase 0 and breaking them will
be challenged hard in review:

1. the **CSM** shape,
2. the **graph schema** (predicates + layer semantics),
3. the **protocol** (methods, param shapes, error codes).

Everything else is substitutable. Before opening a PR, run the
preflight script — it's a bit-for-bit local mirror of
[`.github/workflows/ci.yml`](.github/workflows/ci.yml)'s blocking
jobs and is the single source of truth for "what CI checks":

```bash
tooling/preflight.sh           # mandatory checks (mirrors `build-and-test`)
tooling/preflight.sh --full    # additionally: audit, deny, MSRV, RA e2e
```

Mandatory checks: `cargo fmt --check`, `cargo clippy -D warnings`,
`cargo build`, `cargo test`, JSON schema parse, daemon smoke,
VS Code adapter syntax-check. The `--full` mode adds the
supplementary CI jobs (`cargo audit`, `cargo deny`, MSRV build on
Rust 1.85, and the rust-analyzer e2e test) and self-skips the
ones whose binaries are absent on the host with a clear message.

Any time CI grows a new step, mirror it in `tooling/preflight.sh`
in the same PR. The contract is "anything not in `preflight.sh`
is not on the merge gate."

Issues and PRs are welcome on
[GitHub](https://github.com/maribakulj/AYE-AYE).

---

## License

Apache-2.0. See [`LICENSE`](LICENSE).
