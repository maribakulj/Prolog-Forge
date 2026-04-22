# Protocol — Prolog Forge Core

**Version:** `0.10.0` (Phase 1 step 11, pre-stable).

The Core is a JSON-RPC 2.0 server. Adapters (CLI, VS Code, Emacs, …) are
clients. Nothing else should live in an adapter.

## Transport

- **Default:** stdio, with LSP-style Content-Length framing.
- **Planned:** local Unix socket / TCP localhost, same framing. Authentication
  via a token file (`~/.prologforge/auth`, mode `0600`). gRPC as secondary
  transport in a later phase.

Frame format:

```
Content-Length: <N>\r\n
\r\n
<N bytes of UTF-8 JSON>
```

Other headers are tolerated and ignored. Log output from the server **must**
go to stderr — stdout is reserved for the protocol.

## Versioning

The protocol is semver. A MAJOR bump breaks wire compatibility; MINOR adds
methods or optional fields. Clients send their own name/version during
`session.initialize`; the server advertises its protocol version and the list
of supported methods. Capability negotiation (for optional features) is
server-driven from that list.

## Methods — current surface

| Method | Purpose |
|---|---|
| `session.initialize` | Handshake; returns `ServerCapabilities`. |
| `session.shutdown` | Terminate the daemon cleanly. |
| `workspace.open` | Register a workspace root; returns a `WorkspaceId`. |
| `workspace.index` | Walk the workspace, analyze every supported source file, emit observed facts. |
| `workspace.status` | Counts of facts / rules / derived facts. |
| `graph.ingestFact` | Insert facts into the knowledge graph. |
| `graph.query` | Pattern-match one atom against the graph. |
| `rules.load` | Parse a Datalog source block; registers rules and seed facts. |
| `rules.evaluate` | Run the rule engine to fixpoint; returns `{derived, iterations}`. |
| `llm.propose` | Ask the bounded LLM orchestrator for candidate facts anchored at an entity; every proposal is identifier-resolved against the graph before insertion at the `candidate` layer. |
| `llm.refine` | Iterative revision loop. Accepts prior rejections and validator diagnostics, runs up to `max_rounds` of `refine.v1` prompts, and returns every candidate tagged with its round. Converges early when a round produces zero rejections. |
| `llm.propose_patch` | Ask the LLM orchestrator for *typed patch plans* rather than fact candidates. Each candidate is an `ops + label` plan in the same wire shape `patch.preview` / `patch.apply` / `explain.patch` accept. Every op is identifier-grounded against the graph and rejected with a structured reason on hallucination. Closes the LLM → symbolic loop end-to-end: no translation step between proposal and validation. |
| `patch.preview` | Simulate a typed patch plan against the workspace's source files. Returns a unified diff per changed file plus replacement counts. Does not touch the filesystem. |
| `patch.apply` | Validate the plan (pluggable stage pipeline selected by `validation_profile` — see below) and, if every stage is green, write the shadow state to disk transactionally (preflight → temp files → atomic rename → rollback on failure) and record a commit entry to the on-disk journal. |
| `patch.rollback` | Undo a previously applied commit by id. Preflight-checks that the on-disk content still matches what was written at commit time, then atomically restores the pre-commit bytes from the journal. |
| `explain.patch` | Build a proof-carrying explanation for a typed plan: observed facts cited, rule activations (head + premises), candidates considered, validation stages + diagnostics, and a synthesized verdict (`accepted` / `rejected` / `not_proven`). Pure — reads the graph, does not touch the filesystem. |

## Validation profiles

`patch.apply` and `explain.patch` accept an optional `validation_profile`
field that selects which stages run against the shadow file set. The
profile names are part of the wire contract; unknown names are rejected
with `invalid_params`.

| Profile | Stages | When to use |
|---|---|---|
| `default` (or missing) | `syntactic`; `rules` when rule pack loaded | Fast default. Catches broken syntax and any `violation/*` fact derivable from the rule pack. |
| `typed` | everything in `default` + `cargo_check` | Runs `cargo check --message-format=json` against a temp shadow of the workspace. Upgrades the explainer's verdict from `not_proven` to `accepted` when green. Requires `cargo` on `PATH`; passes with a warning diagnostic when it isn't (the stage is an oracle, not a hard gate). Slower — opt in per apply. |
| `tested` | everything in `typed` + `cargo_test` | Additionally runs `cargo test --no-fail-fast` against the shadow and parses the runner's stable `test X ... FAILED` lines into one diagnostic per failing test. Strongest behavioral gate; substantially slower (full test compilation + run). Feeds structured failure names into `llm.refine` as prior diagnostics. |

Typed JSON Schemas live in [`schemas/protocol.json`](../schemas/protocol.json)
and are the source of truth. The Rust types in `pf-protocol` are expected to
stay in sync with that file; a schema-first codegen is on the Phase 1
roadmap.

## Epistemic layers

Every fact carries a `layer` field, strictly disjoint:

- `observed` — direct output of an analyzer.
- `inferred` — derived by a validated rule from observed facts.
- `candidate` — hypothesized by a miner or LLM; not trusted.
- `validated` — a candidate that has been promoted by a human.
- `constraint` — a hard invariant whose violation raises a diagnostic.

In Phase 0 only `observed` and `inferred` appear. `candidate` / `validated` /
`constraint` arrive in Phase 3 with the rule marketplace.

## Errors

Standard JSON-RPC error codes are used, with a Core-specific layering:

| Code | Meaning |
|---|---|
| -32700 | Parse error (malformed JSON). |
| -32600 | Invalid request. |
| -32601 | Method not found. |
| -32602 | Invalid params (includes decode errors, unknown workspace, etc.). |
| -32603 | Internal error (evaluator failure, etc.). |

Core-reserved codes live in the range `-32000..=-32099` and will be documented
as they land.

## Notifications

Phase 0 does not emit notifications. The server-initiated channel is reserved
for:

- `workspace/didChange` — indexing delta ready.
- `validation/didComplete` — a validation pipeline finished.
- `$/progress` — streaming partial results for long requests.

## Cancellation

Reserved method `$/cancelRequest` takes `{ id }`. Phase 0 does not yet honor
cancellation; long-running requests (none in Phase 0) will become cancellable
in Phase 1.
