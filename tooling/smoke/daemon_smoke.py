#!/usr/bin/env python3
"""End-to-end smoke test for the aa-daemon JSON-RPC stdio protocol.

Spawns the daemon binary, runs a full session (initialize -> open -> load
rules -> evaluate -> query -> shutdown), and asserts the expected outcomes.
Intended for CI; minimal deps (stdlib only).
"""
from __future__ import annotations

import json
import os
import subprocess
import sys


BIN = os.environ.get("AA_DAEMON", "./target/debug/aa-daemon")


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
        assert caps["name"] == "aye-aye", caps
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
        # Phase 1.10 — macro-aware rename now descends into `assert_eq!`
        # token trees, so the rename count is 3 function-body occurrences
        # plus 2 in `#[cfg(test)]` macro bodies = 5. (Scope resolution is
        # still a future phase; shadowed locals of the same name would
        # still be renamed. See docs/rust-rename.md when it lands.)
        assert prev["total_replacements"] == 5, prev
        assert len(prev["files"]) == 1, prev
        diff = prev["files"][0]["diff"]
        assert "-pub fn add" in diff, diff
        assert "+pub fn sum" in diff, diff
        # FS must be untouched (preview only).
        with open("examples/rust-demo/src/lib.rs") as f:
            assert "pub fn add(" in f.read(), "preview must not write to disk"

        # ---- patch.apply on a scratch copy, assert validation + FS write
        import shutil, tempfile
        scratch_root = tempfile.mkdtemp(prefix="aa-smoke-")
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
        scratch2_root = tempfile.mkdtemp(prefix="aa-smoke-rules-")
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
        scratch3_root = tempfile.mkdtemp(prefix="aa-smoke-rollback-")
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
            scratch3_demo, ".aye-aye/journal", f"{commit_id}.json"
        )), "rollback must delete journal entry"

        # ---- validation_profile = "typed": cargo_check on a complete
        # macro-aware rename. `add -> sum` now rewrites every occurrence
        # including the ones inside `assert_eq!` macro bodies (Phase 1.10
        # landed the macro walk), so the shadow type-checks and the
        # apply should succeed. Before Phase 1.10 this same plan would
        # have been rejected — cargo_check finds unresolved `add` in the
        # test module — and that demo is now out of date. The Phase 1.10
        # demo is covered by the `skips_macro_rules_meta_variable_bodies`
        # unit test in aa-patch.
        scratch4_root = tempfile.mkdtemp(prefix="aa-smoke-typed-")
        scratch4_demo = os.path.join(scratch4_root, "demo")
        shutil.copytree("examples/rust-demo", scratch4_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 26, "method": "workspace.open",
            "params": {"root": scratch4_demo},
        })
        ws_typed = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 27, "method": "patch.apply",
            "params": {
                "workspace_id": ws_typed,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "add",
                        "new_name": "sum",
                        "files": [],
                    }],
                    "label": "smoke: typed apply (add -> sum)",
                },
                "validation_profile": "typed",
            },
        })
        typed = recv(proc)["result"]
        assert typed["applied"] is True, typed
        stage_names = [s["stage"] for s in typed["validation"]["stages"]]
        assert "cargo_check" in stage_names, typed
        cargo_stage = next(
            s for s in typed["validation"]["stages"] if s["stage"] == "cargo_check"
        )
        assert cargo_stage["ok"] is True, cargo_stage
        # The on-disk file must now have `pub fn sum` and every
        # previously-add reference rewritten — including inside
        # `assert_eq!(...)` test-module macro bodies. Proves Phase 1.10's
        # macro-aware rename landed.
        with open(os.path.join(scratch4_demo, "src/lib.rs")) as f:
            final = f.read()
        assert "pub fn sum(" in final, final
        assert "pub fn add(" not in final, final
        assert "assert_eq!(sum(1, 2), 3);" in final, final
        assert "assert_eq!(sum(2, 1), 3);" in final, final

        # Second typed apply on the same demo, different target, to prove
        # the pipeline is reusable after a successful apply.
        send(proc, {
            "jsonrpc": "2.0", "id": 28, "method": "workspace.open",
            "params": {"root": scratch4_demo},
        })
        ws_typed2 = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 29, "method": "patch.apply",
            "params": {
                "workspace_id": ws_typed2,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "useless",
                        "new_name": "blank",
                        "files": [],
                    }],
                    "label": "smoke: typed apply (useless -> blank)",
                },
                "validation_profile": "typed",
            },
        })
        typed2 = recv(proc)["result"]
        assert typed2["applied"] is True, typed2
        cargo_stage2 = next(
            s for s in typed2["validation"]["stages"] if s["stage"] == "cargo_check"
        )
        assert cargo_stage2["ok"] is True, cargo_stage2

        # explain.patch on the same clean plan must synthesize an
        # `accepted` verdict (cargo_check supplies the semantic evidence
        # the syntactic stage alone could not).
        send(proc, {
            "jsonrpc": "2.0", "id": 30, "method": "workspace.open",
            "params": {"root": scratch4_demo},
        })
        ws_typed_explain = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 31, "method": "workspace.index",
            "params": {"workspace_id": ws_typed_explain},
        })
        recv(proc)
        send(proc, {
            "jsonrpc": "2.0", "id": 32, "method": "explain.patch",
            "params": {
                "workspace_id": ws_typed_explain,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "does_not_exist_anywhere",
                        "new_name": "also_does_not_exist",
                        "files": [],
                    }],
                    "label": "smoke: typed explain",
                },
                "candidate_outcomes": [],
                "validation_profile": "typed",
            },
        })
        typed_explain = recv(proc)["result"]
        assert typed_explain["verdict"]["kind"] == "accepted", typed_explain
        stage_names2 = [
            e["name"] for e in typed_explain["evidence"] if e["kind"] == "stage"
        ]
        assert "cargo_check" in stage_names2, typed_explain

        # ---- validation_profile = "tested": cargo_test runs against
        # the shadow. `useless -> blank` is a no-op for behavior (zero
        # callers, no test depends on it), so the existing tests stay
        # green and the apply should succeed.
        scratch5_root = tempfile.mkdtemp(prefix="aa-smoke-tested-")
        scratch5_demo = os.path.join(scratch5_root, "demo")
        shutil.copytree("examples/rust-demo", scratch5_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 33, "method": "workspace.open",
            "params": {"root": scratch5_demo},
        })
        ws_tested = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 34, "method": "patch.apply",
            "params": {
                "workspace_id": ws_tested,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "useless",
                        "new_name": "blank",
                        "files": [],
                    }],
                    "label": "smoke: tested apply",
                },
                "validation_profile": "tested",
            },
        })
        tested = recv(proc)["result"]
        assert tested["applied"] is True, tested
        tested_stage_names = [s["stage"] for s in tested["validation"]["stages"]]
        assert "cargo_check" in tested_stage_names, tested
        assert "cargo_test" in tested_stage_names, tested
        cargo_test_stage = next(
            s for s in tested["validation"]["stages"] if s["stage"] == "cargo_test"
        )
        assert cargo_test_stage["ok"] is True, cargo_test_stage

        # ---- llm.propose_patch: the LLM emits typed patch plans, the
        # symbolic grounding guard filters hallucinations, and each
        # grounded plan is directly consumable by explain.patch. This
        # closes the full neuro-symbolic loop end-to-end.
        scratch6_root = tempfile.mkdtemp(prefix="aa-smoke-propose-patch-")
        scratch6_demo = os.path.join(scratch6_root, "demo")
        shutil.copytree("examples/rust-demo", scratch6_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 40, "method": "workspace.open",
            "params": {"root": scratch6_demo},
        })
        ws_pp = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 41, "method": "workspace.index",
            "params": {"workspace_id": ws_pp},
        })
        recv(proc)
        # Anchor on `useless` (zero callers) so a rename of it type-checks
        # cleanly under the typed profile later on.
        send(proc, {
            "jsonrpc": "2.0", "id": 42, "method": "graph.query",
            "params": {"workspace_id": ws_pp, "pattern": "function(F, useless)"},
        })
        anchor_rows = recv(proc)["result"]["bindings"]
        assert anchor_rows, anchor_rows
        anchor_id = anchor_rows[0]["F"]
        send(proc, {
            "jsonrpc": "2.0", "id": 43, "method": "llm.propose_patch",
            "params": {
                "workspace_id": ws_pp,
                "intent": "propose a typed rename for this area",
                "anchor_id": anchor_id,
                "hops": 1,
            },
        })
        pp = recv(proc)["result"]
        assert pp["accepted"] >= 1, pp
        assert pp["rejected"] >= 1, pp  # the hallucinated rename
        # Every accepted candidate's plan must decode as a PatchPlanDto
        # (ops + label). Feed the first accepted plan straight into
        # explain.patch to prove the shape is consumable.
        first_accepted = next(c for c in pp["candidates"] if c["accepted"])
        assert "ops" in first_accepted["plan"], first_accepted
        assert first_accepted["plan"]["ops"], first_accepted
        send(proc, {
            "jsonrpc": "2.0", "id": 44, "method": "explain.patch",
            "params": {
                "workspace_id": ws_pp,
                "plan": first_accepted["plan"],
                "candidate_outcomes": [],
                "validation_profile": "typed",
            },
        })
        pp_explain = recv(proc)["result"]
        # The LLM-proposed plan, after grounding + type-check, must
        # produce an `accepted` verdict. That is the loop closing: LLM
        # says *what to do*, the symbolic side proves it is safe.
        assert pp_explain["verdict"]["kind"] == "accepted", pp_explain
        cargo_stage_pp = next(
            e for e in pp_explain["evidence"]
            if e["kind"] == "stage" and e["name"] == "cargo_check"
        )
        assert cargo_stage_pp["ok"] is True, cargo_stage_pp

        shutil.rmtree(scratch6_root, ignore_errors=True)

        # ---- Phase 1.11 Step 2: scope-resolved rename via rust-analyzer.
        # The op is opt-in through the new `rename_function_typed`
        # variant. In CI here rust-analyzer isn't installed, so we
        # assert the *graceful-degradation* path: the preview must
        # return with a per-file PreviewError that names
        # rust-analyzer, and the filesystem must remain untouched.
        # Hosts with RA on PATH would instead return a diff — that
        # path is exercised by `cargo test -p aa-ra-client` against
        # the real binary.
        scratch7_root = tempfile.mkdtemp(prefix="aa-smoke-typed-rename-")
        scratch7_demo = os.path.join(scratch7_root, "demo")
        shutil.copytree("examples/rust-demo", scratch7_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 50, "method": "workspace.open",
            "params": {"root": scratch7_demo},
        })
        ws_typed_rn = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 51, "method": "patch.preview",
            "params": {
                "workspace_id": ws_typed_rn,
                "plan": {
                    "ops": [{
                        "op": "rename_function_typed",
                        "decl_file": "src/lib.rs",
                        "decl_line": 2,
                        "decl_character": 7,
                        "new_name": "sum",
                        "old_name": "add",
                    }],
                    "label": "smoke: scope-resolved add -> sum",
                },
            },
        })
        typed_rn_prev = recv(proc)["result"]
        # Two valid outcomes: either RA is installed and the preview
        # succeeds with at least one file change, or RA is absent and
        # the preview reports a PreviewError naming it. Both paths
        # confirm the typed variant is wired end-to-end; neither path
        # may panic or silently no-op without a diagnostic.
        if typed_rn_prev["files"]:
            # RA present — the preview must rewrite lib.rs.
            changed = [f["path"] for f in typed_rn_prev["files"]]
            assert "src/lib.rs" in changed, typed_rn_prev
        else:
            assert typed_rn_prev["errors"], typed_rn_prev
            assert any(
                "rust-analyzer" in e["message"] or "rename_function_typed" in e["message"]
                for e in typed_rn_prev["errors"]
            ), typed_rn_prev
        # FS must be untouched either way (preview never writes).
        with open(os.path.join(scratch7_demo, "src/lib.rs")) as f:
            assert "pub fn add(" in f.read(), "typed preview must not touch disk"
        shutil.rmtree(scratch7_root, ignore_errors=True)

        # ---- Phase 1.12: add_derive_to_struct — the first op that
        # isn't a rename. Proves the pipeline tolerates ops of another
        # shape end-to-end. We preview + apply `#[derive(Debug, Clone)]`
        # on `Counter`, assert the on-disk file gained the attribute,
        # and that the syntactic-stage revalidation still passes.
        scratch8_root = tempfile.mkdtemp(prefix="aa-smoke-add-derive-")
        scratch8_demo = os.path.join(scratch8_root, "demo")
        shutil.copytree("examples/rust-demo", scratch8_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 60, "method": "workspace.open",
            "params": {"root": scratch8_demo},
        })
        ws_ad = recv(proc)["result"]["workspace_id"]
        send(proc, {
            "jsonrpc": "2.0", "id": 61, "method": "patch.preview",
            "params": {
                "workspace_id": ws_ad,
                "plan": {
                    "ops": [{
                        "op": "add_derive_to_struct",
                        "type_name": "Counter",
                        "derives": ["Debug", "Clone"],
                        "files": [],
                    }],
                    "label": "smoke: add derive(Debug, Clone) to Counter",
                },
            },
        })
        ad_prev = recv(proc)["result"]
        assert ad_prev["total_replacements"] == 2, ad_prev
        assert len(ad_prev["files"]) == 1, ad_prev
        assert "+#[derive(Debug, Clone)]" in ad_prev["files"][0]["diff"], ad_prev

        send(proc, {
            "jsonrpc": "2.0", "id": 62, "method": "patch.apply",
            "params": {
                "workspace_id": ws_ad,
                "plan": {
                    "ops": [{
                        "op": "add_derive_to_struct",
                        "type_name": "Counter",
                        "derives": ["Debug", "Clone"],
                        "files": [],
                    }],
                    "label": "smoke: apply add_derive",
                },
            },
        })
        ad_app = recv(proc)["result"]
        assert ad_app["applied"] is True, ad_app
        with open(os.path.join(scratch8_demo, "src/lib.rs")) as f:
            content = f.read()
        assert "#[derive(Debug, Clone)]\npub struct Counter" in content, content

        # Idempotency: re-applying the same op is a no-op at the
        # replacement level (nothing new to add). The apply can still
        # record a commit because the shadow equals the original —
        # preview returns zero files and the preflight accepts.
        send(proc, {
            "jsonrpc": "2.0", "id": 63, "method": "patch.preview",
            "params": {
                "workspace_id": ws_ad,
                "plan": {
                    "ops": [{
                        "op": "add_derive_to_struct",
                        "type_name": "Counter",
                        "derives": ["Debug", "Clone"],
                        "files": [],
                    }],
                    "label": "smoke: re-apply add_derive (idempotent)",
                },
            },
        })
        ad_re = recv(proc)["result"]
        assert ad_re["total_replacements"] == 0, ad_re
        assert ad_re["files"] == [], ad_re

        shutil.rmtree(scratch8_root, ignore_errors=True)

        # ---- Phase 1.13: persistent rust-analyzer session pool. We
        # can't prove reuse observationally without RA installed, but
        # we can assert the plumbing: issuing two back-to-back typed
        # renames on the same workspace must not crash the daemon and
        # must return the same degraded-gracefully diagnostic in both
        # cases. Before the pool, two calls spawned two fresh
        # processes and each one surfaced the same handshake-EOF
        # error; after the pool, the pool's `spawn` attempt fails and
        # stays out of the cache, so the second call repeats the same
        # failure path. Either way, success is: no daemon death, no
        # silent no-op, a clear diagnostic.
        scratch9_root = tempfile.mkdtemp(prefix="aa-smoke-typed-pool-")
        scratch9_demo = os.path.join(scratch9_root, "demo")
        shutil.copytree("examples/rust-demo", scratch9_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 70, "method": "workspace.open",
            "params": {"root": scratch9_demo},
        })
        ws_pool = recv(proc)["result"]["workspace_id"]
        for round_id, msg_id in enumerate([71, 72], start=1):
            send(proc, {
                "jsonrpc": "2.0", "id": msg_id, "method": "patch.preview",
                "params": {
                    "workspace_id": ws_pool,
                    "plan": {
                        "ops": [{
                            "op": "rename_function_typed",
                            "decl_file": "src/lib.rs",
                            "decl_line": 2,
                            "decl_character": 7,
                            "new_name": f"sum_round_{round_id}",
                            "old_name": "add",
                        }],
                        "label": f"smoke: pool round {round_id}",
                    },
                },
            })
            pool_prev = recv(proc)["result"]
            # Either RA present (files populated) or absent (clear
            # error). Crash = fail. Silent empty = fail.
            if pool_prev["files"]:
                assert any(
                    f["path"] == "src/lib.rs" for f in pool_prev["files"]
                ), pool_prev
            else:
                assert pool_prev["errors"], pool_prev
                assert any(
                    "rust-analyzer" in e["message"]
                    or "rename_function_typed" in e["message"]
                    for e in pool_prev["errors"]
                ), pool_prev
        # FS must still be untouched.
        with open(os.path.join(scratch9_demo, "src/lib.rs")) as f:
            assert "pub fn add(" in f.read(), "pool preview must not touch disk"
        shutil.rmtree(scratch9_root, ignore_errors=True)

        # ---- Phase 1.14: memory surface (history / get / stats).
        # Apply two patches of different op kinds, then assert that
        # memory.history sees both, memory.stats groups them by op
        # kind and by profile, and memory.get round-trips a single
        # entry's full body.
        scratch10_root = tempfile.mkdtemp(prefix="aa-smoke-memory-")
        scratch10_demo = os.path.join(scratch10_root, "demo")
        shutil.copytree("examples/rust-demo", scratch10_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 80, "method": "workspace.open",
            "params": {"root": scratch10_demo},
        })
        ws_mem = recv(proc)["result"]["workspace_id"]
        # Apply #1: add_derive_to_struct on Counter.
        send(proc, {
            "jsonrpc": "2.0", "id": 81, "method": "patch.apply",
            "params": {
                "workspace_id": ws_mem,
                "plan": {
                    "ops": [{
                        "op": "add_derive_to_struct",
                        "type_name": "Counter",
                        "derives": ["Debug", "Clone"],
                        "files": [],
                    }],
                    "label": "smoke: memory add_derive",
                },
            },
        })
        mem_apply1 = recv(proc)["result"]
        assert mem_apply1["applied"] is True, mem_apply1
        first_commit = mem_apply1["commit_id"]
        # Apply #2: rename_function of `useless`.
        send(proc, {
            "jsonrpc": "2.0", "id": 82, "method": "patch.apply",
            "params": {
                "workspace_id": ws_mem,
                "plan": {
                    "ops": [{
                        "op": "rename_function",
                        "old_name": "useless",
                        "new_name": "unused",
                        "files": [],
                    }],
                    "label": "smoke: memory rename",
                },
                "validation_profile": "default",
            },
        })
        mem_apply2 = recv(proc)["result"]
        assert mem_apply2["applied"] is True, mem_apply2

        # memory.history: two entries, newest first, with op tags.
        send(proc, {
            "jsonrpc": "2.0", "id": 83, "method": "memory.history",
            "params": {"workspace_id": ws_mem},
        })
        hist = recv(proc)["result"]
        assert len(hist["items"]) == 2, hist
        tags = {
            t
            for it in hist["items"]
            for t in it["ops_summary"]
        }
        assert "rename_function" in tags, hist
        assert "add_derive_to_struct" in tags, hist
        # Filter by op tag: only add_derive.
        send(proc, {
            "jsonrpc": "2.0", "id": 84, "method": "memory.history",
            "params": {
                "workspace_id": ws_mem,
                "op_tag": "add_derive_to_struct",
            },
        })
        hist_filtered = recv(proc)["result"]
        assert len(hist_filtered["items"]) == 1, hist_filtered
        assert hist_filtered["items"][0]["commit_id"] == first_commit, hist_filtered

        # memory.get: full round-trip of the first commit.
        send(proc, {
            "jsonrpc": "2.0", "id": 85, "method": "memory.get",
            "params": {"workspace_id": ws_mem, "commit_id": first_commit},
        })
        got = recv(proc)["result"]
        assert got["commit_id"] == first_commit, got
        assert got["ops_summary"] == ["add_derive_to_struct"], got
        assert got["files"], got
        assert got["files"][0]["path"] == "src/lib.rs", got

        # memory.stats: two commits, both profiles default, rename +
        # add_derive each count 1, src/lib.rs touched twice.
        send(proc, {
            "jsonrpc": "2.0", "id": 86, "method": "memory.stats",
            "params": {"workspace_id": ws_mem},
        })
        stats = recv(proc)["result"]
        assert stats["commits"] == 2, stats
        assert stats["by_op_kind"].get("rename_function") == 1, stats
        assert stats["by_op_kind"].get("add_derive_to_struct") == 1, stats
        assert stats["by_validation_profile"].get("default") == 2, stats
        assert any(
            tf["path"] == "src/lib.rs" and tf["commit_count"] == 2
            for tf in stats["top_files"]
        ), stats
        shutil.rmtree(scratch10_root, ignore_errors=True)

        # ---- Phase 1.15: memory-biased llm.propose_patch. Apply an
        # add_derive_to_struct first so the journal shows that op kind
        # has landed, then ask for proposals *with* and *without*
        # include_memory and assert the memory-aware run produces at
        # least one extra candidate whose label is tagged
        # [memory-biased].
        scratch11_root = tempfile.mkdtemp(prefix="aa-smoke-memory-bias-")
        scratch11_demo = os.path.join(scratch11_root, "demo")
        shutil.copytree("examples/rust-demo", scratch11_demo)
        send(proc, {
            "jsonrpc": "2.0", "id": 90, "method": "workspace.open",
            "params": {"root": scratch11_demo},
        })
        ws_bias = recv(proc)["result"]["workspace_id"]
        # Seed the journal with one add_derive_to_struct commit.
        send(proc, {
            "jsonrpc": "2.0", "id": 91, "method": "patch.apply",
            "params": {
                "workspace_id": ws_bias,
                "plan": {
                    "ops": [{
                        "op": "add_derive_to_struct",
                        "type_name": "Counter",
                        "derives": ["Debug"],
                        "files": [],
                    }],
                    "label": "smoke: seed memory with add_derive",
                },
            },
        })
        seed_apply = recv(proc)["result"]
        assert seed_apply["applied"] is True, seed_apply
        # Index so the graph has function/struct facts.
        send(proc, {
            "jsonrpc": "2.0", "id": 92, "method": "workspace.index",
            "params": {"workspace_id": ws_bias},
        })
        recv(proc)
        # Find a valid anchor.
        send(proc, {
            "jsonrpc": "2.0", "id": 93, "method": "graph.query",
            "params": {"workspace_id": ws_bias, "pattern": "struct_def(F, Counter)"},
        })
        struct_rows = recv(proc)["result"]["bindings"]
        assert struct_rows, struct_rows
        struct_anchor = struct_rows[0]["F"]

        # Run #1: no memory.
        send(proc, {
            "jsonrpc": "2.0", "id": 94, "method": "llm.propose_patch",
            "params": {
                "workspace_id": ws_bias,
                "intent": "propose",
                "anchor_id": struct_anchor,
                "hops": 1,
            },
        })
        no_mem = recv(proc)["result"]
        assert not any(
            "[memory-biased]" in c["plan"]["label"]
            for c in no_mem["candidates"]
        ), no_mem

        # Run #2: include_memory=5. Must surface the biased candidate.
        send(proc, {
            "jsonrpc": "2.0", "id": 95, "method": "llm.propose_patch",
            "params": {
                "workspace_id": ws_bias,
                "intent": "propose",
                "anchor_id": struct_anchor,
                "hops": 1,
                "include_memory": 5,
            },
        })
        with_mem = recv(proc)["result"]
        biased = [
            c for c in with_mem["candidates"]
            if "[memory-biased]" in c["plan"]["label"]
        ]
        assert biased, with_mem
        # The biased candidate must be grounded (accepted) since
        # Counter exists as a struct_def in the graph.
        assert any(c["accepted"] for c in biased), biased
        # Running the same request again must hit the cache.
        send(proc, {
            "jsonrpc": "2.0", "id": 96, "method": "llm.propose_patch",
            "params": {
                "workspace_id": ws_bias,
                "intent": "propose",
                "anchor_id": struct_anchor,
                "hops": 1,
                "include_memory": 5,
            },
        })
        with_mem2 = recv(proc)["result"]
        assert with_mem2["cache_hit"] is True, with_mem2
        shutil.rmtree(scratch11_root, ignore_errors=True)

        # ---- Phase 1.18: remove_derive_from_struct, dual of 1.12's
        # add_derive_to_struct. Apply an add first, then remove every
        # derive one by one; assert the final file is byte-identical
        # to the pre-add state (full round-trip).
        scratch12_root = tempfile.mkdtemp(prefix="aa-smoke-remove-derive-")
        scratch12_demo = os.path.join(scratch12_root, "demo")
        shutil.copytree("examples/rust-demo", scratch12_demo)
        with open(os.path.join(scratch12_demo, "src/lib.rs")) as f:
            original_src = f.read()

        send(proc, {
            "jsonrpc": "2.0", "id": 100, "method": "workspace.open",
            "params": {"root": scratch12_demo},
        })
        ws_rd = recv(proc)["result"]["workspace_id"]

        # Add `Debug, Clone` to Counter.
        send(proc, {
            "jsonrpc": "2.0", "id": 101, "method": "patch.apply",
            "params": {
                "workspace_id": ws_rd,
                "plan": {
                    "ops": [{
                        "op": "add_derive_to_struct",
                        "type_name": "Counter",
                        "derives": ["Debug", "Clone"],
                        "files": [],
                    }],
                    "label": "smoke: add derive for dual test",
                },
            },
        })
        add_res = recv(proc)["result"]
        assert add_res["applied"] is True, add_res
        with open(os.path.join(scratch12_demo, "src/lib.rs")) as f:
            assert "#[derive(Debug, Clone)]\npub struct Counter" in f.read(), "add failed"

        # Remove `Clone` — partial removal, attr must survive.
        send(proc, {
            "jsonrpc": "2.0", "id": 102, "method": "patch.apply",
            "params": {
                "workspace_id": ws_rd,
                "plan": {
                    "ops": [{
                        "op": "remove_derive_from_struct",
                        "type_name": "Counter",
                        "derives": ["Clone"],
                        "files": [],
                    }],
                    "label": "smoke: remove Clone",
                },
            },
        })
        rem1 = recv(proc)["result"]
        assert rem1["applied"] is True, rem1
        with open(os.path.join(scratch12_demo, "src/lib.rs")) as f:
            after_partial = f.read()
        assert "#[derive(Debug)]\npub struct Counter" in after_partial, after_partial
        assert "Clone" not in after_partial.split("pub struct Counter")[0], after_partial

        # Remove the last remaining derive (`Debug`) — the whole
        # `#[derive(...)]` attribute line must vanish, leaving the
        # file byte-identical to the pre-add original.
        send(proc, {
            "jsonrpc": "2.0", "id": 103, "method": "patch.apply",
            "params": {
                "workspace_id": ws_rd,
                "plan": {
                    "ops": [{
                        "op": "remove_derive_from_struct",
                        "type_name": "Counter",
                        "derives": ["Debug"],
                        "files": [],
                    }],
                    "label": "smoke: remove last derive (drops attr)",
                },
            },
        })
        rem2 = recv(proc)["result"]
        assert rem2["applied"] is True, rem2
        with open(os.path.join(scratch12_demo, "src/lib.rs")) as f:
            after_full = f.read()
        assert after_full == original_src, (
            "remove-derive round-trip must restore bytes; "
            f"len(after)={len(after_full)} len(original)={len(original_src)}"
        )

        # Idempotence: removing an already-absent derive is a no-op
        # at the wire level (no files changed, `total_replacements=0`).
        send(proc, {
            "jsonrpc": "2.0", "id": 104, "method": "patch.preview",
            "params": {
                "workspace_id": ws_rd,
                "plan": {
                    "ops": [{
                        "op": "remove_derive_from_struct",
                        "type_name": "Counter",
                        "derives": ["Debug"],
                        "files": [],
                    }],
                    "label": "smoke: idempotent remove",
                },
            },
        })
        idem = recv(proc)["result"]
        assert idem["total_replacements"] == 0, idem
        assert idem["files"] == [], idem

        shutil.rmtree(scratch12_root, ignore_errors=True)

        # ---- Phase 1.21: inline_function. Preview substitutes every bare
        # call site of a pure single-body helper with its inlined form
        # (block-wrapped with `let` prelude) and removes the definition.
        # We use a scratch fixture with a small helper whose every call
        # site is bare — rust-demo's real helpers are referenced from
        # macro bodies (`assert_eq!(add(1,2), 3)`) which inline refuses.
        scratch21_root = tempfile.mkdtemp(prefix="aa-smoke-inline-")
        scratch21_demo = os.path.join(scratch21_root, "demo")
        os.makedirs(os.path.join(scratch21_demo, "src"))
        with open(os.path.join(scratch21_demo, "Cargo.toml"), "w") as f:
            f.write("[package]\nname = \"aa-inline-smoke\"\nversion = \"0.0.1\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n")
        inline_src = (
            "pub fn sq(x: i32) -> i32 { x * x }\n"
            "\n"
            "pub fn sum_of_squares(a: i32, b: i32) -> i32 {\n"
            "    sq(a) + sq(b)\n"
            "}\n"
        )
        with open(os.path.join(scratch21_demo, "src/lib.rs"), "w") as f:
            f.write(inline_src)

        send(proc, {
            "jsonrpc": "2.0", "id": 140, "method": "workspace.open",
            "params": {"root": scratch21_demo},
        })
        r = recv(proc)
        ws21 = r["result"]["workspace_id"]

        send(proc, {
            "jsonrpc": "2.0", "id": 141, "method": "workspace.index",
            "params": {"workspace_id": ws21},
        })
        recv(proc)

        send(proc, {
            "jsonrpc": "2.0", "id": 142, "method": "patch.preview",
            "params": {
                "workspace_id": ws21,
                "plan": {
                    "ops": [{"op": "inline_function", "function": "sq", "files": []}],
                    "label": "smoke: inline sq",
                },
            },
        })
        r = recv(proc)["result"]
        # 2 bare call sites + 1 definition removal = 3 byte-level edits.
        assert r["total_replacements"] == 3, r
        assert len(r["files"]) == 1, r
        # Shape check: inlined form with paren-wrap + let prelude.
        diff = r["files"][0]["diff"]
        assert "({ let x = a; x * x })" in diff, diff
        assert "({ let x = b; x * x })" in diff, diff
        assert "-pub fn sq(x" in diff, diff

        send(proc, {
            "jsonrpc": "2.0", "id": 143, "method": "patch.apply",
            "params": {
                "workspace_id": ws21,
                "plan": {
                    "ops": [{"op": "inline_function", "function": "sq", "files": []}],
                    "label": "smoke: inline sq apply",
                },
            },
        })
        r = recv(proc)["result"]
        assert r["applied"] is True, r
        with open(os.path.join(scratch21_demo, "src/lib.rs")) as f:
            applied_src = f.read()
        assert "pub fn sq" not in applied_src, applied_src
        assert "({ let x = a; x * x }) + ({ let x = b; x * x })" in applied_src, applied_src

        # Re-apply the same plan — now that `sq` is gone, the op is a
        # no-op and the preview should produce zero edits.
        send(proc, {
            "jsonrpc": "2.0", "id": 144, "method": "patch.preview",
            "params": {
                "workspace_id": ws21,
                "plan": {
                    "ops": [{"op": "inline_function", "function": "sq", "files": []}],
                    "label": "smoke: inline sq idempotent",
                },
            },
        })
        idem = recv(proc)["result"]
        assert idem["total_replacements"] == 0, idem
        assert idem["files"] == [], idem

        # Confirm memory.stats now records the `inline_function` op tag
        # under by_op_kind — proves the journal + stats wire path for
        # the new op kind is intact.
        send(proc, {
            "jsonrpc": "2.0", "id": 145, "method": "memory.stats",
            "params": {"workspace_id": ws21},
        })
        stats = recv(proc)["result"]
        assert stats["by_op_kind"].get("inline_function", 0) >= 1, stats

        shutil.rmtree(scratch21_root, ignore_errors=True)

        # ---- Phase 1.22: extract_function. Lift a contiguous run of
        # statements out of a free-standing fn body into a new helper.
        # Selection: lines 2..=3 of a 4-line parent fn; the trailing
        # `let _ = b;` (line 4) stays in the parent.
        scratch22_root = tempfile.mkdtemp(prefix="aa-smoke-extract-")
        scratch22_demo = os.path.join(scratch22_root, "demo")
        os.makedirs(os.path.join(scratch22_demo, "src"))
        with open(os.path.join(scratch22_demo, "Cargo.toml"), "w") as f:
            f.write("[package]\nname = \"aa-extract-smoke\"\nversion = \"0.0.1\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n")
        extract_src = (
            "pub fn parent(x: i32) {\n"
            "    let a = x + 1;\n"
            "    let b = a * 2;\n"
            "    let _ = b;\n"
            "}\n"
        )
        with open(os.path.join(scratch22_demo, "src/lib.rs"), "w") as f:
            f.write(extract_src)

        send(proc, {
            "jsonrpc": "2.0", "id": 150, "method": "workspace.open",
            "params": {"root": scratch22_demo},
        })
        ws22 = recv(proc)["result"]["workspace_id"]

        send(proc, {
            "jsonrpc": "2.0", "id": 151, "method": "workspace.index",
            "params": {"workspace_id": ws22},
        })
        recv(proc)

        extract_op = {
            "op": "extract_function",
            "source_file": "src/lib.rs",
            "start_line": 2,
            "end_line": 3,
            "new_name": "compute",
            "params": [{"name": "x", "type": "i32"}],
            "files": [],
        }

        send(proc, {
            "jsonrpc": "2.0", "id": 152, "method": "patch.preview",
            "params": {
                "workspace_id": ws22,
                "plan": {
                    "ops": [extract_op],
                    "label": "smoke: extract compute",
                },
            },
        })
        r = recv(proc)["result"]
        # Two byte-level edits: call-site replace + helper insertion.
        assert r["total_replacements"] == 2, r
        assert len(r["files"]) == 1, r
        diff = r["files"][0]["diff"]
        assert "compute(x);" in diff, diff
        assert "fn compute(x: i32)" in diff, diff
        assert "let a = x + 1;" in diff, diff
        assert "let b = a * 2;" in diff, diff

        send(proc, {
            "jsonrpc": "2.0", "id": 153, "method": "patch.apply",
            "params": {
                "workspace_id": ws22,
                "plan": {
                    "ops": [extract_op],
                    "label": "smoke: extract compute apply",
                },
            },
        })
        r = recv(proc)["result"]
        assert r["applied"] is True, r
        with open(os.path.join(scratch22_demo, "src/lib.rs")) as f:
            applied_src = f.read()
        assert "compute(x);" in applied_src, applied_src
        assert "fn compute(x: i32)" in applied_src, applied_src
        # The original two `let`s now live only in the helper, not in
        # the parent — extracting moves the bytes, doesn't duplicate.
        assert applied_src.count("let a = x + 1;") == 1, applied_src
        assert applied_src.count("let b = a * 2;") == 1, applied_src

        # Verify that the planner refuses a partial-statement
        # selection (lines that cut into the middle of a stmt). After
        # apply, lines 2..=3 cover the `compute(x);` call + `let _ = b;`
        # — both whole stmts, syntactically valid to extract a second
        # time even if semantically absurd, so we use a range we *know*
        # is partial: lines 5..=5 on the rewritten file land between
        # the parent's closing `}` and the helper, where there are no
        # whole stmts. The op must surface an error rather than write.
        send(proc, {
            "jsonrpc": "2.0", "id": 154, "method": "patch.preview",
            "params": {
                "workspace_id": ws22,
                "plan": {
                    "ops": [{
                        "op": "extract_function",
                        "source_file": "src/lib.rs",
                        "start_line": 5,
                        "end_line": 5,
                        "new_name": "should_refuse",
                        "params": [],
                        "files": [],
                    }],
                    "label": "smoke: extract refuses out-of-fn range",
                },
            },
        })
        refuse = recv(proc)["result"]
        assert refuse["total_replacements"] == 0, refuse
        assert refuse["errors"], refuse
        assert "free-standing fn" in refuse["errors"][0]["message"], refuse

        # Confirm memory.stats now records the `extract_function` op
        # tag — proves the journal + stats wire path for the new op
        # kind is intact, mirroring the 1.21 inline_function check.
        send(proc, {
            "jsonrpc": "2.0", "id": 155, "method": "memory.stats",
            "params": {"workspace_id": ws22},
        })
        stats = recv(proc)["result"]
        assert stats["by_op_kind"].get("extract_function", 0) >= 1, stats

        shutil.rmtree(scratch22_root, ignore_errors=True)

        # ---- Phase 1.23: change_signature. Reorder a free-standing
        # function's parameters and propagate the permutation to every
        # bare call site. The fixture has both a swap and a 3-arg
        # permutation so the daemon's wire round-trip exercises the
        # full plumbing for the new op kind.
        scratch23_root = tempfile.mkdtemp(prefix="aa-smoke-change-sig-")
        scratch23_demo = os.path.join(scratch23_root, "demo")
        os.makedirs(os.path.join(scratch23_demo, "src"))
        with open(os.path.join(scratch23_demo, "Cargo.toml"), "w") as f:
            f.write(
                "[package]\nname = \"aa-change-sig-smoke\"\nversion = \"0.0.1\"\nedition = \"2021\"\n"
                "\n[lib]\npath = \"src/lib.rs\"\n"
            )
        change_sig_src = (
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
            "\n"
            "pub fn caller() -> i32 { add(1, 2) + add(3, 4) }\n"
        )
        with open(os.path.join(scratch23_demo, "src/lib.rs"), "w") as f:
            f.write(change_sig_src)

        send(proc, {
            "jsonrpc": "2.0", "id": 160, "method": "workspace.open",
            "params": {"root": scratch23_demo},
        })
        ws23 = recv(proc)["result"]["workspace_id"]

        send(proc, {
            "jsonrpc": "2.0", "id": 161, "method": "workspace.index",
            "params": {"workspace_id": ws23},
        })
        recv(proc)

        # Swap params: from_index 1, then 0. Renames `a` -> `left`,
        # `b` -> `right` to also exercise the body-rename path.
        change_sig_op = {
            "op": "change_signature",
            "function": "add",
            "new_params": [
                {"from_index": 1, "rename": "right"},
                {"from_index": 0, "rename": "left"},
            ],
            "files": [],
        }

        send(proc, {
            "jsonrpc": "2.0", "id": 162, "method": "patch.preview",
            "params": {
                "workspace_id": ws23,
                "plan": {
                    "ops": [change_sig_op],
                    "label": "smoke: swap + rename add",
                },
            },
        })
        r = recv(proc)["result"]
        # 2 call sites + 1 signature + 2 body renames = 5 byte-level edits.
        assert r["total_replacements"] == 5, r
        assert len(r["files"]) == 1, r
        diff = r["files"][0]["diff"]
        assert "fn add(right: i32, left: i32)" in diff, diff
        assert "right + left" in diff or "left + right" in diff, diff
        assert "add(2, 1)" in diff, diff
        assert "add(4, 3)" in diff, diff

        send(proc, {
            "jsonrpc": "2.0", "id": 163, "method": "patch.apply",
            "params": {
                "workspace_id": ws23,
                "plan": {
                    "ops": [change_sig_op],
                    "label": "smoke: swap + rename add apply",
                },
            },
        })
        r = recv(proc)["result"]
        assert r["applied"] is True, r
        with open(os.path.join(scratch23_demo, "src/lib.rs")) as f:
            applied_src = f.read()
        assert "fn add(right: i32, left: i32)" in applied_src, applied_src
        assert "add(2, 1)" in applied_src, applied_src
        assert "add(4, 3)" in applied_src, applied_src

        # Refusal path: arity mismatch in new_params must surface as
        # a preview error, not a silent no-op.
        send(proc, {
            "jsonrpc": "2.0", "id": 164, "method": "patch.preview",
            "params": {
                "workspace_id": ws23,
                "plan": {
                    "ops": [{
                        "op": "change_signature",
                        "function": "add",
                        "new_params": [{"from_index": 0, "rename": None}],
                        "files": [],
                    }],
                    "label": "smoke: change_sig refuses arity drop",
                },
            },
        })
        refuse = recv(proc)["result"]
        assert refuse["total_replacements"] == 0, refuse
        assert refuse["errors"], refuse
        assert "permutation-only" in refuse["errors"][0]["message"], refuse

        # Confirm memory.stats now records the `change_signature` op
        # tag — proves the journal + stats wire path for the new op
        # kind is intact, mirroring the 1.21 / 1.22 checks above.
        send(proc, {
            "jsonrpc": "2.0", "id": 165, "method": "memory.stats",
            "params": {"workspace_id": ws23},
        })
        stats = recv(proc)["result"]
        assert stats["by_op_kind"].get("change_signature", 0) >= 1, stats

        shutil.rmtree(scratch23_root, ignore_errors=True)

        shutil.rmtree(scratch_root, ignore_errors=True)
        shutil.rmtree(scratch2_root, ignore_errors=True)
        shutil.rmtree(scratch3_root, ignore_errors=True)
        shutil.rmtree(scratch4_root, ignore_errors=True)
        shutil.rmtree(scratch5_root, ignore_errors=True)

        send(proc, {"jsonrpc": "2.0", "id": 99, "method": "session.shutdown"})
        recv(proc)
        proc.wait(timeout=5)
        print("daemon smoke test OK")
        return 0
    finally:
        if proc.poll() is None:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
