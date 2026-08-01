import copy
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest

import verify_test_certification as certification


SHA = "a" * 40


def valid_manifest(test_id: str = "core-pending-input") -> dict[str, object]:
    definition = certification.TESTS[test_id]
    command = certification.command_for(definition)
    return {
        "schemaVersion": 1,
        "candidateSha": SHA,
        "workflowIdentity": ".github/workflows/test-certification.yml",
        "workflowRef": "Electivus/electivus-codex/.github/workflows/test-certification.yml@refs/heads/certification/issue-89",
        "runId": "123456789",
        "runAttempt": 1,
        "runUrl": "https://github.com/Electivus/electivus-codex/actions/runs/123456789",
        "commitUrl": f"https://github.com/Electivus/electivus-codex/commit/{SHA}",
        "testId": test_id,
        "runner": "ubuntu-24.04",
        "runnerOs": "Linux",
        "runnerArch": "X64",
        "target": "x86_64-unknown-linux-gnu",
        "package": definition.package,
        "testBinary": "all",
        "testIdentity": definition.identity,
        "executions": [
            {
                "order": order,
                "command": command,
                "commandResult": "success",
                "exitCode": 0,
                "junitVerdict": "pass",
                "retryFree": True,
                "testcaseCount": 1,
                "junitPath": f"junit/{order:02}.xml",
                "junitSha256": f"{order:064x}",
            }
            for order in range(1, 21)
        ],
    }


class TestCertificationVerifierTests(unittest.TestCase):
    def test_verify_cli_accepts_only_the_complete_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            manifest_path = Path(temp_dir) / "manifest.json"
            manifest = valid_manifest()
            for expected, candidate in ((0, manifest), (1, manifest | {"executions": manifest["executions"][:-1]})):
                manifest_path.write_text(json.dumps(candidate), encoding="utf-8")
                with self.subTest(expected=expected), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(expected, certification.main(["verify", str(manifest_path), "--expected-sha", SHA]))

    def test_complete_retry_free_sequence_is_accepted(self) -> None:
        self.assertEqual([], certification.verify_manifest(valid_manifest(), SHA))

    def test_every_manifest_invariant_fails_closed(self) -> None:
        cases = {
            "schema": lambda manifest: manifest.update(schemaVersion=2),
            "field types": lambda manifest: manifest.update(schemaVersion=True),
            "malformed SHA": lambda manifest: manifest.update(candidateSha="main"),
            "different SHA": lambda manifest: manifest.update(candidateSha="b" * 40),
            "workflow identity": lambda manifest: manifest.update(workflowIdentity="other.yml"),
            "workflow ref": lambda manifest: manifest.update(workflowRef="refs/heads/main"),
            "run ID": lambda manifest: manifest.update(runId="not-a-number"),
            "run attempt": lambda manifest: manifest.update(runAttempt=2),
            "run URL": lambda manifest: manifest.update(runUrl="https://example.com/run"),
            "commit URL": lambda manifest: manifest.update(commitUrl="https://example.com/commit"),
            "test ID": lambda manifest: manifest.update(testId="unknown-test"),
            "runner": lambda manifest: manifest.update(runner="ubuntu-24.04-arm"),
            "runner OS": lambda manifest: manifest.update(runnerOs="Windows"),
            "runner architecture": lambda manifest: manifest.update(runnerArch="ARM64"),
            "target": lambda manifest: manifest.update(target="aarch64-unknown-linux-gnu"),
            "package": lambda manifest: manifest.update(package="codex-cli"),
            "test binary": lambda manifest: manifest.update(testBinary="lib"),
            "test identity": lambda manifest: manifest.update(testIdentity="similar_test"),
            "execution count": lambda manifest: manifest["executions"].pop(),
            "execution order": lambda manifest: manifest["executions"][1].update(order=1),
            "exact command": lambda manifest: manifest["executions"][0].update(command="cargo nextest run --retries 1"),
            "command result": lambda manifest: manifest["executions"][0].update(commandResult="skipped"),
            "exit code": lambda manifest: manifest["executions"][0].update(exitCode=1),
            "JUnit verdict": lambda manifest: manifest["executions"][0].update(junitVerdict="fail"),
            "retry evidence": lambda manifest: manifest["executions"][0].update(retryFree=False),
            "testcase count": lambda manifest: manifest["executions"][0].update(testcaseCount=0),
            "JUnit path": lambda manifest: manifest["executions"][0].update(junitPath="junit.xml"),
            "JUnit hash": lambda manifest: manifest["executions"][0].update(junitSha256="missing"),
        }
        for invariant, mutate in cases.items():
            manifest = copy.deepcopy(valid_manifest())
            mutate(manifest)
            with self.subTest(invariant=invariant):
                self.assertNotEqual([], certification.verify_manifest(manifest, SHA))

    def test_recording_uses_required_junit_signal_and_stops_on_retry_evidence(self) -> None:
        manifest = valid_manifest()
        manifest["executions"] = []
        with tempfile.TemporaryDirectory() as temp_dir:
            report = Path(temp_dir) / "junit.xml"
            report.write_text(
                '<testsuites failures="0"><testsuite><testcase name="passed" /></testsuite></testsuites>',
                encoding="utf-8",
            )
            self.assertEqual([], certification.record_execution(manifest, report, 1, 0))
            self.assertEqual(
                ("success", "pass", True, 1),
                tuple(manifest["executions"][0][field] for field in ("commandResult", "junitVerdict", "retryFree", "testcaseCount")),
            )
            report.write_text(
                '<testsuites><testsuite><testcase name="retried"><flakyFailure /></testcase></testsuite></testsuites>',
                encoding="utf-8",
            )
            issues = certification.record_execution(manifest, report, 2, 0)
            self.assertTrue(any("retry evidence" in issue for issue in issues))
            self.assertEqual(("fail", False), (manifest["executions"][1]["junitVerdict"], manifest["executions"][1]["retryFree"]))

    def test_initialization_rejects_a_ref_or_rerun_as_candidate_evidence(self) -> None:
        common = {
            "test_id": "core-pending-input",
            "workflow_ref": "Electivus/electivus-codex/.github/workflows/test-certification.yml@refs/heads/main",
            "run_id": "123456789",
            "runner_os": "Linux",
            "runner_arch": "X64",
        }
        for candidate_sha, checked_out_sha, run_attempt in (("main", SHA, 1), (SHA, "b" * 40, 1), (SHA, SHA, 2)):
            with self.subTest(candidate_sha=candidate_sha, checked_out_sha=checked_out_sha, run_attempt=run_attempt):
                with self.assertRaises(ValueError):
                    certification.new_manifest(candidate_sha=candidate_sha, checked_out_sha=checked_out_sha, run_attempt=run_attempt, **common)
        for field, value in (("workflow_ref", "refs/heads/main"), ("run_id", "rerun"), ("test_id", "other")):
            changed = common | {field: value}
            with self.subTest(field=field):
                with self.assertRaises(ValueError):
                    certification.new_manifest(candidate_sha=SHA, checked_out_sha=SHA, run_attempt=1, **changed)


if __name__ == "__main__":
    unittest.main()
