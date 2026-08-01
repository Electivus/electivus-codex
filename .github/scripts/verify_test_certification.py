#!/usr/bin/env python3
"""Create and verify retained evidence for the two #89 test certifications."""

import argparse
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re
import sys
import tempfile
import xml.etree.ElementTree as ET

from check_nextest_junit import inspect_report


@dataclass(frozen=True)
class TestDefinition:
    package: str
    identity: str


TESTS = {
    "core-pending-input": TestDefinition("codex-core", "suite::pending_input::injected_user_input_triggers_follow_up_request_with_deltas"),
    "app-server-exec-approval-item-id": TestDefinition("codex-app-server", "suite::v2::review::review_start_exec_approval_item_id_matches_command_execution_item"),
}
WORKFLOW_IDENTITY = ".github/workflows/test-certification.yml"
WORKFLOW_REF_PREFIX = f"Electivus/electivus-codex/{WORKFLOW_IDENTITY}@"
SHA_RE = re.compile(r"[0-9a-f]{40}")
HASH_RE = re.compile(r"[0-9a-f]{64}")


def command_for(test: TestDefinition) -> str:
    return (
        f"cargo nextest run --cargo-profile ci-test --target x86_64-unknown-linux-gnu -p {test.package} "
        "--test all --retries 0 "
        f"--run-ignored=only -E 'test(={test.identity})'"
    )


def inspect_certification_report(report: Path, test: TestDefinition) -> tuple[list[str], int]:
    issues = inspect_report(report, expected_testcases=1, reject_skipped=True)
    try:
        testcases = [element for element in ET.parse(report).getroot().iter() if element.tag.rsplit("}", 1)[-1] == "testcase"]
    except (OSError, ET.ParseError):
        return issues, -1
    if len(testcases) == 1:
        testcase = testcases[0]
        actual = (testcase.get("classname", "").strip(), testcase.get("name", "").strip())
        expected = (f"{test.package}::all", test.identity)
        if actual != expected:
            issues.append(f"expected testcase {expected[0]}::{expected[1]}, found {actual[0]}::{actual[1]}")
    return issues, len(testcases)


def new_manifest(
    *, candidate_sha: str, checked_out_sha: str, test_id: str, workflow_ref: str,
    run_id: str, run_attempt: int, runner_os: str, runner_arch: str,
) -> dict[str, object]:
    if SHA_RE.fullmatch(candidate_sha) is None or candidate_sha != checked_out_sha:
        raise ValueError("candidate must be the full checked-out commit SHA")
    if test_id not in TESTS:
        raise ValueError("unknown certification test")
    if not workflow_ref.startswith(WORKFLOW_REF_PREFIX) or len(workflow_ref) == len(WORKFLOW_REF_PREFIX):
        raise ValueError("workflow ref does not identify the certification workflow")
    if not run_id.isdecimal():
        raise ValueError("run ID must be decimal")
    if run_attempt != 1:
        raise ValueError("workflow reruns cannot produce certification evidence")
    if (runner_os, runner_arch) != ("Linux", "X64"):
        raise ValueError("certification requires a Linux X64 runner")
    test = TESTS[test_id]
    return {
        "schemaVersion": 1, "candidateSha": candidate_sha, "workflowIdentity": WORKFLOW_IDENTITY,
        "workflowRef": workflow_ref, "runId": run_id, "runAttempt": run_attempt,
        "runUrl": f"https://github.com/Electivus/electivus-codex/actions/runs/{run_id}",
        "commitUrl": f"https://github.com/Electivus/electivus-codex/commit/{candidate_sha}",
        "testId": test_id, "runner": "ubuntu-24.04", "runnerOs": runner_os, "runnerArch": runner_arch,
        "target": "x86_64-unknown-linux-gnu", "package": test.package, "testBinary": "all",
        "testIdentity": test.identity, "executions": [],
    }


