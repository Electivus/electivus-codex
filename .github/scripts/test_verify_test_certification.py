import copy
import contextlib
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import verify_test_certification as certification


SHA = "a" * 40


def junit_xml(test_id: str, *, name: str | None = None, classname: str | None = None, child: str = "") -> str:
    definition = certification.TESTS[test_id]
    return f'<testsuites><testsuite><testcase classname="{classname or f"{definition.package}::all"}" name="{name or definition.identity}">{child}</testcase></testsuite></testsuites>'


def valid_manifest(test_id: str = "core-pending-input") -> dict[str, object]:
    definition = certification.TESTS[test_id]
    command = certification.command_for(definition)
    return {
        "schemaVersion": 1, "candidateSha": SHA, "workflowIdentity": ".github/workflows/test-certification.yml",
        "workflowRef": "Electivus/electivus-codex/.github/workflows/test-certification.yml@refs/heads/certification/issue-89",
        "runId": "123456789", "runAttempt": 1,
        "runUrl": "https://github.com/Electivus/electivus-codex/actions/runs/123456789",
        "commitUrl": f"https://github.com/Electivus/electivus-codex/commit/{SHA}",
        "testId": test_id, "runner": "ubuntu-24.04", "runnerOs": "Linux", "runnerArch": "X64",
        "target": "x86_64-unknown-linux-gnu", "package": definition.package, "testBinary": "all", "testIdentity": definition.identity,
        "executions": [
            {
                "order": order, "command": command, "commandResult": "success", "exitCode": 0,
                "junitVerdict": "pass", "retryFree": True, "testcaseCount": 1,
                "junitPath": f"junit/{order:02}.xml", "junitSha256": f"{order:064x}",
            }
            for order in range(1, 21)
        ],
    }


