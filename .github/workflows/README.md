# Workflow Strategy

The workflows in this directory implement Sustainable fork CI as a compatibility
patch over the inherited validation suite. Correctness depends only on standard
GitHub-hosted Linux runners. BuildBuddy can accelerate Bazel work when its
secret is available, while the existing local-build and GitHub-cache paths
remain the fallback.

## Linux Support Boundary

- Native Linux x64 on `ubuntu-24.04` is the current Essential platform.
- Linux ARM64 on `ubuntu-24.04-arm` and remaining build variants are Extended
  validation. Promoted release, x64 test, and V8 lanes are not repeated after
  merge.
- macOS and Windows remain Codex product platforms, but this fork does not
  select them in active validation matrices. They can return by restoring
  inherited jobs and widening matrices after their standard-runner paths are
  certified.

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
  Stability certification or diagnosis. Its aggregate accepts only `success`
  for planned children and `skipped` for every unplanned child.
- In `full` and `merge-gate`, native Linux x64 builds one archive and matching
  runtime-helper artifact identity for four
  ordinary partitioned consumers plus one PostgreSQL 18 consumer. The fifth
  consumer runs the explicit 107 database-contract and two process-contract
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
  the staged artifact pair. It remains Change-triggered in the Merge gate and
  complete on manual dispatch, but is not called by postmerge.

### Validation Inventory

`.github/ci-validation-inventory.json` is the machine-checked source that
accounts for every active Full Rust family and all eight V8 legs exactly once.

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
| macOS and Windows                                                          | 0 active legs | Out of boundary                     | None                                |

## Test Signal Integrity

Every full-Rust nextest shard treats its JUnit XML as a required result, including when the nextest command fails.
`check_nextest_junit.py` fails closed on missing, malformed, or wrong-root reports; nonzero failure/error counts;
testcase failures; and the retry elements `flakyFailure`, `flakyError`, `rerunFailure`, and `rerunError`, with or
without XML namespaces. A retry-assisted pass therefore stays red while logs and JUnit artifacts are uploaded.
The PostgreSQL archive consumer uses the same checker and explicit outcome composition as the four ordinary
consumers, and additionally requires exactly 109 executed JUnit testcases. Producer Cargo timings and all five
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
`test-certification-<test-id>-<candidate-sha>` artifact. Verify each downloaded `manifest.json` independently with:

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
| `schema-generation`        | Only two inherited Windows shell-snapshot generation cases.  |
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
