# Workflow Strategy

The workflows in this directory implement Sustainable fork CI as a compatibility
patch over the inherited validation suite. Correctness depends only on standard
GitHub-hosted Linux runners. BuildBuddy can accelerate Bazel work when its
secret is available, while the existing local-build and GitHub-cache paths
remain the fallback.

## Linux Support Boundary

- Native Linux x64 on `ubuntu-24.04` is the current Essential platform.
- Linux ARM64 on `ubuntu-24.04-arm`, musl variants, release builds, and V8
  canaries are Extended validation.
- macOS and Windows remain Codex product platforms, but this fork does not
  select them in active validation matrices. They can return by restoring
  inherited jobs and widening matrices after their standard-runner paths are
  certified.

## Merge Gate

- `blocking-ci.yml` owns the version-controlled list of merge-blocking child
  workflows. After Stability certification, the active `main` ruleset requires
  only its aggregate `CI required` job and enforces strict base freshness.
- `bazel.yml` is the main pre-merge verification path for Rust code. It runs
  native Linux x64 Bazel `test` and `clippy`,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes
- `postgres-runtime-state-contracts.yml` provisions PostgreSQL 18 on Linux and
  runs the real-database Runtime State Namespace contract suite.
- `sdk.yml` runs Python and TypeScript SDK validation on `ubuntu-24.04`.
- `blocking-ci.yml` always runs the repository-owned Deep Linux eligibility
  classifier. The job succeeds with bounded `eligible` and `reason` outputs
  and a summary whether the change is eligible or explicitly irrelevant;
  comparison or classifier uncertainty defaults to eligible. `CI required`
  requires this decision alongside every existing child workflow.
- Only root `README.md`, `CODE_OF_CONDUCT.md`, `CONTRIBUTING.md`, and
  `SECURITY.md`; `docs/**`; and GitHub community metadata under
  `.github/CODEOWNERS`, `.github/ISSUE_TEMPLATE/**`,
  `.github/pull_request_template.md`, and `.github/PULL_REQUEST_TEMPLATE/**`
  are explicitly irrelevant to Deep Linux. All other paths, including unknown
  paths, are eligible.

## Extended Validation

- `postmerge-ci.yml` calls the full Rust and V8 suites after pushes to `main`.
  Both suites can also be dispatched against a non-default branch for Stability
  certification or a single diagnostic retry.
- `rust-ci-full.yml` retains Linux x64 and ARM64 Cargo `clippy`, nextest,
  GNU/musl, release-profile, and argument-comment-lint coverage. Docker
  remote-executor validation is deferred under #41 until the pinned baseline's
  integration fixtures are consistently remote-safe.
- Linux archive producers discard unused hosted-image toolchains and omit
  debuginfo from test artifacts so the inherited full suite fits standard
  runner disks. Linux ARM64 also caps Cargo parallelism to fit its memory
  budget.
- `v8-canary.yml` retains V8 version resolution, source builds, checksums,
  staging, and artifact-pair validation for Linux x64 and ARM64 GNU/musl
  targets. Native Cargo smoke runs for the GNU targets; musl coverage ends at
  the staged artifact pair. All V8 work stays outside the pull-request Merge
  gate.

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Keep additional Linux architectures, specialized build variants, and V8
  canaries in Extended validation so they do not add pull-request latency.
