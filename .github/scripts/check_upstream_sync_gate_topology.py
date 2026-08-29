#!/usr/bin/env python3
"""Fail closed when the Synchronization Merge gate wiring drifts."""

import argparse
from pathlib import Path
import sys

from check_postgres_archive_topology import _job
from check_rust_test_policy import _workflow_jobs


SOURCES = (
    ".github/workflows/blocking-ci.yml",
    ".github/workflows/repo-checks.yml",
    ".github/scripts/check_upstream_sync_topology.py",
    ".github/scripts/test_check_upstream_sync_topology.py",
)
REAL_HEAD_REF = (
    "ref: ${{ github.event_name == 'pull_request' && "
    "github.event.pull_request.head.sha || github.sha }}"
)
REAL_BASE_ENV = (
    "PR_BASE_SHA: ${{ github.event_name == 'pull_request' && "
    "github.event.pull_request.base.sha || '' }}"
)
REAL_HEAD_ENV = (
    "PR_HEAD_SHA: ${{ github.event_name == 'pull_request' && "
    "github.event.pull_request.head.sha || '' }}"
)
REAL_BRANCH_ENV = (
    "PR_HEAD_BRANCH: ${{ github.event_name == 'pull_request' && "
    "github.event.pull_request.head.ref || '' }}"
)


def validate_topology(
    blocking: str,
    repo_checks: str,
    checker: str,
    tests: str,
) -> list[str]:
    synchronization = _job(blocking, "synchronization-topology")
    required = _job(blocking, "required")
    checks = (
        (
            "real head checkout",
            REAL_HEAD_REF in synchronization
            and "fetch-depth: 0" in synchronization
            and "persist-credentials: false" in synchronization,
        ),
        (
            "real PR identity",
            REAL_BASE_ENV in synchronization
            and REAL_HEAD_ENV in synchronization
            and REAL_BRANCH_ENV in synchronization
            and '"${PR_BASE_SHA}"' in synchronization
            and '"${PR_HEAD_SHA}"' in synchronization
            and '"${PR_HEAD_BRANCH}"' in synchronization,
        ),
        (
            "checker invocation",
            synchronization.count(
                "python3 .github/scripts/check_upstream_sync_topology.py"
            )
            == 1
            and "set -euo pipefail" in synchronization
            and 'tee -a "$GITHUB_STEP_SUMMARY"' in synchronization
            and "continue-on-error:" not in synchronization,
        ),
        (
            "required aggregate",
            "- synchronization-topology" in required
            and "name: CI required" in required,
        ),
        (
            "repository wiring test",
            "check_upstream_sync_gate_topology.py" in repo_checks
            and "from check_upstream_sync_topology import" in tests
            and "TopologyEvidence" in tests
            and "def validate_topology(" in checker,
        ),
    )
    return [
        f"Synchronization gate topology drift: {label}"
        for label, valid in checks
        if not valid
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    args = parser.parse_args(argv)
    repo = args.repo.resolve()
    issues: list[str] = []
    _, workflow_issues = _workflow_jobs(repo, "blocking-ci.yml")
    issues.extend(workflow_issues)
    try:
        sources = [(repo / path).read_text(encoding="utf-8") for path in SOURCES]
    except (OSError, UnicodeError) as error:
        issues.append(f"cannot read Synchronization gate sources: {error}")
    else:
        issues.extend(validate_topology(*sources))
    if issues:
        print(
            "Synchronization gate topology failed:\n"
            + "\n".join(f"- {issue}" for issue in issues),
            file=sys.stderr,
        )
        return 1
    print("Synchronization gate topology passed: real-head Merge gate wiring")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
