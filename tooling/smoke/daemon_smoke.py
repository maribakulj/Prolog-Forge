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

        # ---- llm.propose (mock provider) ---------------------------------
        # Pick any known function id to anchor on.
        send(proc, {
            "jsonrpc": "2.0", "id": 11, "method": "graph.query",
            "params": {"workspace_id": ws2, "pattern": "function(F, add)"},
        })
        fn_rows = recv(proc)["result"]["bindings"]
        assert fn_rows, "expected at least one function(_, add) fact"
        anchor = fn_rows[0]["F"]

        send(proc, {
            "jsonrpc": "2.0", "id": 12, "method": "llm.propose",
            "params": {
                "workspace_id": ws2,
                "intent": "propose purity",
                "anchor_id": anchor,
                "hops": 1,
            },
        })
        prop = recv(proc)["result"]
        assert prop["accepted"] >= 1, prop
        assert prop["rejected"] >= 1, prop  # MockProvider includes a hallucination
        assert any(
            (o["rejection_reason"] or "").find("hallucination") >= 0
            for o in prop["outcomes"] if not o["accepted"]
        ), prop

        # Second call hits the cache (context unchanged — candidates are
        # excluded from the trusted context).
        send(proc, {
            "jsonrpc": "2.0", "id": 13, "method": "llm.propose",
            "params": {
                "workspace_id": ws2,
                "intent": "propose purity",
                "anchor_id": anchor,
                "hops": 1,
            },
        })
        prop2 = recv(proc)["result"]
        assert prop2["cache_hit"] is True, prop2

        # ---- llm.refine: the revision loop. Feed the rejected
        # hallucination from llm.propose back as a prior outcome. The
        # refiner must drop it and converge in one round with zero
        # rejections.
        rejected_prior = [
            o for o in prop["outcomes"] if not o["accepted"]
        ]
        assert rejected_prior, prop
        send(proc, {
            "jsonrpc": "2.0", "id": 131, "method": "llm.refine",
            "params": {
                "workspace_id": ws2,
                "intent": "refine purity after hallucination",
                "anchor_id": anchor,
                "hops": 1,
                "max_rounds": 3,
                "prior_outcomes": rejected_prior,
                "prior_diagnostics": [],
            },
        })
        refine = recv(proc)["result"]
        assert refine["converged"] is True, refine
        assert refine["final_rejected"] == 0, refine
        assert refine["rounds"] == 1, refine
        assert all(
            o["round"] is not None and o["round"] >= 1 for o in refine["outcomes"]
        ), refine
        # The hallucinated id must not reappear in any outcome.
        bogus_args = [a for o in rejected_prior for a in o["args"]]
        for o in refine["outcomes"]:
            for a in o["args"]:
                assert a not in bogus_args, (o, bogus_args)

        # ---- patch.preview (rename) --------------------------------------
        send(proc, {
            "jsonrpc": "2.0", "id": 14, "method": "patch.preview",
            "params": {
                "workspace_id": ws2,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "add",
                        "new_name": "sum",
                        "files": [],
                    }],
                    "label": "smoke: add -> sum",
                },
            },
        })
        prev = recv(proc)["result"]
        assert prev["total_replacements"] == 3, prev
        assert len(prev["files"]) == 1, prev
        diff = prev["files"][0]["diff"]
        assert "-pub fn add" in diff, diff
        assert "+pub fn sum" in diff, diff
        # FS must be untouched (preview only).
        with open("examples/rust-demo/src/lib.rs") as f:
            assert "pub fn add(" in f.read(), "preview must not write to disk"

        # ---- patch.apply on a scratch copy, assert validation + FS write
        import shutil, tempfile
        scratch_root = tempfile.mkdtemp(prefix="pf-smoke-")
        scratch_demo = os.path.join(scratch_root, "demo")
        shutil.copytree("examples/rust-demo", scratch_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 15, "method": "workspace.open",
            "params": {"root": scratch_demo},
        })
        scratch_ws = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 16, "method": "patch.apply",
            "params": {
                "workspace_id": scratch_ws,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "add",
                        "new_name": "sum",
                        "files": [],
                    }],
                    "label": "smoke: apply add->sum",
                },
            },
        })
        applied = recv(proc)["result"]
        assert applied["applied"] is True, applied
        assert applied["validation"]["ok"] is True, applied
        assert applied["files_written"] == 1, applied
        with open(os.path.join(scratch_demo, "src/lib.rs")) as f:
            scratch_content = f.read()
        assert "pub fn sum(" in scratch_content, "apply must write new content"
        assert "pub fn add(" not in scratch_content, "apply must remove old name"

        # ---- apply an invalid plan to exercise the validation gate.
        # Rename `Counter` to a reserved keyword to produce broken syntax,
        # and assert the apply is rejected and the FS is untouched.
        content_before = scratch_content
        send(proc, {
            "jsonrpc": "2.0", "id": 17, "method": "patch.apply",
            "params": {
                "workspace_id": scratch_ws,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "Counter",
                        "new_name": "fn",
                        "files": [],
                    }],
                    "label": "smoke: invalid rename",
                },
            },
        })
        bad = recv(proc)["result"]
        # Either the rename itself refuses to produce invalid Rust (no-op)
        # or validation catches it. Either way, the FS must not change.
        assert bad["files_written"] == 0 or not bad["applied"], bad
        with open(os.path.join(scratch_demo, "src/lib.rs")) as f:
            assert f.read() == content_before, "invalid apply must not touch disk"

        # Upstream fixture must still be untouched by either smoke path.
        with open("examples/rust-demo/src/lib.rs") as f:
            upstream = f.read()
        assert "pub fn add(" in upstream, "upstream fixture must remain original"

        # ---- rule-pack apply gate (violation/1 convention)
        # Load a rule that forbids a function named `sum`, then attempt to
        # re-apply the same rename against a fresh scratch. Expect rejection.
        scratch2_root = tempfile.mkdtemp(prefix="pf-smoke-rules-")
        scratch2_demo = os.path.join(scratch2_root, "demo")
        shutil.copytree("examples/rust-demo", scratch2_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 19, "method": "workspace.open",
            "params": {"root": scratch2_demo},
        })
        ws3 = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 20, "method": "rules.load",
            "params": {
                "workspace_id": ws3,
                "source": "violation(F) :- function(F, sum).",
            },
        })
        recv(proc)
        send(proc, {
            "jsonrpc": "2.0", "id": 21, "method": "patch.apply",
            "params": {
                "workspace_id": ws3,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "add",
                        "new_name": "sum",
                        "files": [],
                    }],
                    "label": "smoke: gated apply",
                },
            },
        })
        gated = recv(proc)["result"]
        assert gated["applied"] is False, gated
        # At least one stage must have failed and the rules stage must be in
        # the report.
        stage_names = [s["stage"] for s in gated["validation"]["stages"]]
        assert "rules" in stage_names, gated
        with open(os.path.join(scratch2_demo, "src/lib.rs")) as f:
            assert "pub fn add(" in f.read(), "rule gate must prevent the write"

        # ---- explain.patch on the same gated plan: should produce a
        # Rejected verdict naming the "rules" stage as the culprit, plus a
        # stage-evidence node for each stage that ran.
        send(proc, {
            "jsonrpc": "2.0", "id": 211, "method": "explain.patch",
            "params": {
                "workspace_id": ws3,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "add",
                        "new_name": "sum",
                        "files": [],
                    }],
                    "label": "smoke: explain gated plan",
                },
                "candidate_outcomes": [],
            },
        })
        explained = recv(proc)["result"]
        assert explained["verdict"]["kind"] == "rejected", explained
        assert "rules" in explained["verdict"]["failing_stages"], explained
        assert explained["stats"]["stages_run"] >= 2, explained
        stage_nodes = [
            e for e in explained["evidence"] if e["kind"] == "stage"
        ]
        assert any(not s["ok"] and s["name"] == "rules" for s in stage_nodes), explained
        # Anchors must include both old and new names.
        assert "add" in explained["anchors"] and "sum" in explained["anchors"], explained

        # ---- explain.patch on an *unrule'd* workspace: with no semantic
        # stage available, the verdict must be `not_proven` (syntactic-only
        # evidence is not a proof). Reuse the clean scratch demo.
        send(proc, {
            "jsonrpc": "2.0", "id": 212, "method": "workspace.open",
            "params": {"root": scratch_demo},
        })
        ws_explain = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 213, "method": "workspace.index",
            "params": {"workspace_id": ws_explain},
        })
        recv(proc)
        send(proc, {
            "jsonrpc": "2.0", "id": 214, "method": "explain.patch",
            "params": {
                "workspace_id": ws_explain,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "sum",
                        "new_name": "total",
                        "files": [],
                    }],
                    "label": "smoke: explain clean plan",
                },
                "candidate_outcomes": [],
            },
        })
        explained_clean = recv(proc)["result"]
        assert explained_clean["verdict"]["kind"] == "not_proven", explained_clean
        assert explained_clean["stats"]["stages_run"] == 1, explained_clean

        # ---- full loop: apply -> rollback -> verify disk restored.
        scratch3_root = tempfile.mkdtemp(prefix="pf-smoke-rollback-")
        scratch3_demo = os.path.join(scratch3_root, "demo")
        shutil.copytree("examples/rust-demo", scratch3_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 22, "method": "workspace.open",
            "params": {"root": scratch3_demo},
        })
        ws4 = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 23, "method": "patch.apply",
            "params": {
                "workspace_id": ws4,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "add",
                        "new_name": "sum",
                        "files": [],
                    }],
                    "label": "smoke: rollback cycle",
                },
            },
        })
        ok_apply = recv(proc)["result"]
        assert ok_apply["applied"] is True
        commit_id = ok_apply["commit_id"]
        with open(os.path.join(scratch3_demo, "src/lib.rs")) as f:
            assert "pub fn sum(" in f.read()

        send(proc, {
            "jsonrpc": "2.0", "id": 24, "method": "patch.rollback",
            "params": {"workspace_id": ws4, "commit_id": commit_id},
        })
        rolled = recv(proc)["result"]
        assert rolled["rolled_back"] is True, rolled
        assert rolled["files_restored"] == 1, rolled
        with open(os.path.join(scratch3_demo, "src/lib.rs")) as f:
            restored = f.read()
        assert "pub fn add(" in restored, "rollback must restore original content"
        assert "pub fn sum(" not in restored, "rollback must remove patch content"
        # Journal entry must have been deleted.
        assert not os.path.exists(os.path.join(
            scratch3_demo, ".prolog-forge/journal", f"{commit_id}.json"
        )), "rollback must delete journal entry"

        shutil.rmtree(scratch_root, ignore_errors=True)
        shutil.rmtree(scratch2_root, ignore_errors=True)
        shutil.rmtree(scratch3_root, ignore_errors=True)

        send(proc, {"jsonrpc": "2.0", "id": 25, "method": "session.shutdown"})
        recv(proc)
        proc.wait(timeout=5)
        print("daemon smoke test OK")
        return 0
    finally:
        if proc.poll() is None:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
