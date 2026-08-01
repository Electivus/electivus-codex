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
            for path in topology.WORKFLOWS
        )

    def test_current_release_topology_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_release_topology_mutations_fail_closed(self) -> None:
        bazel, blocking, rust, repo_checks = self.sources
        cases = (
            ("Bazel concurrency scope", 0, bazel.replace("::${{ inputs.validation_scope || 'essential' }}", "::shared-scope")),
            ("Bazel scope fails safe", 0, bazel.replace("default: essential", "default: release-only", 1)),
            ("Bazel essential scheduling", 0, bazel.replace("inputs.validation_scope != 'release-only'", "inputs.validation_scope == 'essential'", 1)),
            ("Bazel release scheduling", 0, bazel.replace("inputs.validation_scope != 'essential' && inputs.validation_scope != ''", "inputs.validation_scope == 'release-only'", 1)),
            ("Bazel release target", 0, replace_last(bazel, "target: x86_64-unknown-linux-gnu", "target: aarch64-unknown-linux-gnu")),
            ("Bazel release assertions", 0, bazel.replace("-Cdebug-assertions=no", "-Cdebug-assertions=yes", 1)),
            ("Bazel release targets", 0, bazel.replace("list-bazel-release-targets.sh", "list-bazel-clippy-targets.sh")),
            ("Bazel release bwrap", 0, bazel.replace("//codex-rs/bwrap:bwrap", "//codex-rs/cli:codex")),
            ("Bazel release logs", 0, bazel.replace("bazel-execution-logs-verify-release-build", "missing-release-logs")),
            ("Bazel release promotion", 1, blocking.replace("needs.deep-linux-eligibility.outputs.eligible == 'true'", "needs.deep-linux-eligibility.outputs.eligible == 'false'", 1)),
            ("Bazel release promotion", 1, blocking.replace("validation_scope: release-only", "validation_scope: essential")),
            ("bounded Bazel result", 1, blocking.replace("if: ${{ always() }}", "if: ${{ needs.deep-linux-bazel-release.result == 'success' }}", 1)),
            ("bounded Bazel result", 1, blocking.replace("VALIDATION_LABEL: Deep Linux Bazel release", "VALIDATION_LABEL: ''")),
            ("independent required results", 1, blocking.replace("- deep-linux-bazel-release-result", "- deep-linux-cargo")),
            ("merge Cargo matrix", 2, rust.replace('"target":"x86_64-unknown-linux-musl","profile":"release"', '"target":"x86_64-unknown-linux-musl","profile":"dev"', 1)),
            ("full Cargo matrix", 2, rust.replace('"target":"x86_64-unknown-linux-gnu","profile":"release"}]', '"target":"x86_64-unknown-linux-gnu","profile":"dev"}]')),
            ("Cargo release build and lint", 2, rust.replace("cargo build --workspace", "cargo build -p codex-core")),
            ("Cargo release build and lint", 2, rust.replace("cargo clippy --workspace --target ${{ matrix.target }} --tests --profile release", "cargo clippy -p codex-core --profile release")),
            ("Cargo release timings", 2, rust.replace("cargo-timings-rust-ci-build", "missing-build-timings")),
            ("scope-aware Cargo aggregate", 2, rust.replace("needs.lint_build.result }}' == 'success'", "needs.lint_build.result }}' == 'failure'")),
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
