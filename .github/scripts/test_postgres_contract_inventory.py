from pathlib import Path
import tempfile
import unittest

import check_rust_test_policy as rust_policy
import postgres_contract_inventory as inventory


DB_REASON = 'ignore="requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"'
PROCESS_REASON = (
    'ignore="requires the PostgreSQL Runtime State process contract environment"'
)
EXPECTED_FILTER = (
    "(package(codex-app-server) | package(codex-app-server-transport) | "
    "package(codex-cli) | package(codex-memories-write) | package(codex-state) | "
    "package(codex-thread-store)) & "
    "test(/postgres_contract_|state_(migrate|initialize)_process_/)"
)


class PostgresContractInventoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.repo = Path(self.temp_dir.name)
        self.packages = (
            "codex-app-server",
            "codex-app-server-transport",
            "codex-cli",
            "codex-memories-write",
            "codex-state",
            "codex-thread-store",
        )
        self.package_paths = {
            "codex-app-server": "codex-rs/app-server",
            "codex-app-server-transport": "codex-rs/app-server-transport",
            "codex-cli": "codex-rs/cli",
            "codex-memories-write": "codex-rs/memories/write",
            "codex-state": "codex-rs/state",
            "codex-thread-store": "codex-rs/thread-store",
        }
        for package, relative in self.package_paths.items():
            manifest = self.repo / relative / "Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text(f'[package]\nname = "{package}"\n', encoding="utf-8")

    def occurrence(
        self, package: str, test: str, attribute: str = DB_REASON
    ) -> rust_policy.IgnoreOccurrence:
        path = f"{self.package_paths[package]}/src/contracts.rs"
        return rust_policy.IgnoreOccurrence(path, test, attribute)

    def policy_for(
        self, occurrences: list[rust_policy.IgnoreOccurrence]
    ) -> dict[str, object]:
        return {
            "postgres_contracts": {
                "database_count": 6,
                "process_count": 2,
                "packages": list(self.packages),
            },
            "ignores": {
                occurrence.identity: "specialized-environment"
                for occurrence in occurrences
            },
        }

    def baseline_occurrences(self) -> list[rust_policy.IgnoreOccurrence]:
        return [
            self.occurrence(package, f"postgres_contract_{index}")
            for index, package in enumerate(self.packages)
        ] + [
            self.occurrence(
                "codex-app-server",
                "state_migrate_process_rejects_invalid_config",
                PROCESS_REASON,
            ),
            self.occurrence(
                "codex-cli",
                "state_initialize_process_creates_namespace",
                PROCESS_REASON,
            ),
        ]

    def test_valid_inventory_exports_one_filter_for_all_packages(self) -> None:
        occurrences = self.baseline_occurrences()

        self.assertEqual(
            (
                inventory.InventorySummary(
                    database_count=6,
                    process_count=2,
                    total_count=8,
                    packages=self.packages,
                    nextest_filter=EXPECTED_FILTER,
                ),
                [],
            ),
            inventory.validate_inventory(
                self.repo, occurrences, self.policy_for(occurrences)
            ),
        )

    def test_inventory_rejects_manifest_and_source_drift(self) -> None:
        baseline = self.baseline_occurrences()
        cases: list[
            tuple[
                str,
                list[rust_policy.IgnoreOccurrence],
                dict[str, object],
                list[str],
            ]
        ] = []

        added = self.occurrence("codex-state", "postgres_contract_new")
        cases.append(
            (
                "new matching contract is unclassified",
                baseline + [added],
                self.policy_for(baseline),
                [
                    f"unclassified PostgreSQL contract: {added.identity}",
                    "PostgreSQL database contract count changed: expected 6, found 7",
                ],
            )
        )

        stale_policy = self.policy_for(baseline)
        stale_identity = (
            'codex-rs/state/src/stale.rs::postgres_contract_stale::'
            + DB_REASON
        )
        stale_policy["ignores"][stale_identity] = "specialized-environment"
        cases.append(
            (
                "stale contract classification",
                baseline,
                stale_policy,
                [f"stale specialized-environment classification: {stale_identity}"],
            )
        )

        wrong_category_policy = self.policy_for(baseline)
        wrong_category_policy["ignores"][baseline[0].identity] = "manual-smoke"
        cases.append(
            (
                "wrong category",
                baseline,
                wrong_category_policy,
                [
                    f"{baseline[0].identity}: PostgreSQL contract category must be specialized-environment"
                ],
            )
        )

        wrong_reason = self.occurrence(
            "codex-state",
            "postgres_contract_0",
            'ignore="requires any database"',
        )
        wrong_reason_occurrences = [wrong_reason, *baseline[1:]]
        cases.append(
            (
                "wrong reason",
                wrong_reason_occurrences,
                self.policy_for(wrong_reason_occurrences),
                [
                    f"{wrong_reason.identity}: PostgreSQL contract has an unsupported ignore reason",
                    "PostgreSQL database contract count changed: expected 6, found 5",
                ],
            )
        )

        wrong_name = self.occurrence("codex-state", "database_smoke")
        wrong_name_occurrences = [wrong_name, *baseline[1:]]
        cases.append(
            (
                "wrong selection name",
                wrong_name_occurrences,
                self.policy_for(wrong_name_occurrences),
                [
                    f"{wrong_name.identity}: PostgreSQL contract test name does not match the selection convention",
                    "PostgreSQL database contract count changed: expected 6, found 5",
                ],
            )
        )

        unrelated = self.occurrence(
            "codex-state", "ordinary_test", 'ignore="named environment"'
        )
        unrelated_occurrences = baseline + [unrelated]
        cases.append(
            (
                "nonmatching specialized entry",
                unrelated_occurrences,
                self.policy_for(unrelated_occurrences),
                [
                    f"{unrelated.identity}: specialized-environment entry is not a PostgreSQL contract"
                ],
            )
        )

        changed_count_policy = self.policy_for(baseline)
        changed_count_policy["postgres_contracts"]["process_count"] = 3
        cases.append(
            (
                "changed explicit count",
                baseline,
                changed_count_policy,
                ["PostgreSQL process contract count changed: expected 3, found 2"],
            )
        )

        drifted_packages_policy = self.policy_for(baseline)
        drifted_packages_policy["postgres_contracts"]["packages"] = [
            *self.packages[:-1],
            "codex-unexpected",
        ]
        cases.append(
            (
                "crate drift",
                baseline,
                drifted_packages_policy,
                [
                    "PostgreSQL contract packages changed: expected "
                    "codex-app-server, codex-app-server-transport, codex-cli, "
                    "codex-memories-write, codex-state, codex-unexpected; found "
                    "codex-app-server, codex-app-server-transport, codex-cli, "
                    "codex-memories-write, codex-state, codex-thread-store"
                ],
            )
        )

        for name, occurrences, policy, expected_issues in cases:
            with self.subTest(name=name):
                self.assertEqual(
                    (None, expected_issues),
                    inventory.validate_inventory(self.repo, occurrences, policy),
                )


if __name__ == "__main__":
    unittest.main()
