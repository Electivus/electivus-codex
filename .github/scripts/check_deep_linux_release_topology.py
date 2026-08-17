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
from check_postgres_archive_topology import FULL_LANES
from check_postgres_archive_topology import MERGE_LANES
from check_postgres_archive_topology import _planner_matrix


NAMES = ("bazel.yml", "blocking-ci.yml", "rust-ci-full.yml", "repo-checks.yml")
WORKFLOWS = tuple(f".github/workflows/{name}" for name in NAMES)
SOURCES = WORKFLOWS + (
    ".github/scripts/rust_ci_full_plan.py",
    ".github/scripts/rust_ci_full_result.py",
)
BAZEL_CONCURRENCY_GROUP = (
    "concurrency-group::${{ github.workflow }}::"
    "${{ github.event.pull_request.number > 0 && "
    "format('pr-{0}', github.event.pull_request.number) || github.ref_name }}::"
    "${{ inputs.validation_scope || 'essential' }}"
    "${{ github.ref_name == 'main' && format('::{0}', github.run_id) || ''}}"
)
ACTIONLINT_INSTALL_COMMANDS = (
    "set -euo pipefail",
    'mkdir -p "$GOBIN"',
    "go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.7",
    '"$GOBIN/actionlint" -version',
    'echo "$GOBIN" >> "$GITHUB_PATH"',
)


def _actionlint_install_contract(step: str) -> bool:
    run = _block(
        step,
        r"^        run: \|\s*$",
        r"^        [A-Za-z_][A-Za-z0-9_-]*:\s*",
    )
    executable_lines = [
        line.strip()
        for line in run.splitlines()[1:]
        if line.strip() and not line.lstrip().startswith("#")
    ]
    return (
        "uses: taiki-e/install-action@" not in step
        and "GOBIN: ${{ runner.temp }}/actionlint/bin" in step
        and "shell: bash" in step
        and executable_lines == list(ACTIONLINT_INSTALL_COMMANDS)
    )


def _named_steps_are_adjacent(job: str, before: str, after: str) -> bool:
    step_names: list[str | None] = []
    for line in job.splitlines():
        if not line.startswith("      - "):
            continue
        prefix = "      - name: "
        step_names.append(line.removeprefix(prefix) if line.startswith(prefix) else None)
    return (
        step_names.count(before) == 1
        and step_names.count(after) == 1
        and step_names.index(after) == step_names.index(before) + 1
    )