class TestCertificationVerifierTests(unittest.TestCase):
    def test_verify_cli_authenticates_every_retained_junit_file(self) -> None:
        for test_id in certification.TESTS:
            with self.subTest(test_id=test_id), tempfile.TemporaryDirectory() as temp_dir:
                manifest_path, _ = self.write_evidence(Path(temp_dir), test_id)
                self.assertEqual(0, self.run_verify(manifest_path))
        with tempfile.TemporaryDirectory() as temp_dir:
            manifest_path, _ = self.write_evidence(Path(temp_dir), "core-pending-input")
            report, valid_xml = manifest_path.parent / "junit/01.xml", junit_xml("core-pending-input")
            cases = (("missing", None), ("hash mismatch", valid_xml.replace("pending_input", "other")), ("malformed", "<testsuites>"), ("skipped", junit_xml("core-pending-input", child="<skipped />")), ("retry", junit_xml("core-pending-input", child="<flakyFailure />")))
            for signal, content in cases:
                report.unlink() if content is None else report.write_text(content, encoding="utf-8")
                with self.subTest(signal=signal):
                    self.assertEqual(1, self.run_verify(manifest_path))
                report.write_text(valid_xml, encoding="utf-8")

    def test_retained_report_rejects_another_test_or_binary_even_with_matching_hash(self) -> None:
        test_id, other_id = tuple(certification.TESTS)
        for signal, xml in (("generic passed name", junit_xml(test_id, name="passed")), ("other test", junit_xml(test_id, name=certification.TESTS[other_id].identity)), ("other binary", junit_xml(test_id, classname="codex-core::other"))):
            with self.subTest(signal=signal), tempfile.TemporaryDirectory() as temp_dir:
                manifest_path, manifest = self.write_evidence(Path(temp_dir), test_id, first_xml=xml)
                issues = certification.verify_retained_reports(manifest, manifest_path.parent)
                self.assertTrue(any("expected testcase" in issue for issue in issues))
                recorded = valid_manifest(test_id); recorded["executions"] = []; self.assertNotEqual([], certification.record_execution(recorded, manifest_path.parent / "junit/01.xml", 1, 0)); self.assertEqual("fail", recorded["executions"][0]["junitVerdict"])

    def run_verify(self, manifest_path: Path) -> int:
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return certification.main(["verify", str(manifest_path), "--expected-sha", SHA])

    def write_evidence(self, directory: Path, test_id: str, first_xml: str | None = None) -> tuple[Path, dict[str, object]]:
        manifest_path, manifest = directory / "manifest.json", valid_manifest(test_id)
        (directory / "junit").mkdir()
        for execution in manifest["executions"]:
            xml = first_xml if execution["order"] == 1 and first_xml is not None else junit_xml(test_id)
            (directory / execution["junitPath"]).write_text(xml, encoding="utf-8")
            execution["junitSha256"] = hashlib.sha256(xml.encode()).hexdigest()
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        return manifest_path, manifest

    def test_complete_retry_free_sequence_is_accepted(self) -> None:
        self.assertEqual([], certification.verify_manifest(valid_manifest(), SHA))

    def test_every_manifest_invariant_fails_closed(self) -> None:
        cases = {
            "schema": lambda manifest: manifest.update(schemaVersion=2), "field types": lambda manifest: manifest.update(schemaVersion=True),
            "malformed SHA": lambda manifest: manifest.update(candidateSha="main"), "different SHA": lambda manifest: manifest.update(candidateSha="b" * 40),
            "workflow identity": lambda manifest: manifest.update(workflowIdentity="other.yml"), "workflow ref": lambda manifest: manifest.update(workflowRef="refs/heads/main"),
            "run ID": lambda manifest: manifest.update(runId="not-a-number"), "run attempt": lambda manifest: manifest.update(runAttempt=2),
            "run URL": lambda manifest: manifest.update(runUrl="https://example.com/run"), "commit URL": lambda manifest: manifest.update(commitUrl="https://example.com/commit"),
            "test ID": lambda manifest: manifest.update(testId="unknown-test"), "runner": lambda manifest: manifest.update(runner="ubuntu-24.04-arm"),
            "runner OS": lambda manifest: manifest.update(runnerOs="Windows"), "runner architecture": lambda manifest: manifest.update(runnerArch="ARM64"),
            "target": lambda manifest: manifest.update(target="aarch64-unknown-linux-gnu"), "package": lambda manifest: manifest.update(package="codex-cli"),
            "test binary": lambda manifest: manifest.update(testBinary="lib"), "test identity": lambda manifest: manifest.update(testIdentity="similar_test"),
            "execution count": lambda manifest: manifest["executions"].pop(), "execution order": lambda manifest: manifest["executions"][1].update(order=1),
            "exact command": lambda manifest: manifest["executions"][0].update(command="cargo nextest run --retries 1"), "command result": lambda manifest: manifest["executions"][0].update(commandResult="skipped"),
            "exit code": lambda manifest: manifest["executions"][0].update(exitCode=1), "JUnit verdict": lambda manifest: manifest["executions"][0].update(junitVerdict="fail"),
            "retry evidence": lambda manifest: manifest["executions"][0].update(retryFree=False), "testcase count": lambda manifest: manifest["executions"][0].update(testcaseCount=0),
            "JUnit path": lambda manifest: manifest["executions"][0].update(junitPath="junit.xml"), "JUnit hash": lambda manifest: manifest["executions"][0].update(junitSha256="missing"),
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
            report.write_text(junit_xml("core-pending-input"), encoding="utf-8")
            self.assertEqual([], certification.record_execution(manifest, report, 1, 0))
            self.assertEqual(
                ("success", "pass", True, 1),
                tuple(manifest["executions"][0][field] for field in ("commandResult", "junitVerdict", "retryFree", "testcaseCount")),
            )
            report.write_text(junit_xml("core-pending-input", child="<flakyFailure />"), encoding="utf-8")
            issues = certification.record_execution(manifest, report, 2, 0)
            self.assertTrue(any("retry evidence" in issue for issue in issues))
            self.assertEqual(("fail", False), (manifest["executions"][1]["junitVerdict"], manifest["executions"][1]["retryFree"]))

    def test_atomic_manifest_write_preserves_previous_file_on_serialization_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "manifest.json"
            path.write_text('{"previous": true}\n', encoding="utf-8")
            with mock.patch.object(certification.json, "dump", side_effect=OSError("injected write failure")), self.assertRaises(OSError):
                certification._write_manifest(path, {"replacement": True})
            self.assertEqual('{"previous": true}\n', path.read_text(encoding="utf-8"))
            self.assertEqual([path], list(path.parent.iterdir()))
            certification._write_manifest(path, {"replacement": True})
            self.assertEqual({"replacement": True}, json.loads(path.read_text(encoding="utf-8")))

    def test_initialization_rejects_a_ref_or_rerun_as_candidate_evidence(self) -> None:
        common = {"test_id": "core-pending-input", "workflow_ref": "Electivus/electivus-codex/.github/workflows/test-certification.yml@refs/heads/main", "run_id": "123456789", "runner_os": "Linux", "runner_arch": "X64"}
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
