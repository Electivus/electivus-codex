from pathlib import Path
import unittest

import check_upstream_sync_gate_topology as topology


class UpstreamSyncGateTopologyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo = Path(__file__).resolve().parents[2]
        cls.sources = tuple(
            (cls.repo / path).read_text(encoding="utf-8") for path in topology.SOURCES
        )

    def test_current_wiring_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_wiring_mutations_fail_closed(self) -> None:
        blocking, repo_checks, checker, tests = self.sources
        cases = (
            (
                "real head checkout",
                0,
                blocking.replace("fetch-depth: 0", "fetch-depth: 1", 1),
            ),
            (
                "real head checkout",
                0,
                blocking.replace("pull_request.head.sha", "pull_request.base.sha", 1),
            ),
            (
                "real PR identity",
                0,
                blocking.replace("PR_BASE_SHA:", "WRONG_BASE_SHA:", 1),
            ),
            (
                "checker invocation",
                0,
                blocking.replace(
                    "check_upstream_sync_topology.py",
                    "missing_sync_topology.py",
                    1,
                ),
            ),
            (
                "checker invocation",
                0,
                blocking.replace(
                    'tee -a "$GITHUB_STEP_SUMMARY"',
                    'tee "$GITHUB_STEP_SUMMARY"',
                    1,
                ),
            ),
            (
                "required aggregate",
                0,
                blocking.replace(
                    "- synchronization-topology",
                    "- missing-synchronization-topology",
                    1,
                ),
            ),
            (
                "repository wiring test",
                1,
                repo_checks.replace(
                    "check_upstream_sync_gate_topology.py",
                    "missing_gate_topology.py",
                ),
            ),
            (
                "repository wiring test",
                2,
                checker.replace("validate_topology", "missing_checker"),
            ),
            (
                "repository wiring test",
                3,
                tests.replace("TopologyEvidence", "MissingEvidence"),
            ),
        )
        for expected, index, changed in cases:
            mutated = list(self.sources)
            mutated[index] = changed
            with self.subTest(expected=expected):
                self.assertIn(
                    expected,
                    "\n".join(topology.validate_topology(*mutated)),
                )


if __name__ == "__main__":
    unittest.main()
