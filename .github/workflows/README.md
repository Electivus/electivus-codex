# Workflow Strategy

The workflows in this directory implement Sustainable fork CI as a compatibility
patch over the inherited validation suite. Correctness depends only on standard
GitHub-hosted Linux runners. BuildBuddy can accelerate Bazel work when its
secret is available, while the existing local-build and GitHub-cache paths
remain the fallback.

## Disabled Code Scanning Policy

CodeQL is intentionally disabled for the Electivus fork. On 2026-08-30,
`Analyze (rust)` took 51m02s on PR #193, and the maintainer explicitly accepted
the security tradeoff of removing that latency from admission. At that decision
point, 93 CodeQL alerts remained open, including 42 high-or-critical alerts.
This policy does not dismiss those alerts or represent them as remediated.

The repository default setup is `not-configured`, and the active
`Protect-Main` ruleset has no `code_scanning` rule or CodeQL status
requirement. The independent errors-threshold code-quality gate and
`CI required` remain enforced. `.github/scripts/check_codeql_disabled.py`
prevents workflow manifests, local actions, and repository-owned scripts from
reintroducing CodeQL actions, `security-events` permission (including
`write-all`), or code-scanning authority implicitly. The guard evaluates
parsed YAML scalar values, build/task recipes, and repository-wide automation
implementations (including text executables without extensions) so equivalent
quoted, multiline, delegated, and shell-continuation forms cannot bypass the
policy.

Re-enabling CodeQL requires a new explicit specification revision and separate
authorization for the corresponding ruleset mutation.

## Linux Support Boundary

- Required checks run against GitHub's synthetic merge commit, not the pull
  request head alone. This includes changes already on `main` and catches
  conflicts before they reach the branch.
- Native Linux x64 on `ubuntu-24.04` is the current Essential platform.
- Linux ARM64 on `ubuntu-24.04-arm` and remaining build variants are Extended
  validation. Promoted release, x64 test, and V8 lanes are not repeated after
  merge.
- macOS and Windows remain Codex product platforms, but this fork does not
  select them in active validation matrices. They can return by restoring
  inherited jobs and widening matrices after their standard-runner paths are
  certified.

## Electivus Linux And Windows Release

`electivus-release.yml` publishes the fork's Linux and Windows GitHub Releases
without invoking OpenAI-owned signing, package registries, R2, WinGet, or
deployment environments. It builds these targets on standard GitHub-hosted
runners:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`

Linux binaries receive keyless Sigstore bundles. Windows binaries are unsigned
and can display an unverified-publisher warning. macOS is intentionally outside
this release boundary. Public binary and package filenames remain compatible
with upstream names such as `codex-x86_64-unknown-linux-musl.zst`; only the Git
tag and GitHub Release identity use the Electivus prefix.

Keep `workspace.package.version` at `0.0.0` on normal development branches.
Create a release-only commit from the exact merged source commit that changes
only that version in `codex-rs/Cargo.toml`. Leave `codex-rs/Cargo.lock`
unchanged and do not merge the release commit into the normal branch:

```bash
git switch --detach <source-commit>
# Change only workspace.package.version in codex-rs/Cargo.toml to 0.1.0.
git add codex-rs/Cargo.toml
git commit -m "Release 0.1.0"
git tag -a electivus-v0.1.0 -m "Release 0.1.0"
git push origin electivus-v0.1.0
```

The tag push starts `electivus-release.yml`. Its `electivus-v` version must
match the workspace version, and the tagged commit must be a one-parent
release-only commit whose parent still reports `0.0.0`. A version containing a
suffix, such as `0.148.0-alpha.5`, is published as a GitHub prerelease and never
changes the repository's latest stable release. A version without a suffix is
published as the latest stable release.

Use the release commit message as the release notes. For an upstream
synchronization release, record at least the immutable upstream commit, its
`rust-v` release, the synchronization pull request, the merged fork source
commit, and a comparison URL. The workflow appends the exact release commit
and the Linux/Windows signing boundary.

The terminal verification job checks the stable/prerelease classification,
the absence of macOS assets, and required Linux and Windows package and binary
assets before the workflow is considered successful.

## Manual Unsigned Windows Release

`rust-release-windows-unsigned.yml` builds the Windows x64 and ARM64 release
assets on standard GitHub-hosted runners. This fork-specific workflow does not
use the inherited self-hosted runner groups or Azure Trusted Signing, so its
executables are unsigned and Windows may show an unverified-publisher warning.

Keep `workspace.package.version` at `0.0.0` on normal development branches. As
in the upstream release process, create a release-only commit from the source
commit that changes only that version in `codex-rs/Cargo.toml`; leave
`codex-rs/Cargo.lock` unchanged, do not merge the release commit into the normal
branch, and tag the release commit with the matching `windows-v` version:

```bash
git switch --detach <source-commit>
# Change only workspace.package.version in codex-rs/Cargo.toml to 0.1.0.
git add codex-rs/Cargo.toml
git commit -m "Release 0.1.0"
git tag -a windows-v0.1.0 -m "Release 0.1.0"
git push origin windows-v0.1.0
```

The requested tag must already exist and its `windows-v` version must match
`workspace.package.version` at that tagged commit. Run the workflow from the
default branch while passing the release tag explicitly:

```bash
gh workflow run rust-release-windows-unsigned.yml \
  --repo Electivus/electivus-codex \
  --ref main \
  -f release_tag=windows-v0.1.0 \
  -f publish_release=false
