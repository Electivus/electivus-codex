from pathlib import Path
import unittest

import check_v8_canary_topology as topology


class V8CanaryTopologyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo = Path(__file__).resolve().parents[2]
        cls.sources = tuple(
            (cls.repo / path).read_text(encoding="utf-8")
            for path in topology.SOURCES
        )

    def test_current_v8_canary_topology_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_v8_canary_mutations_fail_closed(self) -> None:
        canary, blocking, repo_checks, detector = self.sources
        cases = (
            ("exact Linux matrix", 0, canary.replace("variant: ptrcomp-sandbox", "variant: release", 1)),
            ("exact Linux matrix", 0, canary.replace("runner: ubuntu-24.04-arm", "runner: macos-15", 1)),
            ("metadata fail safe", 0, canary.replace("canary_required=true", "canary_required=false", 1)),
            ("metadata fail safe", 0, canary.replace('canary_required="${detector_lines[0]#canary_required=}"', 'canary_required="${BASH_REMATCH[1]}"')),
            ("metadata fail safe", 0, canary.replace("classifier returned malformed output", "classifier output ignored")),
            ("version fallback", 0, canary.replace("unknown-${GITHUB_SHA:0:12}", "unknown")),
            ("conditional build", 0, canary.replace("needs.metadata.outputs.canary_required == 'true'", "needs.metadata.outputs.canary_required != 'false'")),
            ("bounded result", 0, canary.replace("needs: [metadata, build]", "needs: build")),
            ("bounded result", 0, canary.replace("if: ${{ always() }}", "if: ${{ needs.build.result == 'success' }}", 1)),
            ("artifact and smoke integrity", 0, canary.replace("v8-canary-${{ needs.metadata.outputs.v8_version }}-${{ matrix.variant }}-${{ matrix.target }}", "v8-canary-shared")),
            ("artifact and smoke integrity", 0, canary.replace("run_bazel_with_buildbuddy.py", "bazel")),
            ("artifact and smoke integrity", 0, canary.replace("x86_64-unknown-linux-gnu:x86_64", "x86_64-unknown-linux-musl:x86_64")),
            ("red matrix", 0, canary.replace("name: Build Bazel V8 release pair", "continue-on-error: true\n      - name: Build Bazel V8 release pair")),
            ("V8 caller required", 1, blocking.replace("uses: ./.github/workflows/v8-canary.yml", "uses: ./missing-v8.yml")),
            ("V8 caller required", 1, blocking.replace("- v8-canary", "- missing-v8")),
            ("detector self relevance", 3, detector.replace('".github/workflows/blocking-ci.yml"', '"missing-blocking.yml"')),
            ("detector unknown fail safe", 3, detector.replace("unknown V8 impact", "unknown path ignored")),
            ("V8 repository check", 2, repo_checks.replace("check_v8_canary_topology.py", "missing_v8_topology.py")),
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
