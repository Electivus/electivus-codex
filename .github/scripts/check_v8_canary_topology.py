#!/usr/bin/env python3
"""Fail closed when the Change-triggered V8 canary topology drifts."""

import argparse
from pathlib import Path
import re
import sys

from check_postgres_archive_topology import _block
from check_postgres_archive_topology import _job
from check_postgres_archive_topology import _step
from check_rust_test_policy import _workflow_jobs


SOURCES = (
    ".github/workflows/v8-canary.yml",
    ".github/workflows/blocking-ci.yml",
    ".github/workflows/repo-checks.yml",
    ".github/scripts/v8_canary_changes.py",
)
EXPECTED_MATRIX = {
    ("ubuntu-24.04", "ci-v8", "linux_amd64", "false", "x86_64-unknown-linux-gnu", "x64", "release"),
    ("ubuntu-24.04", "ci-v8", "linux_amd64", "true", "x86_64-unknown-linux-gnu", "x64", "ptrcomp-sandbox"),
    ("ubuntu-24.04-arm", "ci-v8", "linux_arm64", "false", "aarch64-unknown-linux-gnu", "arm64", "release"),
    ("ubuntu-24.04-arm", "ci-v8", "linux_arm64", "true", "aarch64-unknown-linux-gnu", "arm64", "ptrcomp-sandbox"),
    ("ubuntu-24.04", "ci-v8", "linux_amd64_musl", "false", "x86_64-unknown-linux-musl", "x64", "release"),
    ("ubuntu-24.04", "ci-v8", "linux_amd64_musl", "true", "x86_64-unknown-linux-musl", "x64", "ptrcomp-sandbox"),
    ("ubuntu-24.04-arm", "ci-v8", "linux_arm64_musl", "false", "aarch64-unknown-linux-musl", "arm64", "release"),
    ("ubuntu-24.04-arm", "ci-v8", "linux_arm64_musl", "true", "aarch64-unknown-linux-musl", "arm64", "ptrcomp-sandbox"),
}
ROW = re.compile(
    r"          - runner: (\S+)\n"
    r"            bazel_config: (\S+)\n"
    r"            platform: (\S+)\n"
    r"            sandbox: (\S+)\n"
    r"            target: (\S+)\n"
    r"            v8_cpu: (\S+)\n"
    r"            variant: (\S+)"
)


def validate_topology(
    canary: str, blocking: str, repo_checks: str, detector: str
) -> list[str]:
    metadata = _job(canary, "metadata")
    build = _job(canary, "build")
    result = _job(canary, "result")
    detect = _step(metadata, "Detect V8 canary changes")
    version = _step(metadata, "Resolve exact v8 crate version")
    smoke = _step(build, "Smoke test staged artifact with Cargo")
    upload = _step(build, "Upload staged artifacts")
    caller = _job(blocking, "v8-canary")
    required = _job(blocking, "required")
    concurrency = _block(canary, r"^concurrency:\s*$", r"^jobs:\s*$")
    infra_paths = (
        ".github/scripts/check_v8_canary_topology.py",
        ".github/scripts/test_check_v8_canary_topology.py",
        ".github/scripts/test_v8_canary_result.py",
        ".github/scripts/v8_canary_result.py",
        ".github/workflows/blocking-ci.yml",
        ".github/workflows/repo-checks.yml",
        ".github/workflows/README.md",
    )
    checks = (
        ("exact Linux matrix", set(ROW.findall(build)) == EXPECTED_MATRIX and len(ROW.findall(build)) == 8 and "fail-fast: false" in build),
        ("metadata fail safe", "canary_reason: ${{ steps.changes.outputs.canary_reason }}" in metadata and "canary_required: ${{ steps.changes.outputs.canary_required || 'true' }}" in metadata and "canary_required=true" in detect and "detector_status=0" in detect and "detector exited with status" in detect and "${#detector_lines[@]} -eq 2" in detect and "^canary_required=(true|false)$" in detect and "^canary_reason=([[:print:]]{1,240})$" in detect and 'canary_required="${detector_lines[0]#canary_required=}"' in detect and "classifier returned malformed output" in detect and "canary_reason=${canary_reason}" in detect and "GITHUB_STEP_SUMMARY" in detect),
        ("version fallback", "unknown-${GITHUB_SHA:0:12}" in version and "resolved-v8-crate-version" in version and "^[A-Za-z0-9._-]{1,64}$" in version),
        ("conditional build", "needs: metadata" in build and "if: ${{ needs.metadata.outputs.canary_required == 'true' }}" in build),
        ("bounded result", "needs: [metadata, build]" in result and "if: ${{ always() }}" in result and "BUILD_RESULT: ${{ needs.build.result }}" in result and "CANARY_REASON: ${{ needs.metadata.outputs.canary_reason }}" in result and "CANARY_REQUIRED: ${{ needs.metadata.outputs.canary_required }}" in result and "METADATA_RESULT: ${{ needs.metadata.result }}" in result and "v8_canary_result.py | tee -a \"$GITHUB_STEP_SUMMARY\"" in result),
        ("artifact and smoke integrity", "Build Bazel V8 release pair" in build and "BUILDBUDDY_API_KEY: ${{ secrets.BUILDBUDDY_API_KEY }}" in build and "run_bazel_with_buildbuddy.py" in build and "rusty_v8_bazel.py stage-release-pair" in build and "x86_64-unknown-linux-gnu:x86_64|aarch64-unknown-linux-gnu:aarch64" in smoke and "Skipping non-native Cargo smoke" in smoke and "actions/upload-artifact@" in upload and "v8-canary-${{ needs.metadata.outputs.v8_version }}-${{ matrix.variant }}-${{ matrix.target }}" in upload and '"codex-rs/v8-poc/**"' in detector),
        ("red matrix", "continue-on-error:" not in build),
        ("V8 caller required", blocking.count("uses: ./.github/workflows/v8-canary.yml") == 1 and "uses: ./.github/workflows/v8-canary.yml" in caller and "- v8-canary" in required),
        ("V8 caller permissions", "permissions:\n      contents: read\n      actions: read" in caller and "permissions:\n      contents: read\n      actions: read" in build),
        ("detector self relevance", all(f'"{path}"' in detector for path in infra_paths)),
        ("detector unknown fail safe", "KNOWN_IRRELEVANT_PATH_PATTERNS" in detector and "unknown V8 impact" in detector and "comparison failed" in detector and "comparison is missing base or head" in detector),
        ("V8 concurrency", "group: ${{ github.workflow }}::${{ github.event.pull_request.number > 0 && format('pr-{0}', github.event.pull_request.number) || github.ref_name }}" in concurrency and "cancel-in-progress: ${{ github.ref_name != 'main' }}" in concurrency),
        ("V8 repository check", "python3 .github/scripts/check_v8_canary_topology.py" in repo_checks),
    )
    return [f"V8 canary topology drift: {label}" for label, valid in checks if not valid]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    issues: list[str] = []
    _, workflow_issues = _workflow_jobs(repo, "blocking-ci.yml")
    issues.extend(workflow_issues)
    try:
        sources = [(repo / path).read_text(encoding="utf-8") for path in SOURCES]
    except (OSError, UnicodeError) as error:
        issues.append(f"cannot read V8 canary sources: {error}")
    else:
        issues.extend(validate_topology(*sources))
    if issues:
        print("V8 canary topology failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
        return 1
    print("V8 canary topology passed: metadata-only skip or exact eight-leg Linux matrix")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
