# Prolog Forge — VS Code adapter

A thin VS Code client for the `pf-daemon` JSON-RPC server. Speaks the
same protocol the CLI speaks; doesn't bundle the daemon.

## What's here

Six commands, all namespaced under **Prolog Forge** in the command
palette (`Cmd/Ctrl+Shift+P`):

| Command | What it does |
|---|---|
| **Rename Function** | Prompts for old/new name, shows the `patch.preview` diff in the output channel, asks for confirmation, runs `patch.apply` — transactional with full validation gate. |
| **Propose Patch (LLM)** | Quick-picks an anchor function from the graph, asks for an intent (free text) and a memory depth, calls `llm.propose_patch`, then for each accepted candidate offers **Explain** (proof-carrying `explain.patch`) or **Apply** (preview → confirm → validated apply) — with a chosen validation profile (`default` / `typed` / `tested`). Phase 1.20. |
| **Explain Rename** | Dry-run of a rename through `explain.patch`: picks a profile, runs the explainer against the plan (no file writes), prints the three-state verdict (`accepted` / `rejected` / `not_proven`) with evidence counts and summary. Phase 1.20. |
| **Show History** | `memory.history` → per-commit metadata (op tags, profile, file count) newest-first. |
| **Show Stats** | `memory.stats` → aggregates by op kind, by validation profile, top-N edited files. |
| **Daemon Info** | `session.initialize` — protocol version + list of methods the current daemon advertises. Handy for confirming the adapter is talking to the daemon you expect. |

All output lands in the **Prolog Forge** output channel (View →
Output → select "Prolog Forge"). Diagnostics from `patch.apply`
validation failures are surfaced verbatim so a rejected rename
explains itself.

On activation the extension opens the first workspace folder
(`workspace.open`) **and** runs `workspace.index` once so the LLM /
explain commands can discover function entities through `graph.query`
without a user-visible round-trip. Index failures don't block the
byte-level ops (rename, etc.) which don't need the graph.

## Install (development mode)

```bash
# 1. Build the daemon.
cargo build --bin pf-daemon

# 2. Launch VS Code with this extension loaded from source.
#    No `npm install` needed — the extension uses only Node built-ins
#    and the VS Code host API.
code --extensionDevelopmentPath="$(pwd)/adapters/vscode" <path/to/a/workspace>

# 3. Point the extension at the daemon you just built.
#    Settings → search "Prolog Forge" → Daemon Path.
#    Default: `pf-daemon` (PATH lookup). For a dev build set, e.g.:
#      "<abs-path>/Prolog-Forge/target/debug/pf-daemon"
```

The extension activates on VS Code startup, spawns the daemon, sends
`session.initialize`, and opens the first workspace folder via
`workspace.open`. If any step fails you'll see a notification; the
output channel always has the full trace.

## Settings

- `prologForge.daemonPath` (string, default `pf-daemon`): absolute
  path or PATH-lookup name for the daemon binary.
- `prologForge.requestTimeoutMs` (number, default `30000`): per-RPC
  wall-clock cap. Bump it if `cargo check` under the `typed`
  validation profile times out on larger workspaces.

## Protocol fidelity

The adapter speaks the same JSON-RPC 2.0 shape with LSP-style
Content-Length framing that the daemon smoke test uses. It imports
nothing from the core crates — the wire surface in
`schemas/protocol.json` is the contract. Upgrading the daemon to a
newer protocol version (as long as the new methods are additive)
doesn't require any adapter change beyond exposing them as commands.

## Limits

- One workspace per extension activation: only the first
  `workspaceFolders[0]` is opened. Multi-root workspaces are a
  follow-up.
- No streaming output: `patch.preview` and `cargo test` results
  come back all at once at the end of the RPC. The daemon's own
  `$/progress` surface is reserved for a future phase.
- LLM orchestrator surface exposed: `llm.propose_patch` (Phase 1.20
  **Propose Patch**) and `explain.patch` (both **Propose Patch** and
  **Explain Rename**). `llm.refine`'s iterative dialogue has no
  dedicated command yet — it needs a multi-round UI which is a
  follow-up.

## Troubleshooting

- **"Failed to start pf-daemon"**: the binary isn't on PATH, or the
  absolute path in the setting is wrong. Run
  `<abs-path-to-pf-daemon> --help` in a terminal to confirm the
  binary runs standalone.
- **"request ... timed out"**: `prologForge.requestTimeoutMs` is too
  low for the operation at hand (typically `cargo test` under the
  `tested` profile). Bump it in settings.
- **Rename seems to touch nothing**: inspect the diff in the output
  channel. The current `rename_function` op is macro-aware (Phase
  1.10) but not scope-resolved; a shadowed local of the same name
  would also be renamed. For scope-correct renames, build the
  daemon with rust-analyzer support and use the
  `rename_function_typed` op variant from the CLI — VS Code surface
  for that variant is a follow-up.
