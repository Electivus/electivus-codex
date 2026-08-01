#!/usr/bin/env python3
"""Fail closed when the full-Rust PostgreSQL archive topology drifts."""

import argparse
from pathlib import Path
import re
import sys
from check_rust_test_policy import _workflow_jobs


WORKFLOWS = tuple(
    f".github/workflows/{name}"
    for name in ("rust-ci-full-nextest-platform.yml", "postgres-runtime-state-contracts.yml", "rust-ci-full.yml", "repo-checks.yml")
)
def _job(workflow: str, name: str) -> str:
    match = re.search(rf"(?m)^  {re.escape(name)}:\s*$", workflow)
    if match is None:
        return ""
    following = re.search(r"(?m)^  [A-Za-z_][A-Za-z0-9_-]*:\s*$", workflow[match.end() :])
    end = match.end() + following.start() if following else len(workflow)
    return workflow[match.start() : end]


def validate_topology(
    platform: str, postgres: str, full: str, repo_checks: str
) -> list[str]:
    archive = _job(platform, "archive")
    shard = _job(platform, "shard")
    consumer = _job(platform, "postgres-contracts")
    result = _job(platform, "result")
    x64 = _job(full, "tests_linux_x64")
    arm64 = _job(full, "tests_linux_arm64")
    checks = (
        (
            "single archive producer",
            platform.count("cargo nextest archive") == 1
            and "cargo nextest archive" in archive
            and "cargo nextest archive" not in postgres,
        ),
        (
            "four partitions",
            re.search(r"shard:\s*\[1,\s*2,\s*3,\s*4\]", shard) is not None
            and '--partition "hash:${{ matrix.shard }}/4"' in shard
            and "--run-ignored" not in shard,
        ),
        (
            "x64 fifth consumer",
            "postgres_contracts: true" in x64
            and "postgres_contracts: true" not in arm64,
        ),
        (
            "shared archive dependency and identity",
            "needs: archive" in consumer
            and "uses: ./.github/workflows/postgres-runtime-state-contracts.yml" in consumer
            and "artifact_id: ${{ inputs.artifact_id }}" in consumer
            and "nextest-archive-${{ inputs.artifact_id }}" in platform
            and postgres.count("nextest-archive-${{ inputs.artifact_id }}") == 1
            and "nextest-test-helpers-${{ inputs.artifact_id }}" in postgres,
        ),
        (
            "no archive-consumer compilation",
            re.search(r"cargo\s+(?:build|test|nextest\s+archive)", postgres) is None,
        ),
        (
            "one PostgreSQL 18 service",
            postgres.count("      postgres:\n") == 1
            and postgres.count("image: postgres:18") == 1,
        ),
        (
            "PostgreSQL concurrency four",
            postgres.count("--test-threads 4") == 1,
        ),
        (
            "exact JUnit cardinality",
            "--expected-testcases \"${{ steps.inventory.outputs.expected_total }}\""
            in postgres
            and "JUNIT_OUTCOME: ${{ steps.archive_junit.outcome }}" in postgres
            and "TEST_STATUS: ${{ steps.archive_test.outputs.status }}" in postgres,
        ),
        (
            "platform result fail closed",
            "needs: [shard, postgres-contracts]" in result
            and "needs.postgres-contracts.result" in result,
        ),
        (
            "standalone Merge gate",
            re.search(r"artifact_id:\n\s+required: false\n\s+default: \"\"", postgres)
            is not None
            and postgres.count("just test -p ") == 6
            and re.search(r"Run PostgreSQL Runtime State contracts\n\s+if: \$\{\{ inputs.artifact_id == '' \}\}", postgres) is not None,
        ),
        (
            "repository check",
            "python3 .github/scripts/check_postgres_archive_topology.py"
            in repo_checks,
        ),
    )
    return [f"PostgreSQL archive topology drift: {label}" for label, valid in checks if not valid]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    issues: list[str] = []
    for name in ("rust-ci-full-nextest-platform.yml", "postgres-runtime-state-contracts.yml"):
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
