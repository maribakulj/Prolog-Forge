#!/usr/bin/env bash
# tooling/preflight.sh
#
# Local mirror of `.github/workflows/ci.yml`'s `build-and-test` job
# plus the supplementary jobs that ship with PR-A and PR-B. Run this
# before `git push` and your branch will pass CI bit-for-bit (modulo
# OS-specific surprises).
#
# Designed to be the single source of truth: any time a CI step is
# added in the workflow, mirror it here. Anything missing here is by
# definition not part of the contract a contributor signs up to keep
# green.
#
# Usage:
#   tooling/preflight.sh           # run everything that doesn't need
#                                  # extra binaries beyond the
#                                  # workspace's pinned toolchain.
#   tooling/preflight.sh --full    # additionally run audit, deny,
#                                  # and the rust-analyzer e2e test.
#                                  # Requires cargo-audit, cargo-deny,
#                                  # and `rust-analyzer` to be on
#                                  # PATH; otherwise the affected
#                                  # checks self-skip with a warning.

set -euo pipefail

# ---- CSI colour helpers (silent in non-TTY) ------------------------------
if [ -t 1 ]; then
  C_OK=$'\033[32m'
  C_FAIL=$'\033[31m'
  C_DIM=$'\033[2m'
  C_OFF=$'\033[0m'
else
  C_OK=""; C_FAIL=""; C_DIM=""; C_OFF=""
fi

run_step() {
  # $1 = label, $2... = command
  local label="$1"; shift
  printf "%s==>%s %s\n" "$C_DIM" "$C_OFF" "$label"
  if "$@"; then
    printf "    %s✓%s %s\n\n" "$C_OK" "$C_OFF" "$label"
  else
    printf "    %s✗ FAILED:%s %s\n" "$C_FAIL" "$C_OFF" "$label"
    printf "    Reproduce: %s\n" "$*"
    exit 1
  fi
}

skip_step() {
  printf "    %s•%s %s (skipped: %s)\n\n" "$C_DIM" "$C_OFF" "$1" "$2"
}

# ---- Mandatory checks (mirror `build-and-test` job exactly) --------------

run_step "cargo fmt --check"        cargo fmt --all -- --check
run_step "cargo clippy -D warnings" cargo clippy --workspace --all-targets -- -D warnings
run_step "cargo build"              cargo build --workspace --all-targets
run_step "cargo test"               cargo test --workspace
run_step "JSON schema parses"       python3 -c "import json; json.load(open('schemas/protocol.json'))"
run_step "daemon smoke"             python3 tooling/smoke/daemon_smoke.py

# ---- VS Code adapter (mirror `vscode-adapter` job) -----------------------

if command -v node >/dev/null 2>&1; then
  run_step "vscode adapter package.json" \
    node -e "JSON.parse(require('fs').readFileSync('adapters/vscode/package.json','utf8'))"
  for f in adapters/vscode/src/*.js; do
    run_step "vscode adapter syntax-check $(basename "$f")" node --check "$f"
  done
else
  skip_step "vscode adapter checks" "node not on PATH"
fi

# ---- Optional --full checks (mirror PR-A + PR-B supplementary jobs) ------

if [ "${1:-}" = "--full" ]; then
  if command -v cargo-audit >/dev/null 2>&1; then
    run_step "cargo audit" cargo audit
  else
    skip_step "cargo audit" "cargo-audit not installed (cargo install --locked --version 0.22.1 cargo-audit)"
  fi

  if command -v cargo-deny >/dev/null 2>&1; then
    run_step "cargo deny" cargo deny check all
  else
    skip_step "cargo deny" "cargo-deny not installed (cargo install --locked cargo-deny)"
  fi

  # MSRV — only if the pinned MSRV toolchain is installed.
  if rustup toolchain list 2>/dev/null | grep -q "^1.85.0"; then
    run_step "MSRV build (rust 1.85)" cargo +1.85.0 build --workspace --all-targets
  else
    skip_step "MSRV build (rust 1.85)" "1.85.0 toolchain not installed (rustup toolchain install 1.85.0)"
  fi

  # rust-analyzer e2e — only if RA is on PATH.
  if rust-analyzer --version >/dev/null 2>&1; then
    run_step "rust-analyzer e2e (real binary)" \
      env AA_REQUIRE_REAL_RA=1 cargo test -p aa-ra-client real_rust_analyzer_rename_when_available
  else
    skip_step "rust-analyzer e2e" "rust-analyzer not on PATH (rustup component add rust-analyzer)"
  fi
fi

printf "\n%s✓ preflight clean — safe to push%s\n" "$C_OK" "$C_OFF"
printf "%sNote:%s the release pipeline (.github/workflows/release.yml) is\n" "$C_DIM" "$C_OFF"
printf "      triggered by annotated tags, not by 'push' — see\n"
printf "      docs/RELEASING.md for the cut procedure.\n"