```

The fork-specific `windows-v` prefix intentionally avoids triggering the
inherited full-release workflow, which owns the upstream `rust-v*` tag family.

The default stores unsigned target archives and Python runtime wheels as
workflow artifacts for 30 days. Set `publish_release=true` only when those
assets should also be attached to a public Windows-only GitHub prerelease; the
workflow never marks an unsigned release as latest. The combined Electivus
release calls this workflow with `publish_release=false` and publishes the
Windows output together with Linux assets in one release.

## Merge Gate

- `blocking-ci.yml` owns the version-controlled list of merge-blocking child
  workflows. After Stability certification, the active `main` ruleset requires
  only its aggregate `CI required` job and enforces strict base freshness.
- `bazel.yml` runs native Linux x64 Bazel `test` and `clippy` as the Essential
  path, including the generated Rust test binaries needed to lint inline
  `#[cfg(test)]` code. Eligible Deep Linux changes independently run its
  release-only scope, preserving the fastbuild no-debug-assertion targets and
  bwrap compilation.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes
- `sdk.yml` runs Python and TypeScript SDK validation on `ubuntu-24.04`.
- `blocking-ci.yml` always runs the repository-owned Deep Linux eligibility
  classifier. The job succeeds with bounded `eligible` and `reason` outputs
  and a summary whether the change is eligible or explicitly irrelevant;
  comparison or classifier uncertainty defaults to eligible. `CI required`
  requires this decision alongside every existing child workflow. Eligible
  changes call `rust-ci-full.yml` with the `merge-gate` scope: native x64 GNU
  dev clippy, native x64 GNU release build plus clippy, x64 musl release build
  plus clippy, and the x64 nextest archive's four ordinary shards plus
  PostgreSQL consumer. Irrelevant changes skip both expensive release calls;
  independent Bazel and Cargo result jobs accept only the exact
  eligible/success or irrelevant/skipped pair and fail closed otherwise.
- `v8-canary.yml` is independently Change-triggered. Known ordinary Codex and
  documentation changes finish through metadata only; V8-relevant, unknown,
  indeterminate, or manual runs require its exact eight-leg Linux matrix and a
  bounded terminal result.
- Bazel-backed gate jobs that can rebuild V8 from a cold cache have a 90-minute
  job limit. This is a failure-containment bound rather than the 120-minute p95
  feedback objective; cached runs should finish sooner, and every selected
  check remains required.
- Only root `README.md`, `CODE_OF_CONDUCT.md`, `CONTRIBUTING.md`, and
  `SECURITY.md`; `docs/**`; and GitHub community metadata under
  `.github/CODEOWNERS`, `.github/ISSUE_TEMPLATE/**`,
  `.github/pull_request_template.md`, and `.github/PULL_REQUEST_TEMPLATE/**`
  are explicitly irrelevant to Deep Linux. All other paths, including unknown
  paths, are eligible.

