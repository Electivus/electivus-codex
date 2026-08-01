from pathlib import Path
import unittest

import check_postgres_archive_topology as topology


def replace_last(source: str, old: str, new: str) -> str:
    before, found, after = source.rpartition(old)
    return before + new + after if found else source


class PostgresArchiveTopologyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo = Path(__file__).resolve().parents[2]
        cls.sources = tuple((cls.repo / path).read_text(encoding="utf-8") for path in topology.SOURCES)

    def test_current_topology_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_deep_linux_cargo_replaces_the_standalone_postgres_gate(self) -> None:
        blocking = self.sources[4]
        self.assertNotIn("  postgres-runtime-state-contracts:\n", blocking)
        self.assertIn("  deep-linux-cargo:\n", blocking)
        self.assertIn("validation_scope: merge-gate", blocking)
        self.assertIn("  deep-linux-cargo-result:\n", blocking)

    def test_mutable_topology_invariants_fail_closed(self) -> None:
        platform, postgres, full, repo_checks, blocking, planner, result_helper = self.sources
        cases = (
            ("archive producer artifact", 0, platform.replace("name: nextest-archive-${{ inputs.artifact_id }}", "name: wrong-archive", 1)),
            ("archive producer artifact", 0, platform.replace("name: ${{ env.TEST_HELPERS_ARTIFACT }}", "name: wrong-helper", 1)),
            ("ordinary shard artifacts", 0, replace_last(platform, "name: nextest-archive-${{ inputs.artifact_id }}", "name: wrong-archive")),
            ("ordinary shard artifacts", 0, replace_last(platform, "name: ${{ env.TEST_HELPERS_ARTIFACT }}", "name: wrong-helper")),
            ("ordinary shard selection", 0, platform.replace("shard: [1, 2, 3, 4]", "shard: [1, 2, 3]")),
            ("ordinary shard selection", 0, platform.replace('--partition "hash:${{ matrix.shard }}/4"', '--partition "hash:${{ matrix.shard }}/4"\n            -E test(smoke)')),
            ("checkout revision identity", 1, postgres.replace("          persist-credentials: false", "          ref: ${{ github.event.pull_request.head.sha }}\n          persist-credentials: false", 1)),
            ("PostgreSQL artifacts", 1, postgres.replace("name: nextest-archive-${{ inputs.artifact_id }}", "name: wrong-archive")),
            ("PostgreSQL artifacts", 1, postgres.replace("name: ${{ env.TEST_HELPERS_ARTIFACT }}", "name: wrong-helper")),
            ("PostgreSQL archive execution", 1, postgres.replace("cargo nextest run", "just test")),
            ("PostgreSQL archive execution", 1, postgres.replace("cargo nextest run", "cargo build && cargo nextest run")),
            ("PostgreSQL service and concurrency", 1, postgres.replace("image: postgres:18", "image: postgres:17")),
            ("PostgreSQL service and concurrency", 1, postgres.replace("--test-threads 4", "--test-threads 8")),
            ("exact JUnit cardinality", 1, postgres.replace("--expected-testcases", "--minimum-testcases")),
            ("platform result fail closed", 0, platform.replace('needs.postgres-contracts.result }}" != "success"', 'needs.postgres-contracts.result }}" == "success"')),
            ("platform result fail closed", 0, platform.replace('needs.shard.result }}" != "success"', 'needs.shard.result }}" == "success"')),
            ("x64 fifth consumer", 2, full.replace("postgres_contracts: true", "postgres_contracts: false")),
            ("eligible Cargo promotion", 4, blocking.replace("needs.deep-linux-eligibility.outputs.eligible == 'true'", "needs.deep-linux-eligibility.outputs.eligible == 'false'")),
            ("bounded Cargo result", 4, blocking.replace("needs: [deep-linux-eligibility, deep-linux-cargo]\n    if: ${{ always() }}", "needs: [deep-linux-eligibility, deep-linux-cargo]\n    if: ${{ needs.deep-linux-cargo.result == 'success' }}")),
            ("eligible Cargo promotion", 4, blocking.replace("validation_scope: merge-gate", "validation_scope: full")),
            ("merge-gate lint matrix", 2, full.replace("cargo clippy --workspace", "cargo clippy -p codex-core")),
            ("merge-gate lint matrix", 5, planner.replace('LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "release")', 'LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "dev")', 1)),
            ("full Extended matrix", 5, planner.replace('LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "dev")', 'LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-unknown", "dev")', 1)),
            ("merge-gate schedules only x64", 2, full.replace("needs.plan.outputs.run_x64 == 'true'", "needs.plan.outputs.run_arm64 == 'true'")),
            ("eligible Cargo promotion", 4, blocking.replace("  repo-checks:\n", "  postgres-runtime-state-contracts:\n    uses: ./.github/workflows/postgres-runtime-state-contracts.yml\n\n  repo-checks:\n")),
            ("eligible Cargo promotion", 4, blocking.replace("  deep-linux-cargo:\n", "  missing-deep-linux-cargo:\n")),
            ("required aggregate promotion", 4, blocking.replace("- deep-linux-cargo-result", "- deep-linux-cargo")),
            ("scope-aware full result", 6, result_helper.replace("actual != wanted", "actual == wanted")),
            ("validation scope fails safe", 5, planner.replace("defaults fail-safe to full", "defaults to extended")),
            ("repository check", 3, repo_checks.replace("check_postgres_archive_topology.py", "missing_topology.py")),
        )
        for expected, index, changed in cases:
            mutated = list(self.sources)
            mutated[index] = changed
            with self.subTest(expected=expected):
                self.assertIn(expected, "\n".join(topology.validate_topology(*mutated)))


if __name__ == "__main__":
    unittest.main()
