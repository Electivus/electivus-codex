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
        r"^      - (?:name:|uses:)",
    )


def _checkout(job: str) -> str:
    return _block(
        job,
        r"^      - uses: actions/checkout@",
        r"^      - (?:name:|uses:)",
    )


def _artifact(step: str, action: str, name: str, path: str) -> bool:
    return f"uses: actions/{action}-artifact@" in step and f"          name: {name}" in step and f"          path: {path}" in step


def _identity(block: str) -> bool:
    return "NEXTEST_ARCHIVE_FILE: nextest-${{ inputs.artifact_id }}.tar.zst" in block and "TEST_HELPERS_ARTIFACT: nextest-test-helpers-${{ inputs.artifact_id }}" in block


def validate_topology(
    platform: str, postgres: str, full: str, repo_checks: str, blocking: str
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
    full_only_results = (
        "argument_comment_lint_package", "argument_comment_lint_prebuilt",
        "general", "cargo_shear", "tests_linux_arm64",
    )
    archive_checkout = _checkout(archive)
    shard_checkout = _checkout(shard)
    postgres_checkout = _checkout(_job(postgres, "postgres-contracts"))
    archive_upload = _step(archive, "Upload nextest archive")
    helper_upload = _step(archive, "Upload runtime test helpers")
    shard_archive = _step(shard, "Download nextest archive")
    shard_helper = _step(shard, "Download runtime test helpers")
    ordinary_run = _step(shard, "tests")
    pg_archive = _step(_job(postgres, "postgres-contracts"), "Download nextest archive")
    pg_helper = _step(_job(postgres, "postgres-contracts"), "Download runtime test helpers")
    pg_run = _step(_job(postgres, "postgres-contracts"), "Run PostgreSQL contracts from nextest archive")
    standalone = _step(_job(postgres, "postgres-contracts"), "Run PostgreSQL Runtime State contracts")
    checks = (
        ("single archive producer", platform.count("cargo nextest archive") == 1 and "cargo nextest archive" in archive and "cargo nextest archive" not in postgres),
        ("archive producer artifact", _identity(archive) and _artifact(archive_upload, "upload", "nextest-archive-${{ inputs.artifact_id }}", "${{ runner.temp }}/nextest-archive/${{ env.NEXTEST_ARCHIVE_FILE }}") and _artifact(helper_upload, "upload", "${{ env.TEST_HELPERS_ARTIFACT }}", "${{ runner.temp }}/${{ env.TEST_HELPERS_ARTIFACT }}/*")),
        ("ordinary shard artifacts", _identity(shard) and _artifact(shard_archive, "download", "nextest-archive-${{ inputs.artifact_id }}", "${{ runner.temp }}/nextest-archive") and _artifact(shard_helper, "download", "${{ env.TEST_HELPERS_ARTIFACT }}", "${{ runner.temp }}/${{ env.TEST_HELPERS_ARTIFACT }}")),
        ("ordinary shard selection", re.search(r"shard:\s*\[1,\s*2,\s*3,\s*4\]", shard) is not None and '--partition "hash:${{ matrix.shard }}/4"' in ordinary_run and not re.search(r"(?:^|\s)-E(?:\s|$)", ordinary_run) and "--run-ignored" not in ordinary_run),
        ("checkout revision identity", all(checkout and "\n          ref:" not in checkout for checkout in (archive_checkout, shard_checkout, postgres_checkout))),
        ("x64 fifth consumer", "postgres_contracts: true" in x64 and "postgres_contracts: true" not in arm64),
        ("shared archive dependency and identity", "needs: archive" in consumer and "uses: ./.github/workflows/postgres-runtime-state-contracts.yml" in consumer and "artifact_id: ${{ inputs.artifact_id }}" in consumer),
        ("PostgreSQL artifacts", _identity(_job(postgres, "postgres-contracts")) and _artifact(pg_archive, "download", "nextest-archive-${{ inputs.artifact_id }}", "${{ runner.temp }}/nextest-archive") and _artifact(pg_helper, "download", "${{ env.TEST_HELPERS_ARTIFACT }}", "${{ runner.temp }}/${{ env.TEST_HELPERS_ARTIFACT }}")),
        ("PostgreSQL archive execution", "cargo nextest run" in pg_run and '--archive-file "${archive_file}"' in pg_run and "just test" not in pg_run and not re.search(r"cargo\s+(?:build|test|nextest\s+archive)", postgres)),
        ("PostgreSQL service and concurrency", postgres.count("      postgres:\n") == 1 and postgres.count("image: postgres:18") == 1 and pg_run.count("--test-threads 4") == 1),
        ("exact JUnit cardinality", '--expected-testcases "${{ steps.inventory.outputs.expected_total }}"' in postgres and "JUNIT_OUTCOME: ${{ steps.archive_junit.outcome }}" in postgres and "TEST_STATUS: ${{ steps.archive_test.outputs.status }}" in postgres),
        ("platform result fail closed", "needs: [shard, postgres-contracts]" in result and 'if [[ "${{ needs.shard.result }}" != "success" ]]; then' in result and 'if [[ "${{ inputs.postgres_contracts }}" == "true" && "${{ needs.postgres-contracts.result }}" != "success" ]]; then' in result and result.count("exit 1") == 2),
        ("standalone fallback remains callable", re.search(r"artifact_id:\n\s+required: false\n\s+default: \"\"", postgres) is not None and "if: ${{ inputs.artifact_id == '' }}" in standalone and standalone.count("just test -p ") == 6),
        ("validation scope fails safe", re.search(r"validation_scope:\n\s+description: .*\n\s+required: false\n\s+default: full\n\s+type: string", full) is not None and "inputs.validation_scope == 'merge-gate'" in lint and "inputs.validation_scope != 'merge-gate'" in full),
        ("merge-gate lint matrix", lint.count('inputs.validation_scope == \'merge-gate\'') == 1 and lint.count('"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-gnu","profile":"dev"') == 2 and "cargo clippy --workspace --target ${{ matrix.target }} --tests --profile dev --timings -- -D warnings" in lint),
        ("full Extended matrix", all(fragment in lint for fragment in ('"target":"x86_64-unknown-linux-musl","profile":"dev"', '"target":"aarch64-unknown-linux-musl","profile":"dev"', '"target":"aarch64-unknown-linux-gnu","profile":"dev"', '"target":"x86_64-unknown-linux-musl","profile":"release"', '"target":"aarch64-unknown-linux-musl","profile":"release"'))),
        ("merge-gate schedules only x64", all("if: ${{ inputs.validation_scope != 'merge-gate' }}" in _job(full, name) for name in ("general", "cargo_shear", "argument_comment_lint_package", "argument_comment_lint_prebuilt", "tests_linux_arm64")) and "inputs.validation_scope != 'merge-gate'" not in x64 and "inputs.validation_scope != 'merge-gate'" not in lint),
        ("scope-aware full result", "VALIDATION_SCOPE: ${{ inputs.validation_scope }}" in full_results and "needs.lint_build.result }}' == 'success'" in full_results and "needs.tests_linux_x64.result }}' == 'success'" in full_results and "if [[ \"$VALIDATION_SCOPE\" != 'merge-gate' ]]" in full_results and all(f"needs.{name}.result }}}}' == 'success'" in full_results for name in full_only_results)),
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
        sources = [(repo / path).read_text(encoding="utf-8") for path in WORKFLOWS]
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
