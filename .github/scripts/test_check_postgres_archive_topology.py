from pathlib import Path
import unittest

import check_postgres_archive_topology as topology


def replace_last(source: str, old: str, new: str) -> str:
    before, found, after = source.rpartition(old)
    return before + new + after if found else source


def replace_in_step(
    source: str, job_name: str, step_name: str, old: str, new: str
) -> str:
    job = topology._job(source, job_name)
    step = topology._step(job, step_name)
    changed = step.replace(old, new, 1)
    if changed == step:
        raise AssertionError(f"{old!r} not found in {step_name} step")
    return source.replace(job, job.replace(step, changed, 1), 1)


def replace_in_nextest_installer(
    source: str, job_name: str, old: str, new: str
) -> str:
    job = topology._job(source, job_name)
    step = topology._nextest_installers(job)[0]
    changed = step.replace(old, new, 1)
    if changed == step:
        raise AssertionError(f"{old!r} not found in {job_name} Nextest installer")
    return source.replace(job, job.replace(step, changed, 1), 1)


def append_installer(
    source: str, job_name: str, installer: str, *, step_name: str | None = None
) -> str:
    job = topology._job(source, job_name)
    anchor = (
        topology._step(job, step_name)
        if step_name
        else topology._nextest_installers(job)[0]
    )
    changed = job.replace(anchor, f"{anchor}{installer}", 1)
    return source.replace(job, changed, 1)


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

    def test_quoted_installer_scalars_keep_their_canonical_meaning(self) -> None:
        platform, postgres, *rest = self.sources
        action = topology.NEXTEST_INSTALL_ACTION
        tool = topology.NEXTEST_TOOL
        for job_name, quote in (("archive", '"'), ("shard", "'")):
            platform = replace_in_nextest_installer(
                platform, job_name, action, f"{quote}{action}{quote}"
            )
            platform = replace_in_nextest_installer(
                platform, job_name, tool, f"{quote}{tool}{quote}"
            )
        postgres = replace_in_nextest_installer(
            postgres,
            "postgres-contracts",
            topology.STANDALONE_NEXTEST_CONDITION,
            f'"{topology.STANDALONE_NEXTEST_CONDITION}"',
        )
        postgres = replace_in_step(
            postgres,
            "postgres-contracts",
            "Install pinned nextest for archive consumption",
            topology.ARCHIVE_NEXTEST_CONDITION,
            f'"{topology.ARCHIVE_NEXTEST_CONDITION}"',
        )
        postgres = replace_in_step(
            postgres,
            "postgres-contracts",
            "Install pinned nextest for archive consumption",
            "        with:\n",
            "        with: # archive installer inputs\n",
        )
        postgres = replace_in_step(
            postgres,
            "postgres-contracts",
            "Install pinned nextest for archive consumption",
            action,
            f'"{action}"',
        )
        postgres = replace_in_step(
            postgres,
            "postgres-contracts",
            "Install pinned nextest for archive consumption",
            tool,
            f'"{tool}"',
        )
        self.assertEqual([], topology.validate_topology(platform, postgres, *rest))

    def test_non_action_tool_metadata_is_not_an_installer(self) -> None:
        platform, *rest = self.sources
        platform = replace_in_step(
            platform,
            "shard",
            "Install Linux build dependencies",
            "      - name: Install Linux build dependencies\n",
            "      - name: Install Linux build dependencies\n"
            "        env:\n"
            "          tool: nextest@0.9.104\n"
            "          version: 0.9.104\n",
        )
        self.assertEqual([], topology.validate_topology(platform, *rest))

    def test_mutable_topology_invariants_fail_closed(self) -> None:
        platform, postgres, full, repo_checks, blocking, planner, result_helper = self.sources
        action = topology.NEXTEST_INSTALL_ACTION
        tool = topology.NEXTEST_TOOL
        additional_installer = (
            "      - name: Install additional Nextest\n"
            f"        uses: {action}\n"
            "        with:\n"
            f"          tool: {tool}\n\n"
        )
        additional_archive_installer = additional_installer.replace(
            "        uses:", "        if: ${{ inputs.artifact_id != '' }}\n        uses:"
        )
        unconditional_floating_installer = additional_installer.replace(
            f"tool: {tool}", "tool: nextest"
        )
        reversed_archive_installer = additional_installer.replace(
            "        uses:", "        if: ${{ '' != inputs.artifact_id }}\n        uses:"
        )
        unbraced_archive_installer = additional_installer.replace(
            "        uses:", "        if: inputs.artifact_id != ''\n        uses:"
        )
        if_first_installer = (
            "      - if: ${{ always() }}\n"
            f"        uses: {action}\n"
            "        with:\n"
            "          tool: nextest\n\n"
        )
        if_first_archive_installer = if_first_installer.replace(
            "${{ always() }}", topology.ARCHIVE_NEXTEST_CONDITION
        )
        quoted_alternate_installer = additional_installer.replace(
            f"tool: {tool}", 'tool: "nextest@0.9.104"'
        )
        quoted_archive_installer = additional_archive_installer.replace(
            f"tool: {tool}", "tool: 'nextest@0.9.104'"
        )
        commented_archive_installer = additional_archive_installer.replace(
            "        with:\n", "        with: # alternate archive inputs\n"
        )
        cases = (
            (
                "archive producer rusty_v8 override",
                0,
                replace_in_step(
                    platform,
                    "archive",
                    "Configure rusty_v8 artifact overrides and verify checksums",
                    topology.RUSTY_V8_SETUP_ACTION,
                    "./missing-rusty-v8-setup",
                ),
            ),
            (
                "archive producer rusty_v8 override",
                0,
                replace_in_step(
                    platform,
                    "archive",
                    "Configure rusty_v8 artifact overrides and verify checksums",
                    "target: ${{ inputs.target }}",
                    "target: x86_64-unknown-linux-gnu",
                ),
            ),
            (
                "archive producer nextest pin",
                0,
                replace_in_nextest_installer(
                    platform,
                    "archive",
                    action,
                    f"taiki-e/install-action@wrong # uses: {action}",
                ),
            ),
            (
                "ordinary shard nextest pin",
                0,
                replace_in_nextest_installer(
                    platform,
                    "shard",
                    action,
                    f"taiki-e/install-action@wrong # uses: {action}",
                ),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                replace_in_step(
                    postgres,
                    "postgres-contracts",
                    "Install pinned nextest for archive consumption",
                    action,
                    f"taiki-e/install-action@wrong # uses: {action}",
                ),
            ),
            (
                "archive producer nextest pin",
                0,
                replace_in_nextest_installer(
                    platform,
                    "archive",
                    tool,
                    f"nextest@0.9.104 #          tool: {tool}",
                ),
            ),
            (
                "ordinary shard nextest pin",
                0,
                replace_in_nextest_installer(
                    platform,
                    "shard",
                    tool,
                    f"nextest@0.9.104 #          tool: {tool}",
                ),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                replace_in_step(
                    postgres,
                    "postgres-contracts",
                    "Install pinned nextest for archive consumption",
                    tool,
                    f"nextest@0.9.104 #          tool: {tool}",
                ),
            ),
            (
                "archive producer nextest pin",
                0,
                append_installer(platform, "archive", additional_installer),
            ),
            (
                "ordinary shard nextest pin",
                0,
                append_installer(platform, "shard", additional_installer),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                append_installer(
                    postgres,
                    "postgres-contracts",
                    additional_archive_installer,
                    step_name="Install pinned nextest for archive consumption",
                ),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                append_installer(
                    postgres,
                    "postgres-contracts",
                    unconditional_floating_installer,
                    step_name="Install pinned nextest for archive consumption",
                ),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                append_installer(
                    postgres,
                    "postgres-contracts",
                    reversed_archive_installer,
                    step_name="Install pinned nextest for archive consumption",
                ),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                append_installer(
                    postgres,
                    "postgres-contracts",
                    unbraced_archive_installer,
                    step_name="Install pinned nextest for archive consumption",
                ),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                replace_in_nextest_installer(
                    postgres,
                    "postgres-contracts",
                    topology.STANDALONE_NEXTEST_CONDITION,
                    "${{ true }}",
                ),
            ),
            (
                "archive producer nextest pin",
                0,
                append_installer(platform, "archive", if_first_installer),
            ),
            (
                "ordinary shard nextest pin",
                0,
                append_installer(platform, "shard", quoted_alternate_installer),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                append_installer(
                    postgres,
                    "postgres-contracts",
                    if_first_archive_installer,
                    step_name="Install pinned nextest for archive consumption",
                ),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                append_installer(
                    postgres,
                    "postgres-contracts",
                    quoted_archive_installer,
                    step_name="Install pinned nextest for archive consumption",
                ),
            ),
            (
                "PostgreSQL archive consumer nextest pin",
                1,
                append_installer(
                    postgres,
                    "postgres-contracts",
                    commented_archive_installer,
                    step_name="Install pinned nextest for archive consumption",
                ),
            ),
            ("archive producer nextest pin", 0, platform.replace("tool: nextest@0.9.103", "tool: nextest", 1)),
            ("ordinary shard nextest pin", 0, replace_last(platform, "tool: nextest@0.9.103", "tool: nextest")),
            ("PostgreSQL archive consumer nextest pin", 1, postgres.replace("tool: nextest@0.9.103", "tool: nextest\n          version: 0.9.103")),
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
            ("ordinary shard JUnit path", 0, platform.replace("codex-rs/target/nextest/default/junit.xml", "codex-rs/wrong/nextest/default/junit.xml", 1)),
            ("ordinary shard JUnit path", 0, platform.replace('rm -f -- "${NEXTEST_JUNIT_FILE}"', 'rm -f -- "${CARGO_TARGET_DIR}/nextest/default/junit.xml"', 1)),
            ("ordinary shard JUnit path", 0, replace_in_step(platform, "shard", "Inspect nextest JUnit signal", '"${NEXTEST_JUNIT_FILE}"', '"${CARGO_TARGET_DIR}/nextest/default/junit.xml"')),
            ("ordinary shard JUnit path", 0, platform.replace("path: ${{ env.NEXTEST_JUNIT_FILE }}", "path: ${{ env.CARGO_TARGET_DIR }}/nextest/default/junit.xml", 1)),
            ("ordinary shard JUnit path", 0, replace_in_step(platform, "shard", "Inspect nextest JUnit signal", "--allow-retries", "--reject-skipped")),
            ("PostgreSQL JUnit path", 1, postgres.replace("codex-rs/target/nextest/default/junit.xml", "codex-rs/wrong/nextest/default/junit.xml", 1)),
            ("PostgreSQL JUnit path", 1, postgres.replace('rm -f -- "${NEXTEST_JUNIT_FILE}"', 'rm -f -- "${CARGO_TARGET_DIR}/nextest/default/junit.xml"', 1)),
            ("PostgreSQL JUnit path", 1, postgres.replace('junit="${NEXTEST_JUNIT_FILE}"', 'junit="${CARGO_TARGET_DIR}/nextest/default/junit.xml"', 1)),
            ("PostgreSQL JUnit path", 1, postgres.replace('rm -f -- "${junit}"', 'rm -f -- "${NEXTEST_JUNIT_FILE}"', 1)),
            ("PostgreSQL JUnit path", 1, postgres.replace('            "${NEXTEST_JUNIT_FILE}" \\\n            --expected-testcases', '            "${CARGO_TARGET_DIR}/nextest/default/junit.xml" \\\n            --expected-testcases', 1)),
            ("PostgreSQL JUnit path", 1, postgres.replace("path: ${{ env.NEXTEST_JUNIT_FILE }}", "path: ${{ env.CARGO_TARGET_DIR }}/nextest/default/junit.xml", 1)),
            ("PostgreSQL JUnit path", 1, replace_in_step(postgres, "postgres-contracts", "Inspect archive nextest JUnit signal", "--allow-retries", "--reject-skipped")),
            ("PostgreSQL inventory root", 1, postgres.replace('            --repo "${GITHUB_WORKSPACE}" \\\n', "", 1)),
            ("PostgreSQL inventory root", 1, postgres.replace('--repo "${GITHUB_WORKSPACE}"', '--repo "${GITHUB_WORKSPACE}/codex-rs"', 1)),
            ("inventory-gated JUnit inspection", 1, postgres.replace("if: ${{ always() && inputs.artifact_id != '' && steps.inventory.outcome == 'success' }}", "if: ${{ always() && inputs.artifact_id != '' }}", 1)),
            ("inventory-gated JUnit inspection", 1, postgres.replace('if [[ "${JUNIT_OUTCOME}" != "success" ]]; then', 'if [[ "${JUNIT_OUTCOME}" != "success" && "${JUNIT_OUTCOME}" != "skipped" ]]; then', 1)),
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
