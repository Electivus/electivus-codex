#!/usr/bin/env python3
"""Fail closed when the Essential Windows validation topology drifts."""

import argparse
import json
from pathlib import Path
import re
import sys

from check_postgres_archive_topology import EXTENDED_LANES
from check_postgres_archive_topology import FULL_LANES
from check_postgres_archive_topology import MERGE_LANES
from check_postgres_archive_topology import WINDOWS_LANES
from check_postgres_archive_topology import _block
from check_postgres_archive_topology import _job
from check_postgres_archive_topology import _planner_matrix
from check_postgres_archive_topology import _step
from check_rust_test_policy import EXPECTED_WINDOWS_SKIP_COUNTS
from check_rust_test_policy import ELECTIVUS_SKIP_BASELINE_COMMIT
from check_rust_test_policy import ELECTIVUS_UNCONDITIONAL_IGNORE_COUNT
from check_rust_test_policy import ELECTIVUS_UNCONDITIONAL_IGNORE_DIGEST
from check_rust_test_policy import WINDOWS_SKIP_BASELINE_COMMIT
from check_rust_test_policy import WINDOWS_SKIP_BASELINE_DIGEST
from check_rust_test_policy import _workflow_jobs


SOURCES = (
    ".github/workflows/bazel.yml",
    ".github/workflows/blocking-ci.yml",
    ".github/workflows/rust-ci-full.yml",
    ".github/workflows/rust-ci-full-nextest-platform.yml",
    ".github/workflows/v8-canary.yml",
    ".github/workflows/postmerge-ci.yml",
    ".github/workflows/repo-checks.yml",
    ".github/scripts/rust_ci_full_plan.py",
    ".github/scripts/rust_ci_full_result.py",
    ".github/scripts/v8_canary_result.py",
    ".github/scripts/run-bazel-ci.sh",
    ".github/scripts/check_rust_test_policy.py",
    ".github/ci-validation-inventory.json",
    ".github/windows-rust-skip-baseline.json",
    ".bazelrc",
    ".github/workflows/rust-ci.yml",
    ".github/scripts/copy-windows-ci-workspace.ps1",
)
WINDOWS_SCOPE_IF = (
    "inputs.validation_scope != 'essential' && "
    "inputs.validation_scope != 'release-only' && "
    "inputs.validation_scope != ''"
)
EXPECTED_INVENTORY = {
    "argument-comment-lint-windows-x64": 1,
    "lint-windows-x64-dev": 1,
    "lint-windows-x64-release": 1,
    "lint-windows-arm64-dev": 1,
    "lint-windows-arm64-release": 1,
    "tests-windows-x64-archive": 1,
    "tests-windows-x64-shards": 4,
    "tests-windows-arm64-archive": 1,
    "tests-windows-arm64-shards": 4,
    "bazel-windows-x64-test-shards": 4,
    "bazel-windows-x64-test-result": 1,
    "bazel-windows-x64-clippy": 1,
    "bazel-windows-x64-release": 1,
    "bazel-windows-result": 1,
    "v8-windows-x64-ptrcomp-sandbox": 1,
    "v8-windows-arm64-ptrcomp-sandbox": 1,
}


