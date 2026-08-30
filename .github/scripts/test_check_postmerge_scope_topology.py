import copy
import json
from pathlib import Path
import unittest

import check_postmerge_scope_topology as topology


class PostmergeScopeTopologyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        repo = Path(__file__).resolve().parents[2]
        cls.sources = tuple((repo / path).read_text(encoding="utf-8") for path in topology.SOURCES)

    def test_current_postmerge_scope_topology_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_cross_scope_contract_uses_selected_families(self) -> None:
        contract = "\n".join(self.sources[:6])
        self.assertNotIn("retained_families", contract)
        self.assertIn("selected_families", contract)

    def test_direct_dispatch_can_select_validation_scope(self) -> None:
        rust_on = topology._block(self.sources[0], r"^on:\s*$", r"^jobs:\s*$")
        self.assertIn("workflow_dispatch:\n    inputs:\n      validation_scope:", rust_on)
        self.assertIn("type: choice", rust_on)
        self.assertIn("options:\n          - merge-gate\n          - extended\n          - full", rust_on)

    def test_every_inventory_family_and_cardinality_is_executable(self) -> None:
        inventory = json.loads(self.sources[7])
        for group in ("rustCiFull", "v8"):
            for index, row in enumerate(inventory[group]):
                for mutation in ("remove", "cardinality"):
                    changed = copy.deepcopy(inventory)
                    if mutation == "remove":
                        changed[group].pop(index)
                    else:
                        changed[group][index].update(cardinality=row["cardinality"] + 1)
                    sources = list(self.sources)
                    sources[7] = json.dumps(changed)
                    with self.subTest(family=row["id"], mutation=mutation):
                        self.assertIn("inventory executable binding", "\n".join(topology.validate_topology(*sources)))

    def test_postmerge_scope_mutations_fail_closed(self) -> None:
        rust, postmerge, blocking, repo_checks, planner, result, detector, inventory, platform, v8 = self.sources
        v8_job = "\n  v8-canary:\n    uses: ./.github/workflows/v8-canary.yml\n"
        cases = (
            ("blocking trigger ownership", 2, blocking.replace("  workflow_dispatch:", "  push:\n    branches: [main]\n  workflow_dispatch:")),
            ("planner workflow contract", 0, rust.replace("run_arm64: ${{ steps.scope.outputs.run_arm64 }}", "run_arm64: false")),
            ("direct dispatch scope contract", 0, rust.replace("          - merge-gate", "          - essential", 1)),
            ("direct dispatch scope contract", 0, rust.replace("        type: choice", "        type: string", 1)),
            ("exact merge lint plan", 4, planner.replace('LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "release")', 'LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "dev")', 1)),
            ("exact Extended lint plan", 4, planner.replace('LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "dev")', 'LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "release")', 1)),
            ("exact full lint plan", 4, planner.replace("MERGE_GATE_LINT_MATRIX[1]", "MERGE_GATE_LINT_MATRIX[0]", 1)),
            ("scoped general scheduling", 0, rust.replace("needs.plan.outputs.run_general == 'true'", "needs.plan.outputs.run_general != 'false'", 1)),
            ("scoped lint scheduling", 0, rust.replace("fromJSON(needs.plan.outputs.lint_matrix)", "fromJSON(inputs.validation_scope)")),
            ("scoped test scheduling", 0, rust.replace("needs.plan.outputs.run_x64 == 'true'", "needs.plan.outputs.run_arm64 == 'true'")),
            ("merge-gate Cargo preserved", 0, rust.replace("postgres_contracts: true", "postgres_contracts: false")),
            ("inventory executable binding", 0, rust.replace("run: cargo shear --deny-warnings", "run: true")),
            ("inventory executable binding", 0, rust.replace("run: cargo test\n", "run: cargo check\n", 1)),
            ("inventory executable binding", 0, rust.replace("uses: ./.github/actions/run-argument-comment-lint", "uses: ./.github/actions/setup-ci")),
            ("inventory executable binding", 8, platform.replace("shard: [1, 2, 3, 4]", "shard: [1, 2, 3]")),
            ("inventory executable binding", 0, rust.replace("uses: ./.github/workflows/rust-ci-full-nextest-platform.yml", "uses: ./missing-nextest.yml", 1)),
            ("inventory executable binding", 0, rust.replace("      use_sccache: true\n      validation_candidate: ${{ inputs.validation_candidate }}\n    secrets: inherit\n\n  # --- Gatherer", "      use_sccache: true\n      validation_candidate: ${{ inputs.validation_candidate }}\n      postgres_contracts: true\n    secrets: inherit\n\n  # --- Gatherer")),
            ("full result fail closed", 0, rust.replace("PLAN_RESULT: ${{ needs.plan.result }}", "PLAN_RESULT: success")),
            ("result helper exact states", 5, result.replace("actual != wanted", "actual == wanted")),
            ("postmerge only Extended Rust", 1, postmerge.replace("validation_scope: extended", "validation_scope: full")),
            ("postmerge only Extended Rust", 1, postmerge.replace("\n  results:\n", v8_job + "\n  results:\n")),
            ("V8 postmerge ownership removed", 6, detector.replace('".github/workflows/repo-checks.yml",', '".github/workflows/postmerge-ci.yml",\n    ".github/workflows/repo-checks.yml",')),
            ("validation inventory complete", 7, inventory.replace('"lint-arm64-musl-release", "disposition": "retained"', '"lint-arm64-musl-release", "disposition": "promoted"')),
            ("validation inventory complete", 7, inventory.replace("v8-arm64-musl-ptrcomp-sandbox", "v8-x64-musl-ptrcomp-sandbox")),
            ("postmerge repository check", 3, repo_checks.replace("check_postmerge_scope_topology.py", "missing_postmerge_topology.py")),
            ("planner fallback fail safe", 4, planner.replace("defaults fail-safe to full", "defaults to extended")),
        )
        for expected, index, changed in cases:
            mutated = list(self.sources)
            mutated[index] = changed
            with self.subTest(expected=expected):
                self.assertIn(expected, "\n".join(topology.validate_topology(*mutated)))


if __name__ == "__main__":
    unittest.main()
