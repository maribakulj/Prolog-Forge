# Protocol — Prolog Forge Core

**Version:** `0.5.0` (Phase 1 step 4, pre-stable).

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
| `patch.preview` | Simulate a typed patch plan against the workspace's source files. Returns a unified diff per changed file plus replacement counts. Does not touch the filesystem. |
| `patch.apply` | Validate the plan (pluggable stage pipeline) and, if every stage is green, write the shadow state to disk transactionally (preflight → temp files → atomic rename → rollback on failure). |

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
