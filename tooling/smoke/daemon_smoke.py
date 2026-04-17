#!/usr/bin/env python3
"""End-to-end smoke test for the pf-daemon JSON-RPC stdio protocol.

Spawns the daemon binary, runs a full session (initialize -> open -> load
rules -> evaluate -> query -> shutdown), and asserts the expected outcomes.
Intended for CI; minimal deps (stdlib only).
"""
from __future__ import annotations

import json
import os
import subprocess
import sys


BIN = os.environ.get("PF_DAEMON", "./target/debug/pf-daemon")


def send(proc: subprocess.Popen, req: dict) -> None:
    body = json.dumps(req).encode()
    header = f"Content-Length: {len(body)}\r\n\r\n".encode()
    proc.stdin.write(header + body)
    proc.stdin.flush()


def recv(proc: subprocess.Popen) -> dict:
    headers = b""
    while b"\r\n\r\n" not in headers:
        c = proc.stdout.read(1)
        if not c:
            raise RuntimeError(
                f"daemon closed stdout; stderr=\n{proc.stderr.read().decode(errors='replace')}"
            )
        headers += c
    header_text = headers.decode().split("\r\n\r\n", 1)[0]
    length = None
    for line in header_text.split("\r\n"):
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
    assert length is not None, f"no Content-Length in {header_text!r}"
    body = proc.stdout.read(length)
    return json.loads(body)


def main() -> int:
    if not os.path.exists(BIN):
        print(f"daemon binary not found at {BIN}; run `cargo build` first", file=sys.stderr)
        return 2

    proc = subprocess.Popen(
        [BIN],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    try:
        send(proc, {
            "jsonrpc": "2.0", "id": 1, "method": "session.initialize",
            "params": {"client": {"name": "smoke", "version": "0"}},
        })
        caps = recv(proc)["result"]
        assert caps["name"] == "prolog-forge", caps
        assert caps["protocol_version"].startswith("0."), caps

        send(proc, {
            "jsonrpc": "2.0", "id": 2, "method": "workspace.open",
            "params": {"root": "/tmp"},
        })
        ws = recv(proc)["result"]["workspace_id"]

        src = (
            "parent(alice, bob). parent(bob, carol). parent(carol, dan). "
            "ancestor(X, Y) :- parent(X, Y). "
            "ancestor(X, Z) :- parent(X, Y), ancestor(Y, Z)."
        )
        send(proc, {
            "jsonrpc": "2.0", "id": 3, "method": "rules.load",
            "params": {"workspace_id": ws, "source": src},
        })
        loaded = recv(proc)["result"]
        assert loaded == {"rules_added": 2, "facts_added": 3}, loaded

        send(proc, {
            "jsonrpc": "2.0", "id": 4, "method": "rules.evaluate",
            "params": {"workspace_id": ws},
        })
        stats = recv(proc)["result"]
        assert stats["derived"] == 6, stats

        send(proc, {
            "jsonrpc": "2.0", "id": 5, "method": "graph.query",
            "params": {"workspace_id": ws, "pattern": "ancestor(alice, X)"},
        })
        q = recv(proc)["result"]
        assert q["count"] == 3, q
        xs = sorted(b["X"] for b in q["bindings"])
        assert xs == ["bob", "carol", "dan"], xs

        # ---- workspace.index against the Rust fixture --------------------
        send(proc, {
            "jsonrpc": "2.0", "id": 6, "method": "workspace.open",
            "params": {"root": os.path.abspath("examples/rust-demo")},
        })
        ws2 = recv(proc)["result"]["workspace_id"]

        send(proc, {
            "jsonrpc": "2.0", "id": 7, "method": "workspace.index",
            "params": {"workspace_id": ws2},
        })
        idx = recv(proc)["result"]
        assert idx["files_indexed"] >= 1, idx
        assert idx["files_failed"] == 0, idx
        assert idx["facts_inserted"] > 0, idx

        with open("examples/rust-demo/rules.pfr") as f:
            rules_src = f.read()
        send(proc, {
            "jsonrpc": "2.0", "id": 8, "method": "rules.load",
            "params": {"workspace_id": ws2, "source": rules_src},
        })
        recv(proc)
        send(proc, {
            "jsonrpc": "2.0", "id": 9, "method": "rules.evaluate",
            "params": {"workspace_id": ws2},
        })
        recv(proc)
        send(proc, {
            "jsonrpc": "2.0", "id": 10, "method": "graph.query",
            "params": {"workspace_id": ws2, "pattern": "recursive(F)"},
        })
        rec = recv(proc)["result"]
        assert rec["count"] == 1, rec
        assert "countdown" in rec["bindings"][0]["F"], rec

        send(proc, {"jsonrpc": "2.0", "id": 11, "method": "session.shutdown"})
        recv(proc)
        proc.wait(timeout=5)
        print("daemon smoke test OK")
        return 0
    finally:
        if proc.poll() is None:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
