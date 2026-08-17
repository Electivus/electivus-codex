from pathlib import Path
import os
import shutil
import subprocess
import unittest

import check_windows_ci_topology as topology


class WindowsCiTopologyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        repo = Path(__file__).resolve().parents[2]
        cls.sources = tuple(
            (repo / path).read_text(encoding="utf-8") for path in topology.SOURCES
        )

    def test_current_windows_topology_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_windows_bazel_shard_shell_is_valid(self) -> None:
        bash = shutil.which("bash")
        if os.name == "nt" and (git := shutil.which("git")) is not None:
            git_bash = Path(git).parent.parent / "usr/bin/bash.exe"
            if git_bash.is_file():
                bash = str(git_bash)
        if bash is None:
            self.skipTest("bash is required to validate the Windows Bazel shard script")
        step = topology._step(
            topology._job(self.sources[0], "test-windows-shard"), "Bazel test shard"
        )
        body = step.split("        run: |\n", 1)[1]
        script = "\n".join(line[10:] for line in body.splitlines())
        result = subprocess.run(
            [bash, "-n", "-c", script], check=False, capture_output=True, text=True
        )
        self.assertEqual(0, result.returncode, result.stderr)

    def test_windows_topology_mutations_fail_closed(self) -> None:
        (
            bazel,
            blocking,
            rust,
            platform,
            v8,
            postmerge,
            repo_checks,
            planner,
            rust_result,
            v8_result,
            bazel_helper,
            skip_policy,
            inventory,
            baseline,
            bazelrc,
            fast_rust,
        ) = self.sources
        cases = (
            ("exact Windows Cargo plan", 7, planner.replace("windows-11-arm", "windows-2025", 1)),
            ("explicit Windows planning outputs", 2, rust.replace("run_windows_arm64", "run_arm64")),
            ("public Windows runners", 2, rust.replace("runner: windows-11-arm", "runner: windows-2025")),
            ("public Windows runners", 0, bazel.replace("runs-on: windows-2025", "runs-on: windows-2025-private", 1)),
            ("public Windows runners", 2, rust.replace("runner: windows-11-arm", "runner: windows-11-arm-private")),
            ("Windows argument comment lint ownership", 2, rust.replace("  argument_comment_lint_windows:", "  missing_argument_comment_lint_windows:")),
            ("Windows argument comment lint ownership", 15, fast_rust + "\n# uses: ./.github/actions/run-argument-comment-lint\n"),
            ("Windows nextest producer and consumers", 3, platform.replace("shard: [1, 2, 3, 4]", "shard: [1, 2, 3]")),
            ("retry-free JUnit evidence", 3, platform.replace("check_nextest_junit.py", "missing_junit_check.py")),
            ("Windows Cargo result fan-in", 8, rust_result.replace("tests_windows_arm64", "tests_windows_x64")),
            ("exact Windows Bazel topology", 0, bazel.replace("matrix:\n        shard: [1, 2, 3, 4]", "matrix:\n        shard: [1, 2, 3]")),
            ("exact Windows Bazel topology", 0, bazel.replace("fail-fast: false", "fail-fast: true")),
            ("exact Windows Bazel topology", 0, bazel.replace("windows_bazel_shards.py", "missing_shards.py")),
            ("Windows Bazel dispatch contract", 0, bazel.replace("          - windows\n", "", 1)),
            ("Windows Bazel fail-closed results", 0, bazel.replace("needs: [test-windows, clippy-windows, verify-release-build-windows]", "needs: test-windows")),
            ("optional BuildBuddy local fallback", 10, bazel_helper.replace("--jobs=4", "--jobs=8")),
            ("optional BuildBuddy local fallback", 10, bazel_helper.replace('bazel_run_args+=("--config=${ci_config}")', 'echo "missing local config"')),
            ("optional BuildBuddy local fallback", 10, bazel_helper.replace('if [[ -n "${BUILDBUDDY_API_KEY:-}" || "${RUNNER_OS:-}" == "Windows" ]]; then', 'if [[ -n "${BUILDBUDDY_API_KEY:-}" || "${RUNNER_OS:-}" == "Linux" ]]; then')),
            ("optional BuildBuddy local fallback", 14, bazelrc.replace("--local_test_jobs=4", "--local_test_jobs=8")),
            ("mandatory Windows V8 parity", 4, v8.replace("- aarch64-pc-windows-msvc", "- x86_64-pc-windows-msvc", 1)),
            ("CI required Windows fan-in", 1, blocking.replace("- windows-cargo", "- deep-linux-cargo")),
            ("CI required Windows fan-in", 1, blocking.replace("      - windows-cargo", "      # - windows-cargo")),
            ("CI required Windows fan-in", 1, blocking.replace("      - windows-bazel\n", "")),
            ("CI required Windows fan-in", 1, blocking.replace("      - v8-canary\n", "")),
            ("Windows inventory binding", 12, inventory.replace('"outOfBoundary": ["macOS"]', '"outOfBoundary": ["macOS", "Windows"]')),
            ("fixed Windows skip baseline", 13, baseline.replace(topology.WINDOWS_SKIP_BASELINE_COMMIT, "0" * 40)),
            ("no new Windows test filters", 14, bazelrc + "\ncommon:ci-windows --test_env=CODEX_BAZEL_TEST_SKIP_FILTERS=new_skip\n"),
            ("required repository topology check", 6, repo_checks.replace("check_windows_ci_topology.py", "missing_windows_topology.py")),
            ("no postmerge Windows duplication", 5, postmerge + "\n# windows-2025\n"),
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
