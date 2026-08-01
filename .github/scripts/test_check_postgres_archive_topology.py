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
        cls.sources = tuple((cls.repo / path).read_text(encoding="utf-8") for path in topology.WORKFLOWS)

    def test_current_topology_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_mutable_topology_invariants_fail_closed(self) -> None:
        platform, postgres, full, repo_checks, blocking = self.sources
        cases = (
            ("archive producer artifact", 0, platform.replace("name: nextest-archive-${{ inputs.artifact_id }}", "name: wrong-archive", 1)),
            ("archive producer artifact", 0, platform.replace("name: ${{ env.TEST_HELPERS_ARTIFACT }}", "name: wrong-helper", 1)),
            ("ordinary shard artifacts", 0, replace_last(platform, "name: nextest-archive-${{ inputs.artifact_id }}", "name: wrong-archive")),
            ("ordinary shard artifacts", 0, replace_last(platform, "name: ${{ env.TEST_HELPERS_ARTIFACT }}", "name: wrong-helper")),
            ("ordinary shard selection", 0, platform.replace("shard: [1, 2, 3, 4]", "shard: [1, 2, 3]")),
            ("ordinary shard selection", 0, platform.replace('--partition "hash:${{ matrix.shard }}/4"', '--partition "hash:${{ matrix.shard }}/4"\n            -E test(smoke)')),
            ("PostgreSQL artifacts", 1, postgres.replace("name: nextest-archive-${{ inputs.artifact_id }}", "name: wrong-archive")),
            ("PostgreSQL artifacts", 1, postgres.replace("name: ${{ env.TEST_HELPERS_ARTIFACT }}", "name: wrong-helper")),
            ("PostgreSQL archive execution", 1, postgres.replace("cargo nextest run", "just test")),
            ("PostgreSQL archive execution", 1, postgres.replace("cargo nextest run", "cargo build && cargo nextest run")),
            ("PostgreSQL service and concurrency", 1, postgres.replace("image: postgres:18", "image: postgres:17")),
            ("PostgreSQL service and concurrency", 1, postgres.replace("--test-threads 4", "--test-threads 8")),
            ("exact JUnit cardinality", 1, postgres.replace("--expected-testcases", "--minimum-testcases")),
            ("platform result fail closed", 0, platform.replace('needs.postgres-contracts.result }}" != "success"', 'needs.postgres-contracts.result }}" == "success"')),
            ("x64 fifth consumer", 2, full.replace("postgres_contracts: true", "postgres_contracts: false")),
            ("standalone Merge gate", 4, blocking.replace("uses: ./.github/workflows/postgres-runtime-state-contracts.yml", "uses: ./missing.yml")),
            ("standalone Merge gate", 4, blocking.replace("- postgres-runtime-state-contracts", "- missing-postgres-gate")),
            ("repository check", 3, repo_checks.replace("check_postgres_archive_topology.py", "missing_topology.py")),
        )
        for expected, index, changed in cases:
            mutated = list(self.sources)
            mutated[index] = changed
            with self.subTest(expected=expected):
                self.assertIn(expected, "\n".join(topology.validate_topology(*mutated)))


if __name__ == "__main__":
    unittest.main()