def record_execution(
    manifest: dict[str, object], report: Path, order: int, exit_code: int
) -> list[str]:
    test_id = manifest.get("testId")
    test = TESTS.get(test_id) if isinstance(test_id, str) else None
    junit_issues, testcase_count = inspect_certification_report(report, test) if test else (["testId must identify one exact #89 test"], -1)
    try:
        junit_hash = hashlib.sha256(report.read_bytes()).hexdigest()
    except OSError:
        junit_hash = ""
    command = command_for(test) if test is not None else ""
    execution = {
        "order": order, "command": command,
        "commandResult": "success" if exit_code == 0 else "failure", "exitCode": exit_code,
        "junitVerdict": "pass" if not junit_issues else "fail", "retryFree": not any("retry evidence" in issue for issue in junit_issues),
        "testcaseCount": testcase_count, "junitPath": f"junit/{order:02}.xml", "junitSha256": junit_hash,
    }
    executions = manifest.setdefault("executions", [])
    if not isinstance(executions, list):
        return ["executions must be a list"]
    executions.append(execution)
    return ([f"nextest command exited {exit_code}"] if exit_code else []) + junit_issues


def verify_manifest(manifest: dict[str, object], expected_sha: str) -> list[str]:
    issues: list[str] = []
    if type(manifest.get("schemaVersion")) is not int or manifest.get("schemaVersion") != 1:
        issues.append("schemaVersion must be 1")
    candidate_sha = manifest.get("candidateSha")
    if not isinstance(candidate_sha, str) or SHA_RE.fullmatch(candidate_sha) is None:
        issues.append("candidateSha must be a full lowercase commit SHA")
    elif candidate_sha != expected_sha:
        issues.append("candidateSha does not match the checked-out candidate")

    workflow_identity = manifest.get("workflowIdentity")
    if workflow_identity != WORKFLOW_IDENTITY:
        issues.append(f"workflowIdentity must be {WORKFLOW_IDENTITY}")
    workflow_ref = manifest.get("workflowRef")
    if not isinstance(workflow_ref, str) or not workflow_ref.startswith(WORKFLOW_REF_PREFIX) or len(workflow_ref) == len(WORKFLOW_REF_PREFIX):
        issues.append("workflowRef must identify the repository certification workflow and ref")
    run_id = manifest.get("runId")
    if not isinstance(run_id, str) or not run_id.isdecimal():
        issues.append("runId must be a decimal string")
    if type(manifest.get("runAttempt")) is not int or manifest.get("runAttempt") != 1:
        issues.append("runAttempt must be 1; rerun evidence cannot certify")
    if manifest.get("runUrl") != f"https://github.com/Electivus/electivus-codex/actions/runs/{run_id}":
        issues.append("runUrl must identify the recorded workflow run")
    if manifest.get("commitUrl") != f"https://github.com/Electivus/electivus-codex/commit/{candidate_sha}":
        issues.append("commitUrl must identify candidateSha")

    test_id = manifest.get("testId")
    definition = TESTS.get(test_id) if isinstance(test_id, str) else None
    if definition is None:
        issues.append("testId must identify one exact #89 test")
    else:
        expected = {
            "runner": "ubuntu-24.04", "runnerOs": "Linux", "runnerArch": "X64",
            "target": "x86_64-unknown-linux-gnu", "package": definition.package,
            "testBinary": "all", "testIdentity": definition.identity,
        }
        for field, value in expected.items():
            if manifest.get(field) != value:
                issues.append(f"{field} must be {value}")

    executions = manifest.get("executions")
    if not isinstance(executions, list):
        return issues + ["executions must be a list"]
    if len(executions) != 20:
        issues.append(f"exactly 20 executions are required, found {len(executions)}")
    command = command_for(definition) if definition is not None else None
    for order, execution in enumerate(executions, 1):
        if not isinstance(execution, dict):
            issues.append(f"execution {order} must be an object")
            continue
        expected = {
            "order": order, "command": command, "commandResult": "success", "exitCode": 0,
            "junitVerdict": "pass", "retryFree": True, "testcaseCount": 1,
            "junitPath": f"junit/{order:02}.xml",
        }
        for field, value in expected.items():
            if type(execution.get(field)) is not type(value) or execution.get(field) != value:
                issues.append(f"execution {order} {field} must be {value!r}")
        junit_hash = execution.get("junitSha256")
        if not isinstance(junit_hash, str) or HASH_RE.fullmatch(junit_hash) is None:
            issues.append(f"execution {order} junitSha256 must be a lowercase SHA-256")
    return issues


