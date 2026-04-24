// VS Code extension entry point.
//
// On activation the extension:
// 1. Spawns the pf-daemon binary (configurable via `prologForge.daemonPath`).
// 2. Runs `session.initialize` and surfaces the server capabilities in the
//    output channel.
// 3. Opens the first workspace folder via `workspace.open`.
// 4. Registers four commands (Rename, History, Stats, Info) that compose
//    JSON-RPC calls against the daemon.
//
// No external npm dependencies: the extension loads directly from source
// (`code --extensionDevelopmentPath=adapters/vscode`). Only Node built-ins
// plus the `vscode` module provided by the host are required.

'use strict';

const vscode = require('vscode');
const { Client } = require('./client');

/** @type {Client|null} */
let client = null;
/** @type {vscode.OutputChannel|null} */
let output = null;
/** @type {string|null} Workspace id returned by `workspace.open`. */
let workspaceId = null;
/** @type {string|null} Absolute path of the opened workspace. */
let workspaceRoot = null;

/**
 * @param {vscode.ExtensionContext} context
 */
async function activate(context) {
  output = vscode.window.createOutputChannel('Prolog Forge');
  output.appendLine('[pf] activating');
  const cfg = vscode.workspace.getConfiguration('prologForge');
  const daemonPath = cfg.get('daemonPath', 'pf-daemon');
  const timeoutMs = cfg.get('requestTimeoutMs', 30000);

  client = new Client(daemonPath, timeoutMs, (line) => {
    if (output) output.appendLine(line);
  });

  try {
    const caps = await client.start();
    output.appendLine(
      `[pf] connected: ${caps.name} ${caps.version} protocol=${caps.protocol_version}`,
    );
  } catch (e) {
    const msg =
      `Failed to start pf-daemon (${daemonPath}). ` +
      `Set \`prologForge.daemonPath\` to an absolute path, or put pf-daemon on PATH. ` +
      `Error: ${e.message}`;
    vscode.window.showErrorMessage(msg);
    if (output) output.appendLine(`[pf] ${msg}`);
    return;
  }

  const folders = vscode.workspace.workspaceFolders;
  if (folders && folders.length > 0) {
    workspaceRoot = folders[0].uri.fsPath;
    try {
      const r = await client.request('workspace.open', { root: workspaceRoot });
      workspaceId = r.workspace_id;
      output.appendLine(`[pf] workspace opened: ${workspaceId} @ ${workspaceRoot}`);
    } catch (e) {
      output.appendLine(`[pf] workspace.open failed: ${e.message}`);
    }
    // Index eagerly so LLM / explain commands see `function(...)` facts
    // without a second user-visible round-trip. Failures are non-fatal:
    // byte-level ops (rename, remove_derive) don't need the graph.
    if (workspaceId) {
      try {
        const r = await client.request('workspace.index', {
          workspace_id: workspaceId,
        });
        output.appendLine(
          `[pf] indexed: ${r.files_indexed} file(s), ${r.entities} entities, ` +
            `${r.facts_inserted} facts`,
        );
      } catch (e) {
        output.appendLine(`[pf] workspace.index failed: ${e.message}`);
      }
    }
  } else {
    output.appendLine('[pf] no workspace folder — commands will ask to open one');
  }

  context.subscriptions.push(
    vscode.commands.registerCommand('prologForge.renameFunction', cmdRename),
    vscode.commands.registerCommand('prologForge.proposePatch', cmdProposePatch),
    vscode.commands.registerCommand('prologForge.explainRename', cmdExplainRename),
    vscode.commands.registerCommand('prologForge.showHistory', cmdHistory),
    vscode.commands.registerCommand('prologForge.showStats', cmdStats),
    vscode.commands.registerCommand('prologForge.info', cmdInfo),
    output,
    // Reap the daemon when the extension unloads.
    { dispose: () => client && client.shutdown() },
  );
}

async function requireWorkspace() {
  if (!client) {
    vscode.window.showErrorMessage('Prolog Forge: daemon is not running');
    return null;
  }
  if (!workspaceId) {
    vscode.window.showErrorMessage(
      'Prolog Forge: no workspace opened (open a folder in VS Code first)',
    );
    return null;
  }
  return { client, workspaceId };
}

// -------- Rename Function -------------------------------------------------