def validate_topology(
    bazel: str,
    blocking: str,
    rust: str,
    platform: str,
    v8: str,
    postmerge: str,
    repo_checks: str,
    planner: str,
    rust_result: str,
    v8_result: str,
    bazel_helper: str,
    skip_policy: str,
    inventory_source: str,
    baseline_source: str,
    bazelrc: str,
    fast_rust: str,
    workspace_copy: str,
) -> list[str]:
    rust_windows_x64 = _job(rust, "tests_windows_x64")
    rust_windows_arm64 = _job(rust, "tests_windows_arm64")
    rust_argument_lint = _job(rust, "argument_comment_lint_windows")
    rust_results = _job(rust, "results")
    archive = _job(platform, "archive")
    shards = _job(platform, "shard")
    shard_tests = _step(shards, "tests")
    platform_result = _job(platform, "result")
    bazel_shards = _job(bazel, "test-windows-shard")
    bazel_shard_test = _step(bazel_shards, "Bazel test shard")
    bazel_test_result = _job(bazel, "test-windows")
    bazel_clippy = _job(bazel, "clippy-windows")
    bazel_release = _job(bazel, "verify-release-build-windows")
    bazel_result = _job(bazel, "windows-result")
    v8_windows = _job(v8, "build-windows-source")
    v8_windows_cache = _step(v8_windows, "Restore upstream source-build cache")
    v8_terminal = _job(v8, "result")
    required = _job(blocking, "required")
    windows_cargo_call = _job(blocking, "windows-cargo")
    windows_bazel_call = _job(blocking, "windows-bazel")
    required_needs = set(
        re.findall(
            r"^      - ([A-Za-z_][A-Za-z0-9_-]*)\s*$",
            _block(required, r"^    needs:\s*$", r"^    [A-Za-z_][A-Za-z0-9_-]*:\s*"),
            re.MULTILINE,
        )
    )
    bazelrc_skip_filter_lines = {
        line
        for line in bazelrc.splitlines()
        if "CODEX_BAZEL_TEST_SKIP_FILTERS=" in line
    }

    try:
        inventory = json.loads(inventory_source)
        baseline = json.loads(baseline_source)
        windows_rows = [
            row
            for group in ("rustCiFull", "bazelWindows", "v8")
            for row in inventory[group]
            if "windows" in row.get("activeScopes", ())
        ]
        inventory_actual = {
            row["id"]: row["cardinality"]
            for row in windows_rows
            if row.get("disposition") == "promoted"
            and row.get("activeScopes") == ["windows", "full"]
        }
        baseline_counts = baseline["counts"]
        baseline_identities = [entry["identity"] for entry in baseline["entries"]]
    except (json.JSONDecodeError, KeyError, TypeError):
        inventory, baseline = {}, {}
        inventory_actual, windows_rows, baseline_counts, baseline_identities = {}, [], {}, []

    public_jobs = (
        rust_argument_lint,
        rust_windows_x64,
        rust_windows_arm64,
        bazel_shards,
        bazel_clippy,
        bazel_release,
        v8_windows,
    )
    checks = (
        (
            "exact Windows Cargo plan",
            _planner_matrix(planner, "WINDOWS_LINT_MATRIX") == WINDOWS_LANES
            and _planner_matrix(planner, "FULL_LINT_MATRIX") == FULL_LANES
            and set(FULL_LANES)
            == set(MERGE_LANES) | set(EXTENDED_LANES) | set(WINDOWS_LANES)
            and '"windows": (WINDOWS_LINT_MATRIX' in planner,
        ),
        (
            "explicit Windows planning outputs",
            all(
                name in planner and f"steps.scope.outputs.{name}" in rust
                for name in ("run_windows_x64", "run_windows_arm64")
            )
            and "unknown scope" in planner
            and "defaults fail-safe to full" in planner,
        ),
        (
            "public Windows runners",
            all("group:" not in job and "self-hosted" not in job for job in public_jobs)
            and "    runs-on: windows-2025" in rust_argument_lint.splitlines()
            and "      runner: windows-2025" in rust_windows_x64.splitlines()
            and "      runner: windows-11-arm" in rust_windows_arm64.splitlines()
            and "      archive_runner: windows-2025" in rust_windows_arm64.splitlines()
            and all("    runs-on: windows-2025" in job.splitlines() for job in (bazel_shards, bazel_clippy, bazel_release, v8_windows)),
        ),
        (
            "Windows argument comment lint ownership",
            rust_argument_lint.count("uses: ./.github/actions/run-argument-comment-lint") == 1
            and "needs.plan.outputs.run_windows_x64 == 'true'" in rust_argument_lint
            and "windows-2025" not in _job(rust, "argument_comment_lint_prebuilt")
            and "uses: ./.github/actions/run-argument-comment-lint" not in fast_rust,
        ),
        (
            "Windows nextest producer and consumers",
            "target: x86_64-pc-windows-msvc" in rust_windows_x64
            and "artifact_id: windows-x64" in rust_windows_x64
            and "target: aarch64-pc-windows-msvc" in rust_windows_arm64
            and "artifact_id: windows-arm64" in rust_windows_arm64
            and "test_threads: 1" in rust_windows_x64
            and "test_threads: 1" in rust_windows_arm64
            and "shard: [1, 2, 3, 4]" in shards
            and "uses: ./.github/actions/setup-msvc-env" in archive
            and "uses: ./.github/actions/setup-rusty-v8" in archive
            and "C:/codex-nextest-workspace/codex-rs" in archive
            and "C:/codex-nextest-workspace/codex-rs" in shards
            and archive.count("Copy checkout to stable Windows CI workspace") == 1
            and shards.count("Copy checkout to stable Windows CI workspace") == 1
            and (archive + shards).count("copy-windows-ci-workspace.ps1") == 2
            and "ItemType Junction" not in platform
            and all(
                marker in workspace_copy
                for marker in (
                    "robocopy.exe",
                    "/COPY:DAT",
                    "/DCOPY:DAT",
                    "$robocopyExitCode -gt 7",
                    "Stable Windows CI workspace is incomplete",
                )
            )
            and "/COPYALL" not in workspace_copy
            and all(
                marker in archive + shards
                for marker in (
                    "Build runtime test helpers",
                    "codex-windows-sandbox-setup.exe",
                    "codex-command-runner.exe",
                )
            ),
        ),
        (
            "retry-free JUnit evidence",
            "check_nextest_junit.py" in shards
            and "NEXTEST_JUNIT_FILE" in shards
            and "Confirm nextest shard result" in shards
            and "needs: [shard, postgres-contracts]" in platform_result
            and "continue-on-error:" not in shard_tests
            and "continue-on-error:" not in _step(shards, "Inspect nextest JUnit signal")
            and shard_tests.count("--retries 0") == 1
            and re.search(
                r'if \[\[ "\$\{RUNNER_OS\}" == "Windows" \]\]; then\s+'
                r'nextest_args\+\=\(--retries 0\)\s+fi',
                shard_tests,
            )
            is not None,
        ),
        (
            "Windows Cargo result fan-in",
            all(
                f"{name}," in rust_results
                and f"needs.{name}.result" in rust_results
                for name in (
                    "argument_comment_lint_windows",
                    "tests_windows_x64",
                    "tests_windows_arm64",
                )
            )
            and all(name in rust_result for name in ("argument_comment_lint_windows", "tests_windows_x64", "tests_windows_arm64"))
            and 'wanted = "success" if should_run else "skipped"' in rust_result,
        ),
        (
            "exact Windows Bazel topology",
            "matrix:\n        shard: [1, 2, 3, 4]" in bazel_shards
            and "fail-fast: false" in bazel_shards
            and "windows_bazel_shards.py" in bazel_shards
            and bazel_shards.count("Copy checkout to stable Windows Bazel workspace") == 1
            and bazel_shards.count("copy-windows-ci-workspace.ps1") == 1
            and "working-directory: C:/codex-bazel-workspace" in bazel_shard_test
            and "--windows-cross-compile" in bazel_shards + bazel_clippy + bazel_release
            and "--test_tag_filters=-argument-comment-lint" in bazel_shards
            and "list-bazel-clippy-targets.sh --windows-cross-compile" in bazel_clippy
            and "list-bazel-release-targets.sh" in bazel_release
            and all(WINDOWS_SCOPE_IF in job for job in (bazel_shards, bazel_clippy, bazel_release)),
        ),
        (
            "Windows Bazel dispatch contract",
            "workflow_dispatch:" in bazel
            and "default: full" in bazel
            and "type: choice" in bazel
            and "options:\n          - essential\n          - release-only\n          - windows\n          - full" in bazel,
        ),
        (
            "Windows Bazel fail-closed results",
            "needs: test-windows-shard" in bazel_test_result
            and "needs.test-windows-shard.result" in bazel_test_result
            and "needs: [test-windows, clippy-windows, verify-release-build-windows]" in bazel_result
            and all(f"needs.{name}.result" in bazel_result for name in ("test-windows", "clippy-windows", "verify-release-build-windows")),
        ),
        (
            "optional BuildBuddy local fallback",
            all("BUILDBUDDY_API_KEY: ${{ secrets.BUILDBUDDY_API_KEY }}" in job for job in (bazel_shards, bazel_clippy, bazel_release))
            and "BuildBuddy API key is not available; using local Bazel configuration." in bazel_helper
            and "ci_config=ci-windows" in bazel_helper
            and bazel_helper.count('bazel_run_args+=("--config=${ci_config}")') == 2
            and 'if [[ -n "${BUILDBUDDY_API_KEY:-}" || "${RUNNER_OS:-}" == "Windows" ]]; then' in bazel_helper
            and "post_config_bazel_args+=(--jobs=4)" in bazel_helper
            and "common:ci-windows-cross --local_test_jobs=4" in bazelrc,
        ),
        (
            "mandatory Windows V8 parity",
            v8_windows.count("- x86_64-pc-windows-msvc") == 1
            and v8_windows.count("- aarch64-pc-windows-msvc") == 1
            and "windows-11-arm" not in v8_windows
            and "timeout-minutes: 180" in v8_windows
            and "V8_FROM_SOURCE: \"1\"" in v8_windows
            and 'GN_ARGS: "symbol_level=0 v8_symbol_level=0"' in v8_windows
            and v8_windows_cache.count("windows-2025-sandbox-symbols0-cert2-") == 2
            and "key: rusty-v8-source-${{ matrix.target }}-windows-2025-sandbox-symbols0-cert2-${{ hashFiles" in v8_windows_cache
            and "rusty-v8-source-${{ matrix.target }}-windows-2025-sandbox-symbols0-cert2-\n" in v8_windows_cache
            and "stage-upstream-release-pair" in v8_windows
            and "WINDOWS_BUILD_RESULT: ${{ needs.build-windows-source.result }}" in v8_terminal
            and "windows_build_result != \"success\"" in v8_result,
        ),
        (
            "CI required Windows fan-in",
            "validation_scope: windows" in windows_cargo_call
            and "validation_scope: windows" in windows_bazel_call
            and {"windows-cargo", "windows-bazel", "v8-canary"}.issubset(required_needs)
            and "name: CI required" in required,
        ),
        (
            "Windows inventory binding",
            len(windows_rows) == len(inventory_actual) == len(EXPECTED_INVENTORY)
            and inventory_actual == EXPECTED_INVENTORY
            and inventory.get("outOfBoundary") == ["macOS"],
        ),
        (
            "fixed Windows skip baseline",
            baseline.get("upstreamCommit") == WINDOWS_SKIP_BASELINE_COMMIT
            and baseline.get("entriesSha256") == WINDOWS_SKIP_BASELINE_DIGEST
            and baseline_counts == EXPECTED_WINDOWS_SKIP_COUNTS
            and len(baseline_identities) == len(set(baseline_identities)) == 84
            and baseline.get("electivusUnconditionalBaseline")
            == {
                "commit": ELECTIVUS_SKIP_BASELINE_COMMIT,
                "count": ELECTIVUS_UNCONDITIONAL_IGNORE_COUNT,
                "sha256": ELECTIVUS_UNCONDITIONAL_IGNORE_DIGEST,
            }
            and "windows-rust-skip-baseline.json" in skip_policy
            and "validate_windows_skip_baseline" in skip_policy
            and WINDOWS_SKIP_BASELINE_DIGEST in skip_policy
            and ELECTIVUS_UNCONDITIONAL_IGNORE_DIGEST in skip_policy
            and "check_rust_test_policy.py" in repo_checks,
        ),
        (
            "no new Windows test filters",
            bazelrc_skip_filter_lines
            == {
                "common:ci-windows --test_env=CODEX_BAZEL_TEST_SKIP_FILTERS=suite::code_mode::code_mode_can_call_hidden_dynamic_tools",
                "common:ci-windows-cross --test_env=CODEX_BAZEL_TEST_SKIP_FILTERS=command_safety::powershell_parser::tests::,suite::code_mode::code_mode_can_call_hidden_dynamic_tools",
            },
        ),
        (
            "required repository topology check",
            "python3 .github/scripts/check_windows_ci_topology.py" in repo_checks,
        ),
        (
            "no postmerge Windows duplication",
            "test-windows-native-main" not in bazel
            and "windows-2025" not in postmerge
            and "windows-11-arm" not in postmerge
            and "validation_scope: windows" not in postmerge,
        ),
    )
    return [
        f"Windows CI topology drift: {label}"
        for label, valid in checks
        if not valid
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    issues = _workflow_jobs(repo, "blocking-ci.yml")[1]
    try:
        issues.extend(
            validate_topology(
                *[(repo / path).read_text(encoding="utf-8") for path in SOURCES]
            )
        )
    except (OSError, UnicodeError) as error:
        issues.append(f"cannot read Windows CI sources: {error}")
    if issues:
        print(
            "Windows CI topology failed:\n"
            + "\n".join(f"- {issue}" for issue in issues),
            file=sys.stderr,
        )
        return 1
    print("Windows CI topology passed: x64 and ARM64 are Essential public-runner platforms")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
