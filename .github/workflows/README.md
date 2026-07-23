# Workflow Strategy

The workflows in this directory implement Sustainable fork CI as a compatibility
patch over the inherited validation suite. Correctness depends only on standard
GitHub-hosted runners. BuildBuddy can accelerate Bazel work when its secret is
available, but the existing local-build and GitHub-cache paths remain the
fallback.

## Merge Gate

- `blocking-ci.yml` owns the version-controlled list of merge-blocking child
  workflows. After Stability certification, the active `main` ruleset requires
  only its aggregate `CI required` job and enforces strict base freshness.
- `bazel.yml` is the main pre-merge verification path for Rust code. Its native
  test matrix covers the three Essential platforms:
  - Linux x64 on `ubuntu-24.04`
  - macOS ARM64 on `macos-15`
  - Windows x64 on `windows-2025`
- Windows x64 Bazel tests are split across four runner shards, with local test
  concurrency capped to the runner's four vCPUs.
- Bazel `clippy` covers those same Essential platforms,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes
- `sdk.yml` runs Python and TypeScript SDK validation on `ubuntu-24.04`.

### Temporary Windows Capability Exclusion

The Windows-only
`legacy_workspace_write_delete_is_limited_to_writable_roots` test is
excluded from Electivus GitHub-hosted Windows jobs through 2026-08-23 in
[Electivus/electivus-codex#38](https://github.com/Electivus/electivus-codex/issues/38).
The standard `windows-2025` checkout ACL exposes the upstream deletion-boundary
bug tracked in
[openai/codex#32915](https://github.com/openai/codex/issues/32915). The
remaining native Windows Bazel tests stay merge-blocking. Affected jobs emit a
named notice, developer and other CI environments continue to run the test, and
the exclusion fails closed after its deadline.

## Extended Validation

- `postmerge-ci.yml` calls the full Rust and V8 suites after pushes to `main`.
  Both suites can also be dispatched against a non-default branch for Stability
  certification or a single diagnostic retry.
- `rust-ci-full.yml` is the full Cargo-native verification workflow:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - Windows ARM64 nextest archives cross-compiled on Windows x64, then replayed on native Windows ARM64 shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests
- Linux archive producers discard unused hosted-image toolchains and omit
  debuginfo from test artifacts so the inherited full suite fits standard
  runner disks. Linux ARM64 also caps Cargo parallelism to fit its memory
  budget.
- `v8-canary.yml` keeps V8 version resolution, source builds, checksums, staging,
  artifact pairing, and Cargo smoke coverage outside the pull-request Merge
  gate.

The Extended platform runner mapping is:

| Platform | Standard runner |
| --- | --- |
| Linux x64 | `ubuntu-24.04` |
| Linux ARM64 | `ubuntu-24.04-arm` |
| Windows x64 | `windows-2025` |
| Windows ARM64 | `windows-11-arm` |
| macOS ARM64 | `macos-15` |
| macOS Intel | `macos-15-intel` |

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Keep additional architectures, specialized build variants, and V8 canaries in
  Extended validation so they do not add pull-request latency.