async function cmdRename() {
  const ctx = await requireWorkspace();
  if (!ctx) return;

  const from = await vscode.window.showInputBox({
    prompt: 'Function name to rename',
    placeHolder: 'add',
    validateInput: (s) =>
      /^[A-Za-z_][A-Za-z0-9_]*$/.test(s) ? null : 'must be a valid Rust identifier',
  });
  if (!from) return;
  const to = await vscode.window.showInputBox({
    prompt: `New name for "${from}"`,
    validateInput: (s) =>
      /^[A-Za-z_][A-Za-z0-9_]*$/.test(s) ? null : 'must be a valid Rust identifier',
  });
  if (!to) return;

  const plan = {
    ops: [{ op: 'rename_function', old_name: from, new_name: to, files: [] }],
    label: `rename ${from} -> ${to}`,
  };

  output.show(true);
  output.appendLine(`\n[pf] preview: rename ${from} -> ${to}`);

  let preview;
  try {
    preview = await ctx.client.request('patch.preview', {
      workspace_id: ctx.workspaceId,
      plan,
    });
  } catch (e) {
    vscode.window.showErrorMessage(`preview failed: ${e.message}`);
    return;
  }
  output.appendLine(
    `[pf] ${preview.total_replacements} replacement(s) across ${preview.files.length} file(s)`,
  );
  for (const err of preview.errors) {
    output.appendLine(`[pf] error in ${err.file}: ${err.message}`);
  }
  for (const f of preview.files) {
    output.appendLine(
      `\n# ${f.path} (${f.before_len} -> ${f.after_len} bytes, ${f.replacements} repl)`,
    );
    output.appendLine(f.diff);
  }
  if (preview.total_replacements === 0) {
    vscode.window.showInformationMessage(
      `Prolog Forge: no occurrences of "${from}" to rename`,
    );
    return;
  }

  const choice = await vscode.window.showQuickPick(
    [
      { label: 'Apply', description: 'write the patch to disk (transactional)' },
      { label: 'Cancel', description: 'discard the preview' },
    ],
    { placeHolder: `Apply rename ${from} -> ${to}?` },
  );
  if (!choice || choice.label !== 'Apply') {
    output.appendLine('[pf] apply cancelled');
    return;
  }

  let result;
  try {
    result = await ctx.client.request('patch.apply', {
      workspace_id: ctx.workspaceId,
      plan,
    });
  } catch (e) {
    vscode.window.showErrorMessage(`apply failed: ${e.message}`);
    return;
  }
  if (result.applied) {
    output.appendLine(
      `[pf] applied: commit ${result.commit_id} (${result.files_written} file(s), ${result.bytes_written} bytes)`,
    );
    vscode.window.showInformationMessage(
      `Prolog Forge: rename applied (commit ${result.commit_id})`,
    );
    // Reload the files in open editors so VS Code picks up the new bytes.
    await vscode.commands.executeCommand('workbench.action.files.revert');
  } else {
    const reason = result.rejection_reason || 'unknown';
    output.appendLine(`[pf] rejected: ${reason}`);
    if (result.validation && !result.validation.ok) {
      for (const st of result.validation.stages) {
        if (!st.ok) {
          output.appendLine(`  stage [${st.stage}]:`);
          for (const d of st.diagnostics) {
            const where = d.file ? ` (${d.file})` : '';
            output.appendLine(`    ${d.severity}: ${d.message}${where}`);
          }
        }
      }
    }
    vscode.window.showErrorMessage(`Prolog Forge: rename rejected (${reason})`);
  }
}

// -------- LLM: Propose Patch ---------------------------------------------

/**
 * Ask the daemon for every function entity and return a QuickPick-ready
 * array of `{ label: name, description: id }`. Sorted by name for stable
 * ordering in the picker.
 */
async function listFunctions(ctx) {
  const r = await ctx.client.request('graph.query', {
    workspace_id: ctx.workspaceId,
    pattern: 'function(Id, Name)',
  });
  const seen = new Set();
  const items = [];
  for (const b of r.bindings) {
    const id = b.Id;
    const name = b.Name;
    if (!id || !name || seen.has(id)) continue;
    seen.add(id);
    items.push({ label: name, description: id });
  }
  items.sort((a, b) => a.label.localeCompare(b.label));
  return items;
}

async function pickValidationProfile() {
  const choice = await vscode.window.showQuickPick(
    [
      { label: 'default', description: 'syntactic + rules (fast)' },
      { label: 'typed', description: 'adds `cargo check` — slower, stronger' },
      {
        label: 'tested',
        description: 'adds `cargo test` on impacted tests — slowest, strongest',
      },
    ],
    { placeHolder: 'Validation profile' },
  );
  return choice ? choice.label : null;
}

