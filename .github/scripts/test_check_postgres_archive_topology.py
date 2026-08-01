from pathlib import Path
import unittest

import check_postgres_archive_topology as topology


class PostgresArchiveTopologyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo = Path(__file__).resolve().parents[2]
        cls.sources = tuple((cls.repo / path).read_text(encoding="utf-8") for path in topology.WORKFLOWS)

    def test_current_topology_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_mutable_topology_invariants_fail_closed(self) -> None:
        platform, postgres, full, repo_checks = self.sources
        cases = (
            ("four partitions", 0, platform.replace("shard: [1, 2, 3, 4]", "shard: [1, 2, 3]")),
            ("single archive producer", 0, platform.replace("cargo nextest archive", "cargo nextest run")),
            ("x64 fifth consumer", 2, full.replace("postgres_contracts: true", "postgres_contracts: false")),
            ("no archive-consumer compilation", 1, postgres.replace("cargo nextest run", "cargo build && cargo nextest run")),
            ("one PostgreSQL 18 service", 1, postgres.replace("image: postgres:18", "image: postgres:17")),
            ("PostgreSQL concurrency four", 1, postgres.replace("--test-threads 4", "--test-threads 8")),
            ("exact JUnit cardinality", 1, postgres.replace("--expected-testcases", "--minimum-testcases")),
            ("repository check", 3, repo_checks.replace("check_postgres_archive_topology.py", "missing_topology.py")),
        )
        for expected, index, changed in cases:
            mutated = list(self.sources)
            mutated[index] = changed
            with self.subTest(expected=expected):
                self.assertIn(expected, "\n".join(topology.validate_topology(*mutated)))


if __name__ == "__main__":
    unittest.main()
