from pathlib import Path
import unittest

import check_test_certification_topology as topology


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
            ("immutable Linux x64", 0, workflow.replace("runs-on: ubuntu-24.04", "runs-on: ubuntu-24.04-arm")),
            ("two independent tests", 0, workflow.replace("fail-fast: false", "fail-fast: true")),
            ("exact identities", 0, workflow.replace("injected_user_input_triggers_follow_up_request_with_deltas", "similar_pending_input_test")),
            ("exact identities", 1, verifier.replace("review_start_exec_approval_item_id_matches_command_execution_item", "similar_review_test")),
            ("twenty ordered executions", 0, workflow.replace("seq 1 20", "seq 1 19")),
            ("retry-free exact nextest", 0, workflow.replace("--retries 0", "--retries 1")),
            ("retry-free exact nextest", 0, workflow.replace("--run-ignored=only", "--run-ignored=all")),
            ("single JUnit testcase", 0, workflow.replace("--expected-testcases 1", "--expected-testcases 2")),
            ("unexpected skip", 4, junit.replace('SKIP_ELEMENTS = {"skipped"}', "SKIP_ELEMENTS = set()")),
            ("stop failed sequence", 0, workflow.replace("break", "continue", 1)),
            ("retained evidence", 0, workflow.replace("retention-days: 90", "retention-days: 1")),
            ("manifest lifecycle", 0, workflow.replace('verify_test_certification.py" verify', 'missing_verifier.py" verify')),
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
