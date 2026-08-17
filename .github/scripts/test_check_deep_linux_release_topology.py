from pathlib import Path
import unittest

import check_deep_linux_release_topology as topology


def replace_last(source: str, old: str, new: str) -> str:
    before, found, after = source.rpartition(old)
    return before + new + after if found else source


class DeepLinuxReleaseTopologyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo = Path(__file__).resolve().parents[2]
        cls.sources = tuple(
            (cls.repo / path).read_text(encoding="utf-8")
            for path in topology.SOURCES
        )

    def test_current_release_topology_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_release_topology_mutations_fail_closed(self) -> None:
        bazel, blocking, rust, repo_checks, planner, result_helper = self.sources
        pinned_actionlint = (
            "go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.7"
        )
        floating_actionlint = (
            "go install github.com/rhysd/actionlint/cmd/actionlint@latest"
        )
        overwrite_actionlint = (
            'curl -fsSL https://example.invalid/actionlint -o "$GOBIN/actionlint"'
        )

        def append_install_command(source: str) -> str:
            return source.replace(
                pinned_actionlint,
                f"{pinned_actionlint}\n          {overwrite_actionlint}",
                1,
            )

        def insert_override_step(source: str, consumer: str) -> str:
            marker = f"\n      - name: {consumer}\n"
            override = (
                "\n      - name: Override actionlint\n"
                "        run: curl -fsSL https://example.invalid/actionlint "
                "-o ${{ runner.temp }}/actionlint/bin/actionlint\n"
            )
            return source.replace(marker, override + marker, 1)

        cases = (
            ("Bazel concurrency scope", 0, bazel.replace("github.event.pull_request.number > 0", "github.actor != ''")),
            ("Bazel concurrency scope", 0, bazel.replace("format('pr-{0}', github.event.pull_request.number)", "format('pr-{0}', github.actor)")),
            ("Bazel concurrency scope", 0, bazel.replace("|| github.ref_name }}::", "|| github.actor }}::")),
            ("Bazel concurrency scope", 0, bazel.replace("::${{ inputs.validation_scope || 'essential' }}", "::shared-scope")),
            ("Bazel scope fails safe", 0, bazel.replace("default: essential", "default: release-only", 1)),
            ("Bazel essential scheduling", 0, bazel.replace("inputs.validation_scope != 'release-only' && inputs.validation_scope != 'windows'", "inputs.validation_scope == 'essential'", 1)),
            ("Bazel release scheduling", 0, bazel.replace("inputs.validation_scope != 'essential' && inputs.validation_scope != 'windows' && inputs.validation_scope != ''", "inputs.validation_scope == 'release-only'", 1)),
            ("Bazel release target", 0, replace_last(bazel, "target: x86_64-unknown-linux-gnu", "target: aarch64-unknown-linux-gnu")),
            ("Bazel release assertions", 0, replace_last(bazel, "-Cdebug-assertions=no", "-Cdebug-assertions=yes")),
            ("Bazel release targets", 0, bazel.replace("list-bazel-release-targets.sh", "list-bazel-clippy-targets.sh")),
            ("Bazel release bwrap", 0, bazel.replace("//codex-rs/bwrap:bwrap", "//codex-rs/cli:codex")),
            ("Bazel release logs", 0, bazel.replace("bazel-execution-logs-verify-release-build", "missing-release-logs")),
            ("hosted actionlint install", 0, bazel.replace("github.com/rhysd/actionlint/cmd/actionlint@v1.7.7", "github.com/rhysd/actionlint/cmd/actionlint@latest", 1)),
            ("hosted actionlint install", 3, repo_checks.replace('echo "$GOBIN" >> "$GITHUB_PATH"', 'echo "$GOBIN"')),
            ("hosted actionlint install", 0, bazel.replace(pinned_actionlint, f"{pinned_actionlint}\n          {floating_actionlint}", 1)),
            ("hosted actionlint install", 3, repo_checks.replace(pinned_actionlint, f"{pinned_actionlint}\n          {floating_actionlint}", 1)),
            ("hosted actionlint install", 0, bazel.replace(pinned_actionlint, f"{floating_actionlint}\n          # {pinned_actionlint}", 1)),
            ("hosted actionlint install", 3, repo_checks.replace(pinned_actionlint, f"{floating_actionlint}\n          # {pinned_actionlint}", 1)),
            ("hosted actionlint install", 0, append_install_command(bazel)),
            ("hosted actionlint install", 3, append_install_command(repo_checks)),
            ("actionlint consumer adjacency", 0, insert_override_step(bazel, "Check rusty_v8 MODULE.bazel checksums")),
            ("actionlint consumer adjacency", 3, insert_override_step(repo_checks, "Test GitHub helper scripts")),
            ("Bazel release promotion", 1, blocking.replace("needs.deep-linux-eligibility.outputs.eligible == 'true'", "needs.deep-linux-eligibility.outputs.eligible == 'false'", 1)),
            ("Bazel release promotion", 1, blocking.replace("validation_scope: release-only", "validation_scope: essential")),
            ("bounded Bazel result", 1, blocking.replace("if: ${{ always() }}", "if: ${{ needs.deep-linux-bazel-release.result == 'success' }}", 1)),
            ("bounded Bazel result", 1, blocking.replace("VALIDATION_LABEL: Deep Linux Bazel release", "VALIDATION_LABEL: ''")),
            ("independent required results", 1, blocking.replace("- deep-linux-bazel-release-result", "- deep-linux-cargo")),
            ("merge Cargo matrix", 4, planner.replace('LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "release")', 'LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "dev")', 1)),
            ("full Cargo matrix", 4, planner.replace('LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "release")', 'LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "dev")', 2)),
            ("Cargo release build and lint", 2, rust.replace("cargo build --workspace", "cargo build -p codex-core")),
            ("Cargo release build and lint", 2, rust.replace("cargo clippy --workspace --target ${{ matrix.target }} --tests --profile release", "cargo clippy -p codex-core --profile release")),
            ("Cargo release timings", 2, rust.replace("cargo-timings-rust-ci-build", "missing-build-timings")),
            ("scope-aware Cargo aggregate", 5, result_helper.replace("actual != wanted", "actual == wanted")),
            ("release repository check", 3, repo_checks.replace("check_deep_linux_release_topology.py", "missing_release_topology.py")),
        )
        for expected, index, changed in cases:
            mutated = list(self.sources)
            mutated[index] = changed
            with self.subTest(expected=expected):
                self.assertIn(
                    expected, "\n".join(topology.validate_topology(*mutated))
                )


if __name__ == "__main__":
    unittest.main()
