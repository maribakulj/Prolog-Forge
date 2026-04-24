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
  } else {
    output.appendLine('[pf] no workspace folder — commands will ask to open one');
  }

  context.subscriptions.push(
    vscode.commands.registerCommand('prologForge.renameFunction', cmdRename),
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
