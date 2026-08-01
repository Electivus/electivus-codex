#!/usr/bin/env python3
"""Fail closed when the Deep Linux release Merge gate topology drifts."""

import argparse
from pathlib import Path
import re
import sys

from check_postgres_archive_topology import _block
from check_postgres_archive_topology import _job
from check_postgres_archive_topology import _step
from check_rust_test_policy import _workflow_jobs


NAMES = ("bazel.yml", "blocking-ci.yml", "rust-ci-full.yml", "repo-checks.yml")
WORKFLOWS = tuple(f".github/workflows/{name}" for name in NAMES)

MERGE_MATRIX = '[{"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-gnu","profile":"dev"},{"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-gnu","profile":"release"},{"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-musl","profile":"release"}]'
FULL_MATRIX = '[{"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-musl","profile":"dev"},{"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-gnu","profile":"dev"},{"runner":"ubuntu-24.04-arm","target":"aarch64-unknown-linux-musl","profile":"dev"},{"runner":"ubuntu-24.04-arm","target":"aarch64-unknown-linux-gnu","profile":"dev"},{"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-musl","profile":"release"},{"runner":"ubuntu-24.04-arm","target":"aarch64-unknown-linux-musl","profile":"release"},{"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-gnu","profile":"release"}]'


def validate_topology(
    bazel: str, blocking: str, rust: str, repo_checks: str
) -> list[str]:
    bazel_test = _job(bazel, "test")
    bazel_clippy = _job(bazel, "clippy")
    bazel_release = _job(bazel, "verify-release-build")
    normal_bazel = _job(blocking, "bazel")
    release_gate = _job(blocking, "deep-linux-bazel-release")
    release_result = _job(blocking, "deep-linux-bazel-release-result")
    required = _job(blocking, "required")
    lint = _job(rust, "lint_build")
    rust_results = _job(rust, "results")
    release_build = _step(lint, "cargo build (release)")
    release_clippy = _step(lint, "cargo clippy (release)")
    dev_clippy = _step(lint, "cargo clippy (dev)")
    build_timings = _step(lint, "Upload Cargo timings (build)")
    concurrency = _block(bazel, r"^concurrency:\s*$", r"^jobs:\s*$")
    checks = (
        ("Bazel concurrency scope", concurrency.count("::${{ inputs.validation_scope || 'essential' }}") == 1 and "github.event.pull_request.number" in concurrency and "github.ref_name" in concurrency and "github.ref_name == 'main'" in concurrency and "github.run_id" in concurrency and "cancel-in-progress: ${{ github.ref_name != 'main' }}" in concurrency),
        ("Bazel scope fails safe", re.search(r"validation_scope:\n\s+description: .*\n\s+required: false\n\s+default: essential\n\s+type: string", bazel) is not None and "inputs.validation_scope != 'release-only'" in bazel_test and "inputs.validation_scope != 'release-only'" in bazel_clippy and "inputs.validation_scope != 'essential' && inputs.validation_scope != ''" in bazel_release),
        ("Bazel essential scheduling", "if: ${{ inputs.validation_scope != 'release-only' }}" in bazel_test and "if: ${{ inputs.validation_scope != 'release-only' }}" in bazel_clippy and "verify-release-build" not in bazel_test + bazel_clippy),
        ("Bazel release scheduling", "if: ${{ inputs.validation_scope != 'essential' && inputs.validation_scope != '' }}" in bazel_release and "github.event_name == 'push'" not in bazel_release),
        ("Bazel release target", "os: ubuntu-24.04" in bazel_release and "target: x86_64-unknown-linux-gnu" in bazel_release and "runs-on: ${{ matrix.os }}" in bazel_release),
        ("Bazel release assertions", bazel_release.count("--compilation_mode=fastbuild") == 1 and bazel_release.count("--@rules_rust//rust/settings:extra_rustc_flag=-Cdebug-assertions=no") == 1 and bazel_release.count("--@rules_rust//rust/settings:extra_exec_rustc_flag=-Cdebug-assertions=no") == 1),
        ("Bazel release targets", "list-bazel-release-targets.sh" in bazel_release and "BUILDBUDDY_API_KEY: ${{ secrets.BUILDBUDDY_API_KEY }}" in bazel_release and "run-bazel-ci.sh" in bazel_release),
        ("Bazel release bwrap", "Verify Bazel builds bwrap" in bazel_release and "//codex-rs/bwrap:bwrap" in bazel_release),
        ("Bazel release logs", "actions/upload-artifact@" in bazel_release and "bazel-execution-logs-verify-release-build-${{ matrix.target }}" in bazel_release and "${{ runner.temp }}/bazel-execution-logs" in bazel_release),
        ("Bazel release promotion", "uses: ./.github/workflows/bazel.yml" in normal_bazel and "validation_scope: essential" in normal_bazel and "needs: deep-linux-eligibility" in release_gate and "needs.deep-linux-eligibility.result == 'success'" in release_gate and "needs.deep-linux-eligibility.outputs.eligible == 'true'" in release_gate and "uses: ./.github/workflows/bazel.yml" in release_gate and "validation_scope: release-only" in release_gate),
        ("bounded Bazel result", "needs: [deep-linux-eligibility, deep-linux-bazel-release]" in release_result and "if: ${{ always() }}" in release_result and "timeout-minutes: 10" in release_result and "ELIGIBILITY_RESULT: ${{ needs.deep-linux-eligibility.result }}" in release_result and "ELIGIBLE: ${{ needs.deep-linux-eligibility.outputs.eligible }}" in release_result and "VALIDATION_LABEL: Deep Linux Bazel release" in release_result and "VALIDATION_RESULT: ${{ needs.deep-linux-bazel-release.result }}" in release_result and "set -euo pipefail" in release_result and "deep_linux_result.py | tee -a \"$GITHUB_STEP_SUMMARY\"" in release_result),
        ("independent required results", all(f"- {name}" in required for name in ("bazel", "deep-linux-bazel-release-result", "deep-linux-cargo-result")) and "- deep-linux-bazel-release\n" not in required and "- deep-linux-cargo\n" not in required),
        ("merge Cargo matrix", MERGE_MATRIX in lint and '"target":"x86_64-unknown-linux-musl","profile":"dev"' not in lint.split("||", 1)[0]),
        ("full Cargo matrix", FULL_MATRIX in lint),
        ("Cargo release build and lint", "if: ${{ matrix.profile == 'release' }}" in release_build and "cargo build --workspace --target ${{ matrix.target }} --profile release --timings" in release_build and "if: ${{ matrix.profile == 'release' }}" in release_clippy and "cargo clippy --workspace --target ${{ matrix.target }} --tests --profile release --timings -- -D warnings" in release_clippy and "if: ${{ matrix.profile == 'dev' }}" in dev_clippy),
        ("Cargo release timings", "if: always() && matrix.profile == 'release'" in build_timings and "cargo-timings-rust-ci-build-${{ matrix.target }}-release" in build_timings and "cargo-timing.html" in build_timings and "cargo-timings-rust-ci-clippy-${{ matrix.target }}-${{ matrix.profile }}" in lint),
        ("scope-aware Cargo aggregate", "needs.lint_build.result }}' == 'success'" in rust_results and "needs.tests_linux_x64.result }}' == 'success'" in rust_results),
        ("release repository check", "python3 .github/scripts/check_deep_linux_release_topology.py" in repo_checks),
    )
    return [f"Deep Linux release topology drift: {label}" for label, valid in checks if not valid]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    issues: list[str] = []
    # Bazel and full-Rust retain inherited actionlint diagnostics that are
    # compared separately; validate the aggregate YAML here and every required
    # release block structurally below.
    for name in (NAMES[1],):
        _, workflow_issues = _workflow_jobs(repo, name)
        issues.extend(workflow_issues)
    try:
        sources = [(repo / path).read_text(encoding="utf-8") for path in WORKFLOWS]
    except (OSError, UnicodeError) as error:
        issues.append(f"cannot read release workflows: {error}")
    else:
        issues.extend(validate_topology(*sources))
    if issues:
        print("Deep Linux release topology failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
        return 1
    print("Deep Linux release topology passed: independent Bazel release and three-lane Cargo gate")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