function renderVerdict(ex) {
  const lines = [];
  const v = ex.verdict || {};
  const kind = v.kind || 'unknown';
  lines.push(`verdict: ${kind}`);
  if (kind === 'accepted') {
    if (v.commit_id) lines.push(`  commit: ${v.commit_id}`);
    for (const n of v.notes || []) lines.push(`  note: ${n}`);
  } else if (kind === 'rejected') {
    if (v.reason) lines.push(`  reason: ${v.reason}`);
    for (const s of v.failing_stages || []) lines.push(`  failing stage: ${s}`);
  } else if (kind === 'not_proven') {
    for (const n of v.notes || []) lines.push(`  note: ${n}`);
  }
  const s = ex.stats || {};
  lines.push(
    `  stats: anchors=${s.anchors} observed=${s.observed_cited} ` +
      `inferred=${s.inferred_cited} rules=${s.rule_activations} ` +
      `candidates=${s.candidates_considered} stages=${s.stages_run}`,
  );
  if (ex.summary) lines.push(`  summary: ${ex.summary}`);
  return lines;
}

async function cmdProposePatch() {
  const ctx = await requireWorkspace();
  if (!ctx) return;

  let fns;
  try {
    fns = await listFunctions(ctx);
  } catch (e) {
    vscode.window.showErrorMessage(`graph.query failed: ${e.message}`);
    return;
  }
  if (fns.length === 0) {
    vscode.window.showInformationMessage(
      'Prolog Forge: no functions indexed (workspace.index ran but the graph is empty)',
    );
    return;
  }
  const pick = await vscode.window.showQuickPick(fns, {
    placeHolder: 'Anchor function (context selection radius starts here)',
    matchOnDescription: true,
  });
  if (!pick) return;
  const anchorId = pick.description;
  const anchorName = pick.label;

  const intent = await vscode.window.showInputBox({
    prompt: `Intent to propose against "${anchorName}"`,
    placeHolder: 'e.g. rename to snake_case / add Debug derive / remove useless helper',
  });
  if (!intent) return;

  const memStr = await vscode.window.showInputBox({
    prompt: 'Memory hint: how many recent commits to feed the proposer? (0 = none)',
    value: '3',
    validateInput: (s) => (/^\d+$/.test(s) ? null : 'must be a non-negative integer'),
  });
  if (memStr === undefined) return;
  const includeMemory = parseInt(memStr, 10);

  output.show(true);
  output.appendLine(
    `\n[pf] propose_patch: anchor=${anchorName} (${anchorId}) intent="${intent}" ` +
      `memory=${includeMemory}`,
  );

  let r;
  try {
    r = await ctx.client.request('llm.propose_patch', {
      workspace_id: ctx.workspaceId,
      intent,
      anchor_id: anchorId,
      hops: 1,
      max_facts: 256,
      include_memory: includeMemory > 0 ? includeMemory : null,
    });
  } catch (e) {
    vscode.window.showErrorMessage(`llm.propose_patch failed: ${e.message}`);
    return;
  }
  output.appendLine(
    `[pf] accepted=${r.accepted} rejected=${r.rejected} ` +
      `cache_hit=${r.cache_hit} tokens_in=${r.tokens_in} tokens_out=${r.tokens_out}`,
  );

  if (r.candidates.length === 0) {
    vscode.window.showInformationMessage('Prolog Forge: LLM returned no candidates');
    return;
  }

  for (let i = 0; i < r.candidates.length; i++) {
    const c = r.candidates[i];
    const head = c.accepted ? 'ACCEPT' : 'REJECT';
    output.appendLine(`  [${i}] ${head} ${c.plan.label || '(unlabeled)'} — ${c.justification}`);
    if (c.rejection_reason) {
      output.appendLine(`         why: ${c.rejection_reason}`);
    }
  }

  const accepted = r.candidates.filter((c) => c.accepted);
  if (accepted.length === 0) {
    vscode.window.showInformationMessage(
      'Prolog Forge: every candidate was rejected by the identifier-resolution guard — see the output channel',
    );
    return;
  }

  const candPick = await vscode.window.showQuickPick(
    accepted.map((c, i) => ({
      label: c.plan.label || `candidate #${i}`,
      description: c.plan.ops.map((o) => o.op || 'op').join(','),
      detail: c.justification,
      candidate: c,
    })),
    { placeHolder: 'Choose a candidate to work with' },
  );
  if (!candPick) return;
  const plan = candPick.candidate.plan;

  const action = await vscode.window.showQuickPick(
    [
      { label: 'Explain', description: 'proof-carrying verdict, no file writes' },
      {
        label: 'Apply',
        description: 'preview diff → confirm → transactional apply',
      },
    ],
    { placeHolder: 'What to do with this candidate?' },
  );
  if (!action) return;

  const profile = await pickValidationProfile();
  if (!profile) return;
  const validationProfile = profile === 'default' ? null : profile;

  if (action.label === 'Explain') {
    let ex;
    try {
      ex = await ctx.client.request('explain.patch', {
        workspace_id: ctx.workspaceId,
        plan,
        candidate_outcomes: [],
        validation_profile: validationProfile,
      });
    } catch (e) {
      vscode.window.showErrorMessage(`explain.patch failed: ${e.message}`);
      return;
    }
    output.appendLine(`\n[pf] explain profile=${profile} plan="${ex.plan_label}"`);
    output.appendLine(`  anchors: ${(ex.anchors || []).join(', ') || '(none)'}`);
    for (const line of renderVerdict(ex)) {
      output.appendLine(`  ${line}`);
    }
    return;
  }

  // Apply: preview → confirm → apply.
  let preview;
  try {
    preview = await ctx.client.request('patch.preview', {
      workspace_id: ctx.workspaceId,
      plan,
    });
  } catch (e) {
    vscode.window.showErrorMessage(`preview failed: ${e.message}`);
    return;
  }
  output.appendLine(
    `\n[pf] preview: ${preview.total_replacements} replacement(s) across ${preview.files.length} file(s)`,
  );
  for (const err of preview.errors) {
    output.appendLine(`[pf] error in ${err.file}: ${err.message}`);
  }
  for (const f of preview.files) {
    output.appendLine(
      `\n# ${f.path} (${f.before_len} -> ${f.after_len} bytes, ${f.replacements} repl)`,
    );
    output.appendLine(f.diff);
  }
  if (preview.total_replacements === 0) {
    vscode.window.showInformationMessage('Prolog Forge: preview is empty — nothing to apply');
    return;
  }
  const confirm = await vscode.window.showQuickPick(
    [
      {
        label: 'Apply',
        description: `write to disk with profile=${profile} (transactional)`,
      },
      { label: 'Cancel', description: 'discard the preview' },
    ],
    { placeHolder: `Apply "${plan.label || 'candidate'}"?` },
  );
  if (!confirm || confirm.label !== 'Apply') {
    output.appendLine('[pf] apply cancelled');
    return;
  }

  let result;
  try {
    result = await ctx.client.request('patch.apply', {
      workspace_id: ctx.workspaceId,
      plan,
      validation_profile: validationProfile,
    });
  } catch (e) {
    vscode.window.showErrorMessage(`apply failed: ${e.message}`);
    return;
  }
  if (result.applied) {
    output.appendLine(
      `[pf] applied: commit ${result.commit_id} (${result.files_written} file(s), ${result.bytes_written} bytes)`,
    );
    vscode.window.showInformationMessage(
      `Prolog Forge: applied (commit ${result.commit_id})`,
    );
    await vscode.commands.executeCommand('workbench.action.files.revert');
  } else {
    const reason = result.rejection_reason || 'unknown';
    output.appendLine(`[pf] rejected: ${reason}`);
    if (result.validation && !result.validation.ok) {
      for (const st of result.validation.stages) {
        if (!st.ok) {
          output.appendLine(`  stage [${st.stage}]:`);
          for (const d of st.diagnostics) {
            const where = d.file ? ` (${d.file})` : '';
            output.appendLine(`    ${d.severity}: ${d.message}${where}`);
          }
        }
      }
    }
    vscode.window.showErrorMessage(`Prolog Forge: candidate rejected (${reason})`);
  }
}

