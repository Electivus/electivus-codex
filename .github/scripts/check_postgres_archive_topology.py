#!/usr/bin/env python3
"""Fail closed when the full-Rust PostgreSQL archive topology drifts."""

import argparse
from pathlib import Path
import re
import sys
from check_rust_test_policy import _workflow_jobs


NAMES = (
    "rust-ci-full-nextest-platform.yml", "postgres-runtime-state-contracts.yml",
    "rust-ci-full.yml", "repo-checks.yml", "blocking-ci.yml",
)
WORKFLOWS = tuple(f".github/workflows/{name}" for name in NAMES)
SOURCES = WORKFLOWS + (
    ".github/scripts/rust_ci_full_plan.py",
    ".github/scripts/rust_ci_full_result.py",
)
LANE = re.compile(r'LintLane\("([^"]+)", "([^"]+)", "([^"]+)"\)')
MERGE_LANES = (
    ("ubuntu-24.04", "x86_64-unknown-linux-gnu", "dev"),
    ("ubuntu-24.04", "x86_64-unknown-linux-gnu", "release"),
    ("ubuntu-24.04", "x86_64-unknown-linux-musl", "release"),
)
EXTENDED_LANES = (
    ("ubuntu-24.04", "x86_64-unknown-linux-musl", "dev"),
    ("ubuntu-24.04-arm", "aarch64-unknown-linux-musl", "dev"),
    ("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "dev"),
    ("ubuntu-24.04-arm", "aarch64-unknown-linux-musl", "release"),
)
WINDOWS_LANES = (
    ("windows-2025", "x86_64-pc-windows-msvc", "dev"),
    ("windows-2025", "x86_64-pc-windows-msvc", "release"),
    ("windows-11-arm", "aarch64-pc-windows-msvc", "dev"),
    ("windows-11-arm", "aarch64-pc-windows-msvc", "release"),
)
FULL_LANES = (
    EXTENDED_LANES[0],
    MERGE_LANES[0],
    EXTENDED_LANES[1],
    EXTENDED_LANES[2],
    MERGE_LANES[2],
    EXTENDED_LANES[3],
    MERGE_LANES[1],
    *WINDOWS_LANES,
)
NEXTEST_JUNIT_ENV = (
    "NEXTEST_JUNIT_FILE: "
    "${{ github.workspace }}/codex-rs/target/nextest/default/junit.xml"
)
NEXTEST_INSTALL_ACTION = (
    "taiki-e/install-action@44c6d64aa62cd779e873306675c7a58e86d6d532"
)
NEXTEST_TOOL = "nextest@0.9.103"
RUSTY_V8_SETUP_ACTION = "./.github/actions/setup-rusty-v8"
STANDALONE_NEXTEST_CONDITION = "${{ inputs.artifact_id == '' }}"
ARCHIVE_NEXTEST_CONDITION = "${{ inputs.artifact_id != '' }}"


def _planner_matrix(source: str, name: str) -> tuple[tuple[str, str, str], ...]:
    match = re.search(
        rf"^{re.escape(name)} = \(\n(?P<body>.*?)^\)$",
        source,
        re.MULTILINE | re.DOTALL,
    )
    if not match:
        return ()
    body = match.group("body")
    if lanes := tuple(LANE.findall(body)):
        return lanes
    matrices = {
        "MERGE_GATE": _planner_matrix(source, "MERGE_GATE_LINT_MATRIX"),
        "EXTENDED": _planner_matrix(source, "EXTENDED_LINT_MATRIX"),
        "WINDOWS": _planner_matrix(source, "WINDOWS_LINT_MATRIX"),
    }
    lanes = []
    token = re.compile(
        r"(?:(MERGE_GATE|EXTENDED|WINDOWS)_LINT_MATRIX\[(\d)\])"
        r"|(?:\*(MERGE_GATE|EXTENDED|WINDOWS)_LINT_MATRIX)"
    )
    for match in token.finditer(body):
        indexed_matrix, index, expanded_matrix = match.groups()
        if expanded_matrix:
            lanes.extend(matrices[expanded_matrix])
        else:
            lanes.append(matrices[indexed_matrix][int(index)])
    return tuple(lanes)


def _block(text: str, start: str, following: str) -> str:
    match = re.search(start, text, re.MULTILINE)
    if match is None:
        return ""
    after = re.search(following, text[match.end() :], re.MULTILINE)
    end = match.end() + after.start() if after else len(text)
    return text[match.start() : end]


def _job(workflow: str, name: str) -> str:
    return _block(
        workflow,
        rf"^  {re.escape(name)}:\s*$",
        r"^  [A-Za-z_][A-Za-z0-9_-]*:\s*$",
    )


def _step(job: str, name: str) -> str:
    return _block(
        job,
        rf"^      - name: {re.escape(name)}\s*$",
        r"^      -(?:\s|$)",
    )


def _checkout(job: str) -> str:
    return _block(
        job,
        r"^      - uses: actions/checkout@",
        r"^      -(?:\s|$)",
    )


def _normalize_scalar(value: str) -> str:
    value = re.sub(r"\s+#.*$", "", value).strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
        return value[1:-1]
    return value


def _step_value(step: str, field: str) -> str | None:
    prefixes = (
        rf"^(?:      - {re.escape(field)}:|        {re.escape(field)}:)"
        if field in {"if", "uses"}
        else rf"^          {re.escape(field)}:"
    )
    match = re.search(rf"{prefixes}\s*(?P<value>.+?)\s*$", step, re.MULTILINE)
    return _normalize_scalar(match.group("value")) if match else None


def _action_input(step: str, field: str) -> str | None:
    if _step_value(step, "uses") is None:
        return None
    inputs = _block(
        step,
        r"^(?:      - with:|        with:)[ \t]*(?:#.*)?$",
        r"^        [A-Za-z_][A-Za-z0-9_-]*:",
    )
    match = re.search(
        rf"^          {re.escape(field)}:\s*(?P<value>.+?)\s*$",
        inputs,
        re.MULTILINE,
    )
    return _normalize_scalar(match.group("value")) if match else None


def _steps(block: str) -> tuple[str, ...]:
    sequence = _block(
        block,
        r"^    steps:\s*$",
        r"^    [A-Za-z_][A-Za-z0-9_-]*:\s*$",
    )
    starts = tuple(
        match.start()
        for match in re.finditer(r"^      -(?:\s|$)", sequence, re.MULTILINE)
    )
    return tuple(
        sequence[
            start : starts[index + 1] if index + 1 < len(starts) else len(sequence)
        ]
        for index, start in enumerate(starts)
    )


def _nextest_installers(block: str) -> tuple[str, ...]:
    installers = []
    for step in _steps(block):
        tool = _action_input(step, "tool")
        if tool is not None and (tool == "nextest" or tool.startswith("nextest@")):
            installers.append(step)
    return tuple(installers)


def _step_condition(step: str) -> str | None:
    return _step_value(step, "if")


def _pinned_nextest(installers: tuple[str, ...]) -> bool:
    if len(installers) != 1:
        return False
    installer = installers[0]
    return (
        _step_value(installer, "uses") == NEXTEST_INSTALL_ACTION
        and _action_input(installer, "tool") == NEXTEST_TOOL
        and _action_input(installer, "version") is None
    )


def _artifact(step: str, action: str, name: str, path: str) -> bool:
    return f"uses: actions/{action}-artifact@" in step and f"          name: {name}" in step and f"          path: {path}" in step


def _identity(block: str) -> bool:
    return "NEXTEST_ARCHIVE_FILE: nextest-${{ inputs.artifact_id }}.tar.zst" in block and "TEST_HELPERS_ARTIFACT: nextest-test-helpers-${{ inputs.artifact_id }}" in block


def validate_topology(
    platform: str,
    postgres: str,
    full: str,
    repo_checks: str,
    blocking: str,
    planner: str,
    result_helper: str,
) -> list[str]:
    archive, shard = (_job(platform, name) for name in ("archive", "shard"))
    consumer, result = (_job(platform, name) for name in ("postgres-contracts", "result"))
    x64, arm64 = (_job(full, name) for name in ("tests_linux_x64", "tests_linux_arm64"))
    pg_gate = _job(blocking, "postgres-runtime-state-contracts")
    cargo_gate = _job(blocking, "deep-linux-cargo")
    cargo_result = _job(blocking, "deep-linux-cargo-result")
    required = _job(blocking, "required")
    lint = _job(full, "lint_build")
    full_results = _job(full, "results")
    archive_checkout = _checkout(archive)
    shard_checkout = _checkout(shard)
    postgres_checkout = _checkout(_job(postgres, "postgres-contracts"))
    archive_rusty_v8 = _step(
        archive, "Configure rusty_v8 artifact overrides and verify checksums"
    )
    archive_build = _step(archive, "Build nextest archive")
    archive_upload = _step(archive, "Upload nextest archive")
    helper_upload = _step(archive, "Upload runtime test helpers")
    shard_archive = _step(shard, "Download nextest archive")
    shard_helper = _step(shard, "Download runtime test helpers")
    ordinary_run = _step(shard, "tests")
    shard_junit = _step(shard, "Inspect nextest JUnit signal")
    shard_junit_upload = _step(shard, "Upload nextest JUnit report")
    postgres_job = _job(postgres, "postgres-contracts")
    archive_nextest = _nextest_installers(archive)
    shard_nextest = _nextest_installers(shard)
    postgres_installers = _nextest_installers(postgres_job)
    standalone_nextest = tuple(
        installer
        for installer in postgres_installers
        if _step_condition(installer) == STANDALONE_NEXTEST_CONDITION
    )
    postgres_archive_nextest = tuple(
        installer
        for installer in postgres_installers
        if _step_condition(installer) == ARCHIVE_NEXTEST_CONDITION
    )
    postgres_nextest_roles = (
        len(standalone_nextest) == 1
        and len(postgres_archive_nextest) == 1
        and len(postgres_installers) == 2
    )
    inventory = _step(postgres_job, "Validate PostgreSQL contract inventory")
    pg_archive = _step(postgres_job, "Download nextest archive")
    pg_helper = _step(postgres_job, "Download runtime test helpers")
    pg_run = _step(postgres_job, "Run PostgreSQL contracts from nextest archive")
    pg_junit = _step(postgres_job, "Inspect archive nextest JUnit signal")
    pg_junit_upload = _step(postgres_job, "Upload archive nextest JUnit report")
    pg_confirm = _step(postgres_job, "Confirm archive PostgreSQL contract result")
    standalone = _step(postgres_job, "Run PostgreSQL Runtime State contracts")
    shard_junit_path = (
        shard.count(NEXTEST_JUNIT_ENV) == 1
        and ordinary_run.count('rm -f -- "${NEXTEST_JUNIT_FILE}"') == 1
        and shard_junit.count('"${NEXTEST_JUNIT_FILE}"') == 1
        and "path: ${{ env.NEXTEST_JUNIT_FILE }}" in shard_junit_upload
        and "${CARGO_TARGET_DIR}/nextest/default/junit.xml" not in shard
    )
    postgres_junit_path = (
        postgres_job.count(NEXTEST_JUNIT_ENV) == 1
        and inventory.count('rm -f -- "${NEXTEST_JUNIT_FILE}"') == 1
        and 'junit="${NEXTEST_JUNIT_FILE}"' in pg_run
        and pg_run.count('rm -f -- "${junit}"') == 1
        and pg_junit.count('"${NEXTEST_JUNIT_FILE}"') == 1
        and "path: ${{ env.NEXTEST_JUNIT_FILE }}" in pg_junit_upload
        and "${CARGO_TARGET_DIR}/nextest/default/junit.xml" not in postgres_job
    )
    inventory_root = (
        "postgres_contract_inventory.py" in inventory
        and '--repo "${GITHUB_WORKSPACE}"' in inventory
        and "--github-output" in inventory
    )
    inventory_gated_junit = (
        "if: ${{ always() && inputs.artifact_id != '' && steps.inventory.outcome == 'success' }}"
        in pg_junit
        and "INVENTORY_OUTCOME: ${{ steps.inventory.outcome }}" in pg_confirm
        and "JUNIT_OUTCOME: ${{ steps.archive_junit.outcome }}" in pg_confirm
        and 'if [[ "${INVENTORY_OUTCOME}" != "success" ]]; then' in pg_confirm
        and 'if [[ "${JUNIT_OUTCOME}" != "success" ]]; then' in pg_confirm
    )
    checks = (
        (
            "archive producer rusty_v8 override",
            _step_value(archive_rusty_v8, "uses") == RUSTY_V8_SETUP_ACTION
            and _action_input(archive_rusty_v8, "target") == "${{ inputs.target }}"
            and archive.index(archive_rusty_v8) < archive.index(archive_build),
        ),
        ("archive producer nextest pin", _pinned_nextest(archive_nextest)),
        ("ordinary shard nextest pin", _pinned_nextest(shard_nextest)),
        (
            "PostgreSQL archive consumer nextest pin",
            postgres_nextest_roles and _pinned_nextest(postgres_archive_nextest),
        ),
        ("single archive producer", platform.count("cargo nextest archive") == 1 and "cargo nextest archive" in archive and "cargo nextest archive" not in postgres),
        ("archive producer artifact", _identity(archive) and _artifact(archive_upload, "upload", "nextest-archive-${{ inputs.artifact_id }}", "${{ runner.temp }}/nextest-archive/${{ env.NEXTEST_ARCHIVE_FILE }}") and _artifact(helper_upload, "upload", "${{ env.TEST_HELPERS_ARTIFACT }}", "${{ runner.temp }}/${{ env.TEST_HELPERS_ARTIFACT }}/*")),
        ("ordinary shard artifacts", _identity(shard) and _artifact(shard_archive, "download", "nextest-archive-${{ inputs.artifact_id }}", "${{ runner.temp }}/nextest-archive") and _artifact(shard_helper, "download", "${{ env.TEST_HELPERS_ARTIFACT }}", "${{ runner.temp }}/${{ env.TEST_HELPERS_ARTIFACT }}")),
        ("ordinary shard selection", re.search(r"shard:\s*\[1,\s*2,\s*3,\s*4\]", shard) is not None and '--partition "hash:${{ matrix.shard }}/4"' in ordinary_run and not re.search(r"(?:^|\s)-E(?:\s|$)", ordinary_run) and "--run-ignored" not in ordinary_run),
        ("checkout revision identity", all(checkout and "\n          ref:" not in checkout for checkout in (archive_checkout, shard_checkout, postgres_checkout))),
        ("x64 fifth consumer", "postgres_contracts: true" in x64 and "postgres_contracts: true" not in arm64),
        ("shared archive dependency and identity", "needs: archive" in consumer and "uses: ./.github/workflows/postgres-runtime-state-contracts.yml" in consumer and "artifact_id: ${{ inputs.artifact_id }}" in consumer),
        ("PostgreSQL artifacts", _identity(postgres_job) and _artifact(pg_archive, "download", "nextest-archive-${{ inputs.artifact_id }}", "${{ runner.temp }}/nextest-archive") and _artifact(pg_helper, "download", "${{ env.TEST_HELPERS_ARTIFACT }}", "${{ runner.temp }}/${{ env.TEST_HELPERS_ARTIFACT }}")),
        ("PostgreSQL archive execution", "cargo nextest run" in pg_run and '--archive-file "${archive_file}"' in pg_run and "just test" not in pg_run and not re.search(r"cargo\s+(?:build|test|nextest\s+archive)", postgres)),
        ("PostgreSQL service and concurrency", postgres.count("      postgres:\n") == 1 and postgres.count("image: postgres:18") == 1 and pg_run.count("--test-threads 4") == 1),
        ("ordinary shard JUnit path", shard_junit_path),
        ("PostgreSQL JUnit path", postgres_junit_path),
        ("PostgreSQL inventory root", inventory_root),
        ("inventory-gated JUnit inspection", inventory_gated_junit),
        ("exact JUnit cardinality", '--expected-testcases "${{ steps.inventory.outputs.expected_total }}"' in pg_junit and "TEST_STATUS: ${{ steps.archive_test.outputs.status }}" in pg_confirm),
        ("platform result fail closed", "needs: [shard, postgres-contracts]" in result and 'if [[ "${{ needs.shard.result }}" != "success" ]]; then' in result and 'if [[ "${{ inputs.postgres_contracts }}" == "true" && "${{ needs.postgres-contracts.result }}" != "success" ]]; then' in result and result.count("exit 1") == 2),
        ("standalone fallback remains callable", re.search(r"artifact_id:\n\s+required: false\n\s+default: \"\"", postgres) is not None and "if: ${{ inputs.artifact_id == '' }}" in standalone and standalone.count("just test -p ") == 6),
        ("validation scope fails safe", re.search(r"validation_scope:\n\s+description: .*\n\s+required: false\n\s+default: full\n\s+type: string", full) is not None and "empty scope defaults to full" in planner and "defaults fail-safe to full" in planner),
        ("merge-gate lint matrix", _planner_matrix(planner, "MERGE_GATE_LINT_MATRIX") == MERGE_LANES and "cargo clippy --workspace --target ${{ matrix.target }} --tests --profile dev --timings -- -D warnings" in lint),
        ("full Extended matrix", _planner_matrix(planner, "EXTENDED_LINT_MATRIX") == EXTENDED_LANES),
        ("merge-gate schedules only x64", all("needs.plan.outputs.run_general == 'true'" in _job(full, name) for name in ("general", "cargo_shear", "argument_comment_lint_package", "argument_comment_lint_prebuilt")) and "needs.plan.outputs.run_linux_x64 == 'true'" in x64 and "needs.plan.outputs.run_linux_arm64 == 'true'" in arm64 and "include: ${{ fromJSON(needs.plan.outputs.lint_matrix) }}" in lint),
        ("scope-aware full result", "rust_ci_full_result.py" in full_results and "plan expected success" in result_helper and 'wanted = "success" if should_run else "skipped"' in result_helper and "actual != wanted" in result_helper and "tests_linux_x64" in result_helper and "tests_linux_arm64" in result_helper),
        ("eligible Cargo promotion", not pg_gate and "needs: deep-linux-eligibility" in cargo_gate and "needs.deep-linux-eligibility.result == 'success'" in cargo_gate and "needs.deep-linux-eligibility.outputs.eligible == 'true'" in cargo_gate and "uses: ./.github/workflows/rust-ci-full.yml" in cargo_gate and "validation_scope: merge-gate" in cargo_gate),
        ("bounded Cargo result", "needs: [deep-linux-eligibility, deep-linux-cargo]" in cargo_result and "if: ${{ always() }}" in cargo_result and "timeout-minutes: 10" in cargo_result and "VALIDATION_LABEL: Deep Linux Cargo" in cargo_result and "VALIDATION_RESULT: ${{ needs.deep-linux-cargo.result }}" in cargo_result and "ELIGIBILITY_RESULT: ${{ needs.deep-linux-eligibility.result }}" in cargo_result and "ELIGIBLE: ${{ needs.deep-linux-eligibility.outputs.eligible }}" in cargo_result and "set -euo pipefail" in cargo_result and "deep_linux_result.py | tee -a \"$GITHUB_STEP_SUMMARY\"" in cargo_result),
        ("required aggregate promotion", "- deep-linux-eligibility" in required and "- deep-linux-cargo-result" in required and "- deep-linux-cargo\n" not in required and "- postgres-runtime-state-contracts" not in required and "check_ci_results.py" in required),
        ("repository check", "python3 .github/scripts/check_postgres_archive_topology.py" in repo_checks),
    )
    return [f"PostgreSQL archive topology drift: {label}" for label, valid in checks if not valid]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    issues: list[str] = []
    for name in (NAMES[0], NAMES[1], NAMES[4]):
        _, workflow_issues = _workflow_jobs(repo, name)
        issues.extend(workflow_issues)
    try:
        sources = [(repo / path).read_text(encoding="utf-8") for path in SOURCES]
    except (OSError, UnicodeError) as error:
        issues.append(f"cannot read topology workflows: {error}")
    else:
        issues.extend(validate_topology(*sources))
    if issues:
        print("PostgreSQL archive topology failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
        return 1
    print("PostgreSQL archive topology passed: one producer, four ordinary shards, one PostgreSQL consumer")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