## Extended Validation

- A normal push to `main` enters only `postmerge-ci.yml`, which calls
  `rust-ci-full.yml` with the `extended` scope. That scope runs x64 musl dev,
  ARM64 musl dev, ARM64 GNU dev, and ARM64 musl release lint/build lanes plus
  ARM64 nextest. General checks, x64 nextest/PostgreSQL, promoted release lanes,
  Bazel no-debug release validation, and V8 are not repeated.
- `rust-ci-full.yml` resolves `merge-gate`, `extended`, and `full` through the
  repository-owned planner. Empty direct-dispatch input resolves to `full`, and
  an unknown nonempty scope fails safe to `full`. Manual dispatch and the
  opt-in `**full-ci**` branch trigger therefore retain the complete suite for
  Stability certification or diagnosis. Stability certification requires one
  complete retry-free successful Merge gate run plus one successful run of
  every retained Extended validation suite for the same rollout/workflow
  version. The aggregate accepts only `success` for planned children and
  `skipped` for every unplanned child.
- In `full` and `merge-gate`, native Linux x64 builds one archive and matching
  runtime-helper artifact identity for four
  ordinary partitioned consumers plus one PostgreSQL 18 consumer. The fifth
  consumer runs the explicit 108 database-contract and two process-contract
  inventory across `codex-state`, `codex-thread-store`, `codex-app-server`,
  `codex-app-server-transport`, `codex-memories-write`, and `codex-cli`, with
  nextest concurrency fixed at four. ARM64 keeps only the four ordinary
  consumers. Docker
  remote-executor validation is deferred under #41 until the pinned baseline's
  integration fixtures are consistently remote-safe.
- Linux archive producers discard unused hosted-image toolchains and omit
  debuginfo from test artifacts so the inherited full suite fits standard
  runner disks. Linux ARM64 also caps Cargo parallelism to fit its memory
  budget.
- `v8-canary.yml` retains V8 version resolution, source builds, checksums,
  staging, and artifact-pair validation for Linux x64 and ARM64 GNU/musl
  targets. Native Cargo smoke runs for the GNU targets; musl coverage ends at
  the staged artifact pair. Relevant source changes also retain the narrower
  upstream Windows x64 and ARM64 sandbox source-build smoke jobs. It remains
  Change-triggered in the Merge gate and complete on manual dispatch, but is
  not called by postmerge.

### Validation Inventory

`.github/ci-validation-inventory.json` is the machine-checked source that
accounts for every active Full Rust family and all eight Linux V8 legs exactly
once. The two narrower upstream Windows V8 source-build jobs are checked by the
V8 topology validator instead of expanding the fork's Full Rust platform scope.

| Family or lanes                                                            |   Cardinality | Disposition                         | Active scope                        |
| -------------------------------------------------------------------------- | ------------: | ----------------------------------- | ----------------------------------- |
| Format + benchmark smoke                                                   |             2 | Promoted (existing Rust/Bazel gate) | `full` manual only                  |
| Cargo shear                                                                |             1 | Promoted (existing Rust gate)       | `full` manual only                  |
| Argument-comment-lint package + prebuilt                                   |             2 | Promoted (existing Rust/Bazel gate) | `full` manual only                  |
| x64 GNU dev; x64 GNU release; x64 musl release lint/build                  |             3 | Promoted by #86/#87                 | `merge-gate`, `full`                |
| x64 musl dev; ARM64 musl dev; ARM64 GNU dev; ARM64 musl release lint/build |             4 | Retained                            | `extended`, `full`                  |
| x64 nextest four shards + PostgreSQL consumer                              |             5 | Promoted by #86                     | `merge-gate`, `full`                |
| ARM64 nextest shards                                                       |             4 | Retained                            | `extended`, `full`                  |
| x64/ARM64 x GNU/musl x release/ptrcomp-sandbox V8                          |             8 | Promoted by #88                     | Change-triggered Merge gate, manual |
| Windows V8 sandbox source-build smoke                                      |             2 | Retained upstream                   | Change-triggered, manual            |
| macOS and Windows Full Rust validation                                     | 0 active legs | Out of boundary                     | None                                |

## Test Signal Integrity

