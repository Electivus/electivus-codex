from pathlib import Path
import unittest

import check_test_certification_topology as topology


def displaced(source: str, old: str, new: str, decoy: str | None = None) -> str:
    changed = source.replace(old, new, 1)
    return changed.replace("jobs:", f"# unrelated decoy: {decoy or old}\njobs:", 1)


class TestCertificationTopologyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        repo = Path(__file__).resolve().parents[2]
        cls.sources = tuple((repo / path).read_text(encoding="utf-8") for path in topology.SOURCES)

    def test_current_certification_topology_is_exact(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_certification_contract_mutations_fail_closed(self) -> None:
        workflow, verifier, repo_checks, policy, junit, platform, rust = self.sources
        cases = (
            ("isolated trigger", 0, workflow.replace("certification/issue-89", "main", 1)),
            ("immutable Linux x64", 0, displaced(workflow, 'ref: ${{ env.CANDIDATE_SHA }}', 'ref: main')),
            ("immutable Linux x64", 0, displaced(workflow, '--candidate-sha "${CANDIDATE_SHA}"', '--candidate-sha "main"')),
            ("immutable Linux x64", 0, displaced(workflow, '--run-id "${GITHUB_RUN_ID}"', '--run-id "forged"')),
            ("immutable Linux x64", 0, displaced(workflow, '--run-attempt "${GITHUB_RUN_ATTEMPT}"', '--run-attempt 1')),
            ("immutable Linux x64", 0, displaced(workflow, '--runner-os "${RUNNER_OS}"', '--runner-os "Linux"')),
            ("hosted capacity and stack", 0, workflow.replace('RUST_MIN_STACK: "8388608"', 'RUST_MIN_STACK: "1048576"')),
            ("hosted capacity and stack", 0, workflow.replace('CARGO_PROFILE_CI_TEST_DEBUG: "0"', 'CARGO_PROFILE_CI_TEST_DEBUG: "1"')), ("hosted capacity and stack", 0, displaced(workflow, "tool: nextest@0.9.103", "tool: nextest\n          version: 0.9.103")), ("hosted capacity and stack", 0, displaced(workflow, "tool: nextest@0.9.103", "tool: nextest")),
            ("hosted capacity and stack", 0, displaced(workflow, "sudo rm -rf", "echo keep-hosted-images")),
            ("two independent tests", 0, workflow.replace("fail-fast: false", "fail-fast: true")),
            ("exact identities", 0, workflow.replace("injected_user_input_triggers_follow_up_request_with_deltas", "similar_pending_input_test")),
            ("exact identities", 1, verifier.replace("review_start_exec_approval_item_id_matches_command_execution_item", "similar_review_test")),
            ("twenty ordered executions", 0, displaced(workflow, "seq 1 20", "seq 1 19")),
            ("retry-free exact nextest", 0, displaced(workflow, "--retries 0", "--retries 1")), ("retry-free exact nextest", 0, displaced(workflow, "--cargo-profile ci-test", "--cargo-profile test")),
            ("retry-free exact nextest", 0, displaced(workflow, "--run-ignored=only", "--run-ignored=all")), ("retry-free exact nextest", 1, verifier.replace("--cargo-profile ci-test", "--cargo-profile test")),
            ("single JUnit testcase", 0, displaced(workflow, "--expected-testcases 1", "--expected-testcases 2")), ("single JUnit testcase", 0, displaced(workflow, 'junit_source="${GITHUB_WORKSPACE}/codex-rs/target/nextest/default/junit.xml"', 'junit_source="${CARGO_TARGET_DIR}/nextest/default/junit.xml"')),
            ("unexpected skip", 4, junit.replace('SKIP_ELEMENTS = {"skipped"}', "SKIP_ELEMENTS = set()")),
            ("stop failed sequence", 0, displaced(workflow, "break", "continue")),
            ("retained evidence", 0, displaced(workflow, 'verify_test_certification.py" verify', 'missing_verifier.py" verify')), ("retained evidence", 0, displaced(workflow, 'exit "${status}"', "true")), ("retained evidence", 0, displaced(workflow, 'exit "${status}"', 'true # exit "${status}"')), ("retained evidence", 0, displaced(workflow, "      - name: Verify and summarize independent sequence\n        if: always()", "      - name: Verify and summarize independent sequence\n        if: always()\n        continue-on-error: true", "if: always()")),
            ("retained evidence", 0, displaced(workflow, "      - name: Upload retained certification evidence\n        if: always()", "      - name: Upload retained certification evidence\n        if: success()", "if: always()")), ("retained evidence", 0, displaced(workflow, "      - name: Upload retained certification evidence\n        if: always()", "      - name: Upload retained certification evidence\n        if: always()\n        continue-on-error: true", "if: always()")),
            ("retained evidence", 0, displaced(workflow, "name: test-certification-${{ matrix.test_id }}-${{ env.CANDIDATE_SHA }}", "name: test-certification")),
            ("retained evidence", 0, displaced(workflow, "if-no-files-found: error", "if-no-files-found: warn")),
            ("retained evidence", 0, displaced(workflow, "retention-days: 90", "retention-days: 1")), ("retained evidence", 0, displaced(workflow, "    runs-on: ubuntu-24.04", "    runs-on: ubuntu-24.04\n    continue-on-error: true", "runs-on: ubuntu-24.04")),
            ("manifest lifecycle", 0, displaced(workflow, 'verify_test_certification.py" record', 'missing_verifier.py" record')),
            ("manifest lifecycle", 1, verifier.replace("issues.extend(verify_retained_reports(manifest, args.manifest.parent))", "pass")),
            ("temporary policy", 3, policy.replace("stale Rust ignore classification", "stale entry ignored")),
            ("ordinary reactivation path", 5, platform.replace('--partition "hash:${{ matrix.shard }}/4"', '--run-ignored=only\n            --partition "hash:${{ matrix.shard }}/4"', 1)),
            ("repository check", 2, repo_checks.replace("check_test_certification_topology.py", "missing_certification_topology.py")),
        )
        for expected, index, changed in cases:
            sources = list(self.sources)
            sources[index] = changed
            with self.subTest(expected=expected):
                self.assertIn(expected, "\n".join(topology.validate_topology(*sources)))


if __name__ == "__main__":
    unittest.main()