def validate_topology(
    bazel: str,
    blocking: str,
    rust: str,
    repo_checks: str,
    planner: str,
    result_helper: str,
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
    bazel_actionlint = _step(bazel_test, "Install actionlint")
    bazel_actionlint_consumer = _step(
        bazel_test, "Check rusty_v8 MODULE.bazel checksums"
    )
    repo_job = _job(repo_checks, "build-test")
    repo_actionlint = _step(repo_job, "Install actionlint")
    repo_actionlint_consumer = _step(repo_job, "Test GitHub helper scripts")
    bazel_actionlint_ready = _named_steps_are_adjacent(
        bazel_test,
        "Install actionlint",
        "Check rusty_v8 MODULE.bazel checksums",
    ) and "just test-github-scripts" in bazel_actionlint_consumer
    repo_actionlint_ready = _named_steps_are_adjacent(
        repo_job,
        "Install actionlint",
        "Test GitHub helper scripts",
    ) and "just test-github-scripts" in repo_actionlint_consumer
    concurrency = _block(bazel, r"^concurrency:\s*$", r"^jobs:\s*$")
    group_lines = re.findall(r"^  group: (.+)$", concurrency, re.MULTILINE)
    concurrency_group = group_lines[0] if len(group_lines) == 1 else ""
    checks = (
        ("Bazel concurrency scope", concurrency_group == BAZEL_CONCURRENCY_GROUP and "cancel-in-progress: ${{ github.ref_name != 'main' }}" in concurrency),
        ("Bazel scope fails safe", re.search(r"validation_scope:\n\s+description: .*\n\s+required: false\n\s+default: essential\n\s+type: string", bazel) is not None and "inputs.validation_scope != 'release-only' && inputs.validation_scope != 'windows'" in bazel_test and "inputs.validation_scope != 'release-only' && inputs.validation_scope != 'windows'" in bazel_clippy and "inputs.validation_scope != 'essential' && inputs.validation_scope != 'windows' && inputs.validation_scope != ''" in bazel_release),
        ("Bazel essential scheduling", "if: ${{ inputs.validation_scope != 'release-only' && inputs.validation_scope != 'windows' }}" in bazel_test and "if: ${{ inputs.validation_scope != 'release-only' && inputs.validation_scope != 'windows' }}" in bazel_clippy and "verify-release-build" not in bazel_test + bazel_clippy),
        ("Bazel release scheduling", "if: ${{ inputs.validation_scope != 'essential' && inputs.validation_scope != 'windows' && inputs.validation_scope != '' }}" in bazel_release and "github.event_name == 'push'" not in bazel_release),
        ("Bazel release target", "os: ubuntu-24.04" in bazel_release and "target: x86_64-unknown-linux-gnu" in bazel_release and "runs-on: ${{ matrix.os }}" in bazel_release),
        ("Bazel release assertions", bazel_release.count("--compilation_mode=fastbuild") == 1 and bazel_release.count("--@rules_rust//rust/settings:extra_rustc_flag=-Cdebug-assertions=no") == 1 and bazel_release.count("--@rules_rust//rust/settings:extra_exec_rustc_flag=-Cdebug-assertions=no") == 1),
        ("Bazel release targets", "list-bazel-release-targets.sh" in bazel_release and "BUILDBUDDY_API_KEY: ${{ secrets.BUILDBUDDY_API_KEY }}" in bazel_release and "run-bazel-ci.sh" in bazel_release),
        ("Bazel release bwrap", "Verify Bazel builds bwrap" in bazel_release and "//codex-rs/bwrap:bwrap" in bazel_release),
        ("Bazel release logs", "actions/upload-artifact@" in bazel_release and "bazel-execution-logs-verify-release-build-${{ matrix.target }}" in bazel_release and "${{ runner.temp }}/bazel-execution-logs" in bazel_release),
        ("hosted actionlint install", _actionlint_install_contract(bazel_actionlint) and _actionlint_install_contract(repo_actionlint)),
        ("actionlint consumer adjacency", bazel_actionlint_ready and repo_actionlint_ready),
        ("Bazel release promotion", "uses: ./.github/workflows/bazel.yml" in normal_bazel and "validation_scope: essential" in normal_bazel and "needs: deep-linux-eligibility" in release_gate and "needs.deep-linux-eligibility.result == 'success'" in release_gate and "needs.deep-linux-eligibility.outputs.eligible == 'true'" in release_gate and "uses: ./.github/workflows/bazel.yml" in release_gate and "validation_scope: release-only" in release_gate),
        ("bounded Bazel result", "needs: [deep-linux-eligibility, deep-linux-bazel-release]" in release_result and "if: ${{ always() }}" in release_result and "timeout-minutes: 10" in release_result and "ELIGIBILITY_RESULT: ${{ needs.deep-linux-eligibility.result }}" in release_result and "ELIGIBLE: ${{ needs.deep-linux-eligibility.outputs.eligible }}" in release_result and "VALIDATION_LABEL: Deep Linux Bazel release" in release_result and "VALIDATION_RESULT: ${{ needs.deep-linux-bazel-release.result }}" in release_result and "set -euo pipefail" in release_result and "deep_linux_result.py | tee -a \"$GITHUB_STEP_SUMMARY\"" in release_result),
        ("independent required results", all(f"- {name}" in required for name in ("bazel", "deep-linux-bazel-release-result", "deep-linux-cargo-result")) and "- deep-linux-bazel-release\n" not in required and "- deep-linux-cargo\n" not in required),
        ("merge Cargo matrix", _planner_matrix(planner, "MERGE_GATE_LINT_MATRIX") == MERGE_LANES),
        ("full Cargo matrix", _planner_matrix(planner, "FULL_LINT_MATRIX") == FULL_LANES),
        ("Cargo release build and lint", "if: ${{ matrix.profile == 'release' }}" in release_build and "cargo build --workspace --target ${{ matrix.target }} --profile release --timings" in release_build and "if: ${{ matrix.profile == 'release' }}" in release_clippy and "cargo clippy --workspace --target ${{ matrix.target }} --tests --profile release --timings -- -D warnings" in release_clippy and "if: ${{ matrix.profile == 'dev' }}" in dev_clippy),
        ("Cargo release timings", "if: always() && matrix.profile == 'release'" in build_timings and "cargo-timings-rust-ci-build-${{ matrix.target }}-release" in build_timings and "cargo-timing.html" in build_timings and "cargo-timings-rust-ci-clippy-${{ matrix.target }}-${{ matrix.profile }}" in lint),
        ("scope-aware Cargo aggregate", "rust_ci_full_result.py" in rust_results and "plan expected success" in result_helper and 'wanted = "success" if should_run else "skipped"' in result_helper and "actual != wanted" in result_helper),
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
        sources = [(repo / path).read_text(encoding="utf-8") for path in SOURCES]
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