// -------- Explain Rename --------------------------------------------------

async function cmdExplainRename() {
  const ctx = await requireWorkspace();
  if (!ctx) return;

  const from = await vscode.window.showInputBox({
    prompt: 'Function name to explain a rename of',
    validateInput: (s) =>
      /^[A-Za-z_][A-Za-z0-9_]*$/.test(s) ? null : 'must be a valid Rust identifier',
  });
  if (!from) return;
  const to = await vscode.window.showInputBox({
    prompt: `Hypothetical new name for "${from}"`,
    validateInput: (s) =>
      /^[A-Za-z_][A-Za-z0-9_]*$/.test(s) ? null : 'must be a valid Rust identifier',
  });
  if (!to) return;

  const profile = await pickValidationProfile();
  if (!profile) return;
  const validationProfile = profile === 'default' ? null : profile;

  const plan = {
    ops: [{ op: 'rename_function', old_name: from, new_name: to, files: [] }],
    label: `rename ${from} -> ${to}`,
  };

  output.show(true);
  output.appendLine(
    `\n[pf] explain: rename ${from} -> ${to} profile=${profile} (read-only, no file writes)`,
  );

  let ex;
  try {
    ex = await ctx.client.request('explain.patch', {
      workspace_id: ctx.workspaceId,
      plan,
      candidate_outcomes: [],
      validation_profile: validationProfile,
    });
  } catch (e) {
    vscode.window.showErrorMessage(`explain.patch failed: ${e.message}`);
    return;
  }
  output.appendLine(`  plan: ${ex.plan_label}`);
  output.appendLine(`  anchors: ${(ex.anchors || []).join(', ') || '(none)'}`);
  for (const line of renderVerdict(ex)) {
    output.appendLine(`  ${line}`);
  }
}

