// JSON-RPC client for pf-daemon.
//
// Spawns the daemon as a child process, speaks LSP-style Content-Length
// framing over stdio, and correlates requests with responses by numeric
// id. Deliberately dependency-free so the extension loads directly
// from the sources without an `npm install` step — only Node built-ins
// (`child_process`, `Buffer`) are used.
//
// The protocol surface we speak is mirrored from `pf-protocol`; the
// client is intentionally thin and does not know about any specific
// method. Higher-level flows (rename, explain, memory) live in
// `extension.js` and compose `request()` calls.

'use strict';

const { spawn } = require('child_process');

class Client {
  /**
   * @param {string} binPath Absolute path or PATH-lookup name for pf-daemon.
   * @param {number} timeoutMs Per-request wall-clock cap in ms.
   * @param {(line: string) => void} [log] Optional sink for stderr/info lines.
   */
  constructor(binPath, timeoutMs, log) {
    this.binPath = binPath;
    this.timeoutMs = timeoutMs || 30000;
    this.log = log || (() => {});
    this.proc = null;
    this.nextId = 1;
    this.pending = new Map(); // id -> { resolve, reject, timer }
    this.buffer = Buffer.alloc(0);
    this.shuttingDown = false;
  }

  /**
   * Spawn the daemon and run the `session.initialize` handshake.
   * Returns the server capabilities from the initialize response.
   */
  async start() {
    this.proc = spawn(this.binPath, [], {
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    this.proc.on('error', (err) => {
      this.log(`[pf-daemon spawn error] ${err.message}`);
      this._failAllPending(new Error(`pf-daemon spawn: ${err.message}`));
    });
    this.proc.stdout.on('data', (chunk) => this._onStdoutData(chunk));
    this.proc.stderr.on('data', (chunk) => {
      // Daemon logs go to stderr; forward line-by-line so the
      // extension's OutputChannel reads like a real log.
      const text = chunk.toString('utf8');
      for (const line of text.split(/\r?\n/)) {
        if (line.length > 0) {
          this.log(`[pf-daemon] ${line}`);
        }
      }
    });
    this.proc.on('exit', (code, signal) => {
      const reason = signal
        ? `killed by ${signal}`
        : `exited with code ${code}`;
      this.log(`[pf-daemon] ${reason}`);
      if (!this.shuttingDown) {
        this._failAllPending(new Error(`pf-daemon ${reason}`));
      }
    });

    const caps = await this.request('session.initialize', {
      client: { name: 'vscode-adapter', version: '0.0.1' },
    });
    return caps;
  }

  /**
   * Send a JSON-RPC request and return a promise for its result.
   */
  request(method, params) {
    if (!this.proc || this.proc.killed) {
      return Promise.reject(new Error('pf-daemon not running'));
    }
    const id = this.nextId++;
    const envelope = {
      jsonrpc: '2.0',
      id,
      method,
      params: params === undefined ? null : params,
    };
    const body = Buffer.from(JSON.stringify(envelope), 'utf8');
    const header = Buffer.from(
      `Content-Length: ${body.length}\r\n\r\n`,
      'utf8',
    );

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) {
          reject(new Error(`request ${method} timed out after ${this.timeoutMs}ms`));
        }
      }, this.timeoutMs);
      this.pending.set(id, {
        resolve: (v) => {
          clearTimeout(timer);
          resolve(v);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
      try {
        this.proc.stdin.write(Buffer.concat([header, body]));
      } catch (e) {
        if (this.pending.delete(id)) {
          clearTimeout(timer);
          reject(e);
        }
      }
    });
  }

  /**
   * Send `session.shutdown` and reap the subprocess. Safe to call
   * more than once; subsequent calls are no-ops.
   */
  async shutdown() {
    if (this.shuttingDown) return;
    this.shuttingDown = true;
    try {
      await this.request('session.shutdown', null);
    } catch (_e) {
      // Already dead, timed out, etc. — proceed to kill.
    }
    if (this.proc && !this.proc.killed) {
      try {
        this.proc.kill();
      } catch (_e) {
        // best effort
      }
    }
  }

  // ---- internals ---------------------------------------------------------

  _onStdoutData(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    // Drain as many complete messages as the buffer contains.
    while (true) {
      const headerEnd = this.buffer.indexOf('\r\n\r\n');
      if (headerEnd === -1) return;
      const headerText = this.buffer.slice(0, headerEnd).toString('utf8');
      const match = headerText.match(/Content-Length:\s*(\d+)/i);
      if (!match) {
        // Malformed header — drop everything up to the separator and
        // retry. A real LSP peer would never emit a body without a
        // Content-Length, so this is a defensive path.
        this.log(`[pf-daemon] malformed header, dropping: ${headerText}`);
        this.buffer = this.buffer.slice(headerEnd + 4);
        continue;
      }
      const length = parseInt(match[1], 10);
      const bodyStart = headerEnd + 4;
      if (this.buffer.length < bodyStart + length) return;
      const body = this.buffer.slice(bodyStart, bodyStart + length).toString('utf8');
      this.buffer = this.buffer.slice(bodyStart + length);

      let msg;
      try {
        msg = JSON.parse(body);
      } catch (e) {
        this.log(`[pf-daemon] bad JSON: ${e.message}`);
        continue;
      }
      if (msg.id === undefined || msg.id === null) {
        // Notifications / server-initiated requests — we don't
        // produce any in the current protocol, but drain silently
        // so they can be added later without breaking older clients.
        continue;
      }
      const entry = this.pending.get(msg.id);
      if (!entry) continue; // late response
      this.pending.delete(msg.id);
      if (msg.error) {
        entry.reject(new Error(msg.error.message || JSON.stringify(msg.error)));
      } else {
        entry.resolve(msg.result === undefined ? null : msg.result);
      }
    }
  }

  _failAllPending(err) {
    for (const entry of this.pending.values()) {
      try {
        entry.reject(err);
      } catch (_e) {
        // ignore
      }
    }
    this.pending.clear();
  }
}

module.exports = { Client };
