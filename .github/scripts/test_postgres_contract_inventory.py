from pathlib import Path
import tempfile
import unittest

import check_rust_test_policy as rust_policy
import postgres_contract_inventory as inventory


DB = 'ignore="requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"'
PROCESS = 'ignore="requires the PostgreSQL Runtime State process contract environment"'
FILTER = (
    "(package(codex-app-server) | package(codex-app-server-transport) | package(codex-cli) | "
    "package(codex-memories-write) | package(codex-state) | package(codex-thread-store)) & "
    "test(/(^|::)(postgres_contract_|state_(migrate|initialize)_process_)/)"
)


class PostgresContractInventoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.locations = {
            "codex-app-server": "app-server",
            "codex-app-server-transport": "app-server-transport",
            "codex-cli": "cli",
            "codex-memories-write": "memories/write",
            "codex-state": "state",
            "codex-thread-store": "thread-store",
        }
        self.packages = tuple(self.locations)
        for package, location in self.locations.items():
            manifest = self.repo / "codex-rs" / location / "Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text(f'[package]\nname = "{package}"\n', encoding="utf-8")
        self.baseline = [
            self.occurrence(package, f"postgres_contract_{index}")
            for index, package in enumerate(self.packages)
        ] + [
            self.occurrence("codex-app-server", "postgres_contract_process_a", PROCESS),
            self.occurrence("codex-cli", "state_initialize_process_b", PROCESS),
        ]

    def occurrence(self, package: str, test: str, reason: str = DB):
        path = f"codex-rs/{self.locations[package]}/src/contracts.rs"
        return rust_policy.IgnoreOccurrence(path, test, reason)

    @staticmethod
    def functions(occurrences):
        return [rust_policy.RustFunctionOccurrence(item.path, item.test) for item in occurrences]

    def policy(self, occurrences):
        return {
            "postgres_contracts": {"database_count": 6, "process_count": 2, "packages": list(self.packages)},
            "ignores": {item.identity: "specialized-environment" for item in occurrences},
        }

    def expected(self):
        return inventory.InventorySummary(6, 2, 8, self.packages, FILTER)

    def validate(self, occurrences=None, functions=None, policy=None):
        occurrences = occurrences or self.baseline
        return inventory.validate_inventory(
            self.repo,
            occurrences,
            functions or self.functions(occurrences),
            policy or self.policy(occurrences),
        )

    def test_valid_inventory_exports_one_anchored_filter(self) -> None:
        self.assertEqual((self.expected(), []), self.validate())

    def test_unignored_prefix_fails_while_substring_and_other_environment_do_not(self) -> None:
        unignored = rust_policy.RustFunctionOccurrence(
            "codex-rs/state/src/contracts.rs", "postgres_contract_unignored"
        )
        substring = rust_policy.RustFunctionOccurrence(
            "codex-rs/state/src/contracts.rs", "helper_postgres_contract_child"
        )
        self.assertEqual(
            (None, [f"PostgreSQL test function lacks an accepted ignore: {unignored.identity}"]),
            self.validate(functions=self.functions(self.baseline) + [unignored, substring]),
        )
        other = self.occurrence("codex-state", "ordinary_test", 'ignore="named environment"')
        occurrences = self.baseline + [other]
        self.assertEqual((self.expected(), []), self.validate(occurrences, policy=self.policy(occurrences)))

    def test_manifest_and_source_drift_fail_with_complete_diagnostics(self) -> None:
        added = self.occurrence("codex-state", "postgres_contract_new")
        stale = f"codex-rs/state/src/stale.rs::postgres_contract_stale::{DB}"
        stale_policy = self.policy(self.baseline)
        stale_policy["ignores"][stale] = "specialized-environment"
        wrong_category = self.policy(self.baseline)
        wrong_category["ignores"][self.baseline[0].identity] = "manual-smoke"
        wrong_reason = self.occurrence("codex-state", "postgres_contract_0", 'ignore="any db"')
        wrong_name = self.occurrence("codex-state", "database_smoke")
        wrong_count = self.policy(self.baseline)
        wrong_count["postgres_contracts"]["process_count"] = 3
        wrong_crates = self.policy(self.baseline)
        wrong_crates["postgres_contracts"]["packages"][-1] = "codex-unexpected"
        cases = (
            (self.baseline + [added], None, self.policy(self.baseline), [f"unclassified PostgreSQL contract: {added.identity}", "PostgreSQL database contract count changed: expected 6, found 7"]),
            (self.baseline, None, stale_policy, [f"stale specialized-environment classification: {stale}"]),
            (self.baseline, None, wrong_category, [f"{self.baseline[0].identity}: PostgreSQL contract category must be specialized-environment"]),
            ([wrong_reason, *self.baseline[1:]], None, None, [f"{wrong_reason.identity}: PostgreSQL contract has an unsupported ignore reason", "PostgreSQL database contract count changed: expected 6, found 5"]),
            ([wrong_name, *self.baseline[1:]], None, None, [f"{wrong_name.identity}: PostgreSQL contract test name does not match the selection convention", "PostgreSQL database contract count changed: expected 6, found 5"]),
            (self.baseline, None, wrong_count, ["PostgreSQL process contract count changed: expected 3, found 2"]),
            (self.baseline, None, wrong_crates, ["PostgreSQL contract packages changed: expected codex-app-server, codex-app-server-transport, codex-cli, codex-memories-write, codex-state, codex-unexpected; found codex-app-server, codex-app-server-transport, codex-cli, codex-memories-write, codex-state, codex-thread-store"]),
        )
        for occurrences, functions, policy, issues in cases:
            with self.subTest(issues=issues):
                self.assertEqual((None, issues), self.validate(occurrences, functions, policy))


if __name__ == "__main__":
    unittest.main()