// -------- Show History ----------------------------------------------------

async function cmdHistory() {
  const ctx = await requireWorkspace();
  if (!ctx) return;
  let r;
  try {
    r = await ctx.client.request('memory.history', {
      workspace_id: ctx.workspaceId,
    });
  } catch (e) {
    vscode.window.showErrorMessage(`memory.history failed: ${e.message}`);
    return;
  }
  output.show(true);
  output.appendLine(`\n[pf] history: ${r.items.length} commit(s) (newest first)`);
  if (r.items.length === 0) {
    output.appendLine('  (no commits)');
    return;
  }
  for (const it of r.items) {
    const profile = it.validation_profile || '-';
    const ops = it.ops_summary.length > 0 ? it.ops_summary.join(',') : '(unknown)';
    output.appendLine(
      `  ${it.commit_id}  ts=${it.timestamp_unix}  profile=${profile}  ` +
        `ops=[${ops}]  files=${it.files_changed}  repl=${it.total_replacements}  ` +
        `label=${it.label}`,
    );
  }
}

// -------- Show Stats ------------------------------------------------------

async function cmdStats() {
  const ctx = await requireWorkspace();
  if (!ctx) return;
  let s;
  try {
    s = await ctx.client.request('memory.stats', {
      workspace_id: ctx.workspaceId,
    });
  } catch (e) {
    vscode.window.showErrorMessage(`memory.stats failed: ${e.message}`);
    return;
  }
  output.show(true);
  output.appendLine(
    `\n[pf] stats: commits=${s.commits} files_touched=${s.files_touched} bytes_written=${s.total_bytes_written}`,
  );
  if (s.first_commit_at != null && s.last_commit_at != null) {
    output.appendLine(`  first: ts=${s.first_commit_at}  last: ts=${s.last_commit_at}`);
  }
  if (Object.keys(s.by_op_kind).length > 0) {
    output.appendLine('  by op kind:');
    for (const [k, v] of Object.entries(s.by_op_kind)) {
      output.appendLine(`    ${k}: ${v}`);
    }
  }
  if (Object.keys(s.by_validation_profile).length > 0) {
    output.appendLine('  by validation profile:');
    for (const [k, v] of Object.entries(s.by_validation_profile)) {
      output.appendLine(`    ${k}: ${v}`);
    }
  }
  if (s.top_files.length > 0) {
    output.appendLine('  top files (by edit count):');
    for (const f of s.top_files) {
      output.appendLine(`    ${f.path} — ${f.commit_count}`);
    }
  }
}

// -------- Daemon Info -----------------------------------------------------

async function cmdInfo() {
  if (!client) {
    vscode.window.showErrorMessage('Prolog Forge: daemon is not running');
    return;
  }
  let caps;
  try {
    caps = await client.request('session.initialize', {
      client: { name: 'vscode-adapter', version: '0.0.1' },
    });
  } catch (e) {
    vscode.window.showErrorMessage(`session.initialize failed: ${e.message}`);
    return;
  }
  output.show(true);
  output.appendLine(`\n[pf] name: ${caps.name}`);
  output.appendLine(`[pf] version: ${caps.version}`);
  output.appendLine(`[pf] protocol: ${caps.protocol_version}`);
  output.appendLine('[pf] methods:');
  for (const m of caps.methods) {
    output.appendLine(`  - ${m}`);
  }
  if (workspaceId) {
    output.appendLine(`[pf] workspace: ${workspaceId} @ ${workspaceRoot}`);
  }
}

function deactivate() {
  if (client) return client.shutdown();
  return undefined;
}

module.exports = { activate, deactivate };