Every full-Rust nextest shard treats its JUnit XML as a required result, including when the nextest command fails.
`check_nextest_junit.py` fails closed on missing, malformed, or wrong-root reports; nonzero failure/error counts;
testcase failures; and the retry elements `flakyFailure`, `flakyError`, `rerunFailure`, and `rerunError`, with or
without XML namespaces. A retry-assisted pass therefore stays red while logs and JUnit artifacts are uploaded.
The PostgreSQL archive consumer uses the same checker and explicit outcome composition as the four ordinary
consumers, and additionally requires exactly 110 executed JUnit testcases. Producer Cargo timings and all five
consumers' JUnit durations and failure diagnostics remain available as artifacts and job logs. The former
standalone PostgreSQL Merge-gate job is retired because eligible changes now reuse this x64 Cargo archive path.

`rust-test-policy.toml` inventories tracked Rust test ignores by source path, following test function, and normalized
attribute or condition. New, changed, duplicate, stale, or unclassified occurrences fail repository checks. Review
and assign every exact identity with its source change; names and reason text never classify it automatically.

### #89 Test Reactivation Certification

`test-certification.yml` runs Test reactivation certification only for the two temporary #89 identities. Its two
Linux x64 matrix jobs are independent; each performs 20 ordered executions on one full candidate SHA with nextest
retries disabled and exactly one JUnit testcase required. The first command or JUnit failure stops only that test's
sequence. A rerun attempt, incomplete manifest, changed SHA, retry evidence, cancellation, or skipped testcase cannot
certify. Each job retains its manifest and JUnit reports for 90 days and links the independent outcome in the summary.

After an operator is authorized to begin hosted certification, the workflow can first run from a candidate that is
not yet the default branch by creating the narrowly named ref without rewriting the candidate commit:

```bash
git push origin <40-character-candidate-sha>:refs/heads/certification/issue-89
```

After this workflow exists on `main`, dispatch the default-branch workflow definition against an immutable candidate:

```bash
gh workflow run test-certification.yml --ref main -f candidate_sha=<40-character-candidate-sha>
```

For each test, retain the workflow run URL and download its
`test-certification-<test-id>-<candidate-sha>` artifact. Verify each manifest and its 20 retained JUnit files with:

```bash
python3 .github/scripts/verify_test_certification.py verify <manifest.json> \
  --expected-sha <40-character-candidate-sha>
```

These commands describe the later hosted phase; no hosted evidence is asserted by this repository change. Only after
both final-candidate manifests pass may a follow-up remove the two `#[ignore]` attributes and their
`temporary-certification` policy entries, restoring both tests to ordinary native x64 archive selection.

The allowed ignore categories have these narrow boundaries:

| Category                   | Boundary                                                     |
| -------------------------- | ------------------------------------------------------------ |
| `helper-process`           | Subprocess entry point invoked by another test.              |
| `live-external-api`        | Two opt-in tests calling the real OpenAI API.                |
| `manual-smoke`             | Tmux/local-binary resize checks run by an operator.          |
| `schema-generation`        | Explicit app-server fixture generation and inherited Windows shell-snapshot cases. |
| `out-of-boundary-platform` | Conditionally inapplicable product-platform case.            |
| `specialized-environment`  | PostgreSQL 18 or another named process-contract environment. |
| `pending-behavior-change`  | Only two inherited compaction follow-up expectations.        |
| `temporary-certification`  | Only the two #89 tests named in `rust-test-policy.toml`.     |

`quarantined-checks.toml` starts empty. Each record needs an exact `check_identity`, narrow `scope`, evidence,
justification, an existing workflow and top-level job, an exact GitHub issue or pull-request URL, and TOML dates.
Repository checks install pinned actionlint first; quarantine validation requires it to accept the exact workflow
before the Python policy helper verifies the named top-level job exists. Missing actionlint or lint failures fail closed.
Identities must be unique; wildcard or blanket scopes fail. Expiry must be on or after the start, no more than seven
days later, and not elapsed. Quarantine is temporary relocation, never a silent pass or stopped validation.

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Keep additional Linux architectures and unpromoted specialized build
  variants in Extended validation so they do not add pull-request latency.
