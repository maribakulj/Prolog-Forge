# Releasing AYE-AYE

The release pipeline lives in
[`.github/workflows/release.yml`](../.github/workflows/release.yml).
This document is the human-facing companion: what minting a release
looks like, what gets published, and what to do when something goes
wrong.

## What a release contains

Every release attaches the following assets to a GitHub Release:

| Asset | Target | Notes |
|---|---|---|
| `aa-x86_64-unknown-linux-gnu.tar.gz` | x86_64 Linux (glibc) | most common dev / CI host |
| `aa-aarch64-unknown-linux-gnu.tar.gz` | aarch64 Linux (glibc) | AWS Graviton, Pi 4/5, recent Hetzner |
| `aa-x86_64-apple-darwin.tar.gz` | Intel Mac | shrinking but still supported |
| `aa-aarch64-apple-darwin.tar.gz` | Apple Silicon | M1/M2/M3 |
| `aa-daemon-<target>.tar.gz` × 4 | same matrix | the JSON-RPC daemon |
| `aye-aye-vscode-<version>.vsix` | any | drop into VS Code via *Install from VSIX…* |
| `SHA256SUMS` | — | one line per asset, sorted by filename |

Each tarball includes the binary + `LICENSE` + `README.md`, so
distribution is self-contained (Apache-2.0 requires the licence to
travel with the binary).

**Windows is intentionally absent.** `aa-ra-client` and
`aa-core::apply` use POSIX fork+exec and atomic-rename patterns;
adding Windows builds requires Windows-specific code paths that
aren't shipped yet.

**`crates.io` publishing is not part of this workflow** at the
project's `0.0.1` alpha stage. Adding it later is one job guarded by
a `CARGO_REGISTRY_TOKEN` secret.

## Cutting a release

Releases are triggered by an annotated git tag matching `v*`.
Semver is enforced by convention, not by the workflow:

```bash
# 0. Make sure main is green.
git checkout main && git pull
tooling/preflight.sh --full

# 1. Bump versions in lockstep:
#    - workspace.package.version   in  Cargo.toml
#    - every internal path-dep version in  [workspace.dependencies]
#    - adapters/vscode/package.json `version`  (if the adapter changed)
$EDITOR Cargo.toml adapters/vscode/package.json
cargo update --workspace        # picks up the new version

# 2. Commit + tag.
git commit -am "release v0.1.0"
git tag -a v0.1.0 -m "v0.1.0 — short summary"

# 3. Push.
git push origin main
git push origin v0.1.0          # this triggers the workflow
```

The workflow will:

1. Build all four cross-compile targets in parallel
   (~5 min cold-cache, ~2 min warm).
2. Package the VS Code adapter as `.vsix`.
3. Compute `SHA256SUMS`.
4. Create the GitHub Release with auto-generated notes from commit
   messages since the previous tag.

Pre-release tags (`-rc.N`, `-alpha.N`, `-beta.N`) are flagged as
prereleases on GitHub so they don't show up as "Latest".

## Dry-run

To exercise the matrix without minting a release (e.g. before
landing a workflow change), trigger
`workflow_dispatch` from the Actions tab with `dry_run: true`.
All build + package jobs run; the publish job is skipped. Artefacts
are available from the workflow run page for 7 days.

## Verifying a download

```bash
curl -L -O https://github.com/maribakulj/AYE-AYE/releases/download/v0.1.0/aa-x86_64-unknown-linux-gnu.tar.gz
curl -L -O https://github.com/maribakulj/AYE-AYE/releases/download/v0.1.0/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
```

## When something goes wrong

- **One target fails, the others succeed.** `fail-fast: false` on
  the matrix means partial successes upload their artefacts to the
  workflow run. Investigate the failing target locally with
  `cargo build --release --target <target>` (install the linker
  with `apt install gcc-aarch64-linux-gnu` for the Linux ARM
  cross), commit the fix, re-tag with `vX.Y.Z+1` and re-trigger.
  GitHub doesn't allow re-running a tag-triggered workflow against
  a different commit; the pragmatic recovery is a new patch tag.

- **The release exists but assets are missing.** Re-run only the
  failed jobs from the workflow run UI. `softprops/action-gh-release@v2`
  re-attaches missing files idempotently.

- **VS Code installer rejects the `.vsix`.** Most likely a typo in
  `adapters/vscode/package.json`'s manifest. The `vscode-adapter`
  CI job catches structural issues but not all VS Code-specific
  schema rules. Validate locally with `vsce package --no-yarn` if
  you have `vsce` installed.
