import unittest

from rust_ci_full_plan import LintLane, RustCiFullPlan, github_outputs, plan_for_scope, render_summary


MERGE = (LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "dev"), LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "release"), LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "release"))
EXTENDED = (LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "dev"), LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-musl", "dev"), LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "dev"), LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-musl", "release"))
FULL = (EXTENDED[0], MERGE[0], EXTENDED[1], EXTENDED[2], MERGE[2], EXTENDED[3], MERGE[1])
WINDOWS = (
    LintLane("windows-2025", "x86_64-pc-windows-msvc", "dev"),
    LintLane("windows-2025", "x86_64-pc-windows-msvc", "release"),
    LintLane("windows-11-arm", "aarch64-pc-windows-msvc", "dev"),
    LintLane("windows-11-arm", "aarch64-pc-windows-msvc", "release"),
)
FULL = (*FULL, *WINDOWS)
MERGE_FAMILIES = ("x64 GNU dev lint/build", "x64 GNU release lint/build", "x64 musl release lint/build", "x64 nextest 4+1 PostgreSQL")
EXTENDED_FAMILIES = ("x64 musl dev lint/build", "ARM64 musl dev lint/build", "ARM64 GNU dev lint/build", "ARM64 musl release lint/build", "ARM64 nextest")
WINDOWS_FAMILIES = ("Windows argument comment lint", "Windows x64 dev/release lint/build", "Windows ARM64 dev/release lint/build", "Windows x64 nextest", "Windows ARM64 nextest")
FULL_FAMILIES = ("general formatting and benchmark", "cargo shear", "argument comment lint package", "argument comment lint prebuilt", "all eleven lint/build lanes", "x64 nextest 4+1 PostgreSQL", "ARM64 nextest", *WINDOWS_FAMILIES)


def full_plan(requested: str, reason: str) -> RustCiFullPlan:
    return RustCiFullPlan(requested, "full", reason, FULL, True, True, True, True, True, FULL_FAMILIES)


class RustCiFullPlanTests(unittest.TestCase):
    def test_known_scopes_have_exact_deep_equal_plans(self) -> None:
        cases = (
            ("merge-gate", RustCiFullPlan("merge-gate", "merge-gate", "requested merge-gate scope", MERGE, False, True, False, False, False, MERGE_FAMILIES)),
            ("extended", RustCiFullPlan("extended", "extended", "requested extended scope", EXTENDED, False, False, True, False, False, EXTENDED_FAMILIES)),
            ("windows", RustCiFullPlan("windows", "windows", "requested windows scope", WINDOWS, False, False, False, True, True, WINDOWS_FAMILIES)),
            ("full", full_plan("full", "requested full scope")),
        )
        for scope, expected in cases:
            with self.subTest(scope=scope):
                self.assertEqual(expected, plan_for_scope(scope))

    def test_empty_and_unknown_scopes_fail_safe_to_full(self) -> None:
        cases = (("", full_plan("", "empty scope defaults to full")), ("nightly", full_plan("nightly", "unknown scope 'nightly' defaults fail-safe to full")))
        for scope, expected in cases:
            with self.subTest(scope=scope):
                self.assertEqual(expected, plan_for_scope(scope))

    def test_requested_scope_is_sanitized_and_outputs_are_bounded(self) -> None:
        plan = plan_for_scope("nightly\nresolved_scope=extended" + "x" * 5000)
        self.assertEqual(64, len(plan.requested_scope))
        self.assertTrue(plan.requested_scope.startswith("nightly?resolved_scope?extended"))
        self.assertTrue(all("\n" not in value and len(value) <= 4096 for value in github_outputs(plan).values()))

    def test_extended_github_outputs_and_summary_are_bounded(self) -> None:
        plan = plan_for_scope("extended")
        self.assertEqual({"resolved_scope": "extended", "reason": "requested extended scope", "lint_matrix": '[{"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-musl","profile":"dev"},{"runner":"ubuntu-24.04-arm","target":"aarch64-unknown-linux-musl","profile":"dev"},{"runner":"ubuntu-24.04-arm","target":"aarch64-unknown-linux-gnu","profile":"dev"},{"runner":"ubuntu-24.04-arm","target":"aarch64-unknown-linux-musl","profile":"release"}]', "run_general": "false", "run_linux_x64": "false", "run_linux_arm64": "true", "run_windows_x64": "false", "run_windows_arm64": "false", "selected_families": '["x64 musl dev lint/build","ARM64 musl dev lint/build","ARM64 GNU dev lint/build","ARM64 musl release lint/build","ARM64 nextest"]'}, github_outputs(plan))
        summary = render_summary(plan)
        for fragment in ("Resolved scope: `extended`", "General families: `false`", "Linux x64 nextest 4+1 PostgreSQL: `false`", "Linux ARM64 nextest: `true`", "Windows x64 nextest: `false`", "Windows ARM64 nextest: `false`", "Lint/build lanes: `4`"):
            self.assertIn(fragment, summary)

    def test_windows_and_full_outputs_select_windows_positive_paths(self) -> None:
        windows = plan_for_scope("windows")
        self.assertEqual(
            {
                "resolved_scope": "windows",
                "reason": "requested windows scope",
                "lint_matrix": '[{"runner":"windows-2025","target":"x86_64-pc-windows-msvc","profile":"dev"},{"runner":"windows-2025","target":"x86_64-pc-windows-msvc","profile":"release"},{"runner":"windows-11-arm","target":"aarch64-pc-windows-msvc","profile":"dev"},{"runner":"windows-11-arm","target":"aarch64-pc-windows-msvc","profile":"release"}]',
                "run_general": "false",
                "run_linux_x64": "false",
                "run_linux_arm64": "false",
                "run_windows_x64": "true",
                "run_windows_arm64": "true",
                "selected_families": '["Windows argument comment lint","Windows x64 dev/release lint/build","Windows ARM64 dev/release lint/build","Windows x64 nextest","Windows ARM64 nextest"]',
            },
            github_outputs(windows),
        )
        self.assertIn("Windows x64 nextest: `true`", render_summary(windows))
        self.assertIn("Windows ARM64 nextest: `true`", render_summary(windows))

        full_outputs = github_outputs(plan_for_scope("full"))
        self.assertEqual("true", full_outputs["run_windows_x64"])
        self.assertEqual("true", full_outputs["run_windows_arm64"])
        self.assertEqual(11, full_outputs["lint_matrix"].count('"runner"'))


if __name__ == "__main__":
    unittest.main()
