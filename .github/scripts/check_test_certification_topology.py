#!/usr/bin/env python3
"""Fail closed when the bounded #89 certification mechanism drifts."""

import argparse
from pathlib import Path
import re
import sys

from check_postgres_archive_topology import _job
from check_rust_test_policy import _workflow_jobs
from verify_test_certification import TESTS


SOURCES = (
    ".github/workflows/test-certification.yml",
    ".github/scripts/verify_test_certification.py",
    ".github/workflows/repo-checks.yml",
    ".github/scripts/check_rust_test_policy.py",
    ".github/scripts/check_nextest_junit.py",
    ".github/workflows/rust-ci-full-nextest-platform.yml",
    ".github/workflows/rust-ci-full.yml",
)
MATRIX_ROW = re.compile(
    r"          - test_id: (\S+)\n"
    r"            package: (\S+)\n"
    r"            identity: (\S+)"
)


def validate_topology(
    workflow: str,
    verifier: str,
    repo_checks: str,
    policy: str,
    junit: str,
    platform: str,
    rust: str,
) -> list[str]:
    expected_rows = {
        (test_id, definition.package, definition.identity)
        for test_id, definition in TESTS.items()
    }
    trigger = workflow.split("jobs:", 1)[0]
    shard = _job(platform, "shard")
    x64 = _job(rust, "tests_linux_x64")
    checks = (
        ("isolated trigger", "push:" in trigger and "branches: [certification/issue-89]" in trigger and "workflow_dispatch:" in trigger and "pull_request:" not in trigger),
        ("immutable Linux x64", "runs-on: ubuntu-24.04\n" in workflow and "TARGET: x86_64-unknown-linux-gnu" in workflow and "ref: ${{ env.CANDIDATE_SHA }}" in workflow and "--checked-out-sha \"$(git rev-parse HEAD)\"" in workflow and "--runner-os \"${RUNNER_OS}\"" in workflow and "--runner-arch \"${RUNNER_ARCH}\"" in workflow),
        ("hosted capacity and stack", 'RUST_MIN_STACK: "8388608"' in workflow and 'CARGO_PROFILE_CI_TEST_DEBUG: "0"' in workflow and "sudo rm -rf" in workflow and "/opt/hostedtoolcache" in workflow),
        ("two independent tests", set(MATRIX_ROW.findall(workflow)) == expected_rows and len(MATRIX_ROW.findall(workflow)) == 2 and "fail-fast: false" in workflow),
        ("exact identities", all(definition.identity in workflow and definition.identity in verifier for definition in TESTS.values())),
        ("twenty ordered executions", "seq 1 20" in workflow and "for order in" in workflow),
        ("retry-free exact nextest", "--cargo-profile ci-test" in workflow and "--cargo-profile ci-test" in verifier and "--retries 0" in workflow and "--run-ignored=only" in workflow and "-E \"test(=${{ matrix.identity }})\"" in workflow),
        ("single JUnit testcase", "check_nextest_junit.py" in workflow and "--expected-testcases 1" in workflow),
        ("unexpected skip", "--reject-skipped" in workflow and 'SKIP_ELEMENTS = {"skipped"}' in junit and 'if reject_skipped else ("failures", "errors")' in junit),
        ("stop failed sequence", all(status in workflow for status in ("test_status", "junit_status", "record_status")) and "if [[ \"${test_status}\" -ne 0" in workflow and "break" in workflow),
        ("retained evidence", "if: always()" in workflow and "actions/upload-artifact@" in workflow and "retention-days: 90" in workflow and "manifest.json" in workflow and "GITHUB_STEP_SUMMARY" in workflow and "actions/runs/${GITHUB_RUN_ID}" in workflow),
        ("manifest lifecycle", all(f'verify_test_certification.py" {command}' in workflow for command in ("init", "record", "verify")) and "issues.extend(verify_retained_reports(manifest, args.manifest.parent))" in verifier),
        ("temporary policy", "TEMPORARY_CERTIFICATION_TESTS" in policy and "stale Rust ignore classification" in policy and "check_rust_test_policy.py" in repo_checks),
        ("ordinary reactivation path", "target: x86_64-unknown-linux-gnu" in x64 and "uses: ./.github/workflows/rust-ci-full-nextest-platform.yml" in x64 and "--run-ignored" not in shard and "check_nextest_junit.py" in shard and "RETRY_ELEMENTS" in junit),
        ("repository check", "python3 .github/scripts/check_test_certification_topology.py" in repo_checks),
    )
    return [f"Test certification topology drift: {label}" for label, valid in checks if not valid]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    _, issues = _workflow_jobs(repo, "test-certification.yml")
    try:
        sources = [(repo / path).read_text(encoding="utf-8") for path in SOURCES]
    except (OSError, UnicodeError) as error:
        issues.append(f"cannot read test certification sources: {error}")
    else:
        issues.extend(validate_topology(*sources))
    if issues:
        print("Test certification topology failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
        return 1
    print("Test certification topology passed: two independent 20-run Linux x64 sequences")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
