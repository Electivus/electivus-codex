import unittest

from rust_ci_full_plan import LintLane, RustCiFullPlan, github_outputs, plan_for_scope, render_summary


MERGE = (LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "dev"), LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "release"), LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "release"))
EXTENDED = (LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "dev"), LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-musl", "dev"), LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "dev"), LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-musl", "release"))
FULL = (EXTENDED[0], MERGE[0], EXTENDED[1], EXTENDED[2], MERGE[2], EXTENDED[3], MERGE[1])
MERGE_FAMILIES = ("x64 GNU dev lint/build", "x64 GNU release lint/build", "x64 musl release lint/build", "x64 nextest 4+1 PostgreSQL")
EXTENDED_FAMILIES = ("x64 musl dev lint/build", "ARM64 musl dev lint/build", "ARM64 GNU dev lint/build", "ARM64 musl release lint/build", "ARM64 nextest")
FULL_FAMILIES = ("general formatting and benchmark", "cargo shear", "argument comment lint package", "argument comment lint prebuilt", "all seven lint/build lanes", "x64 nextest 4+1 PostgreSQL", "ARM64 nextest")


def full_plan(requested: str, reason: str) -> RustCiFullPlan:
    return RustCiFullPlan(requested, "full", reason, FULL, True, True, True, FULL_FAMILIES)


class RustCiFullPlanTests(unittest.TestCase):
    def test_known_scopes_have_exact_deep_equal_plans(self) -> None:
        cases = (
            ("merge-gate", RustCiFullPlan("merge-gate", "merge-gate", "requested merge-gate scope", MERGE, False, True, False, MERGE_FAMILIES)),
            ("extended", RustCiFullPlan("extended", "extended", "requested extended scope", EXTENDED, False, False, True, EXTENDED_FAMILIES)),
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
        self.assertEqual({"resolved_scope": "extended", "reason": "requested extended scope", "lint_matrix": '[{"runner":"ubuntu-24.04","target":"x86_64-unknown-linux-musl","profile":"dev"},{"runner":"ubuntu-24.04-arm","target":"aarch64-unknown-linux-musl","profile":"dev"},{"runner":"ubuntu-24.04-arm","target":"aarch64-unknown-linux-gnu","profile":"dev"},{"runner":"ubuntu-24.04-arm","target":"aarch64-unknown-linux-musl","profile":"release"}]', "run_general": "false", "run_x64": "false", "run_arm64": "true", "selected_families": '["x64 musl dev lint/build","ARM64 musl dev lint/build","ARM64 GNU dev lint/build","ARM64 musl release lint/build","ARM64 nextest"]'}, github_outputs(plan))
        summary = render_summary(plan)
        for fragment in ("Resolved scope: `extended`", "General families: `false`", "x64 nextest 4+1 PostgreSQL: `false`", "ARM64 nextest: `true`", "Lint/build lanes: `4`"):
            self.assertIn(fragment, summary)


if __name__ == "__main__":
    unittest.main()