def verify_retained_reports(manifest: dict[str, object], directory: Path) -> list[str]:
    executions = manifest.get("executions")
    entries = executions if isinstance(executions, list) else []
    test_id = manifest.get("testId")
    test = TESTS.get(test_id) if isinstance(test_id, str) else None
    issues: list[str] = []
    for order in range(1, 21):
        report = directory / "junit" / f"{order:02}.xml"
        report_issues = inspect_certification_report(report, test)[0] if test else inspect_report(report, expected_testcases=1, reject_skipped=True)
        issues.extend(f"execution {order}: {issue}" for issue in report_issues)
        entry = entries[order - 1] if order <= len(entries) and isinstance(entries[order - 1], dict) else {}
        if report.is_file():
            actual_hash = hashlib.sha256(report.read_bytes()).hexdigest()
            if entry.get("junitSha256") != actual_hash:
                issues.append(f"execution {order}: JUnit SHA-256 does not match retained file")
    return issues


def _write_manifest(path: Path, manifest: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary: Path | None = None
    try:
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=path.parent, prefix=f".{path.name}.", suffix=".tmp", delete=False) as output:
            temporary = Path(output.name)
            json.dump(manifest, output, indent=2)
            output.write("\n")
        temporary.replace(path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def _read_manifest(path: Path) -> dict[str, object]:
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(manifest, dict):
        raise ValueError("manifest root must be an object")
    return manifest


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    init = commands.add_parser("init")
    init.add_argument("manifest", type=Path)
    for flag in ("candidate-sha", "checked-out-sha", "test-id", "workflow-ref", "run-id", "runner-os", "runner-arch"):
        init.add_argument(f"--{flag}", required=True)
    init.add_argument("--run-attempt", required=True, type=int)
    record = commands.add_parser("record")
    record.add_argument("manifest", type=Path)
    record.add_argument("report", type=Path)
    for flag in ("order", "exit-code"):
        record.add_argument(f"--{flag}", required=True, type=int)
    verify = commands.add_parser("verify")
    verify.add_argument("manifest", type=Path)
    verify.add_argument("--expected-sha", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "init":
            manifest = new_manifest(
                candidate_sha=args.candidate_sha,
                checked_out_sha=args.checked_out_sha,
                test_id=args.test_id,
                workflow_ref=args.workflow_ref,
                run_id=args.run_id,
                run_attempt=args.run_attempt,
                runner_os=args.runner_os,
                runner_arch=args.runner_arch,
            )
            _write_manifest(args.manifest, manifest)
            return 0
        manifest = _read_manifest(args.manifest)
        if args.command == "record":
            issues = record_execution(manifest, args.report, args.order, args.exit_code)
            _write_manifest(args.manifest, manifest)
            if issues:
                print("Test certification execution failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
                return 1
            return 0
    except (OSError, json.JSONDecodeError, ValueError) as error:
        print(f"Test certification failed: {error}", file=sys.stderr)
        return 1
    issues = verify_manifest(manifest, args.expected_sha)
    issues.extend(verify_retained_reports(manifest, args.manifest.parent))
    if issues:
        print("Test certification failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
        return 1
    print(f"Test certification passed: {manifest['testId']} has 20 retry-free executions")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
