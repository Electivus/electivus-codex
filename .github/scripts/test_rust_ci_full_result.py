from dataclasses import replace
import unittest

from rust_ci_full_result import ChildResults
from rust_ci_full_result import RustCiFullDecision
from rust_ci_full_result import evaluate_results
from rust_ci_full_result import render_result_summary


MERGE = ChildResults("skipped", "skipped", "skipped", "skipped", "success", "success", "skipped")
EXTENDED = ChildResults("skipped", "skipped", "skipped", "skipped", "success", "skipped", "success")
FULL = ChildResults(*("success",) * 7)


class RustCiFullResultTests(unittest.TestCase):
    def test_each_scope_requires_exact_success_and_skipped_children(self) -> None:
        for scope, results in (("merge-gate", MERGE), ("extended", EXTENDED), ("full", FULL)):
            with self.subTest(scope=scope):
                self.assertEqual(RustCiFullDecision(True, ()), evaluate_results(scope, "success", results))

    def test_unexpected_scheduling_fails_closed(self) -> None:
        self.assertEqual(
            RustCiFullDecision(False, ("tests_linux_x64 expected skipped, got success",)),
            evaluate_results("extended", "success", replace(EXTENDED, tests_linux_x64="success")),
        )

    def test_plan_and_every_non_success_child_state_fail_closed(self) -> None:
        plan_failure = evaluate_results("extended", "failure", ChildResults(*("skipped",) * 7))
        self.assertEqual("plan expected success, got failure", plan_failure.issues[0])
        for state in ("skipped", "failure", "cancelled", "incomplete"):
            with self.subTest(state=state):
                self.assertEqual(
                    RustCiFullDecision(False, (f"lint_build expected success, got {state}",)),
                    evaluate_results("extended", "success", replace(EXTENDED, lint_build=state)),
                )

    def test_summary_declares_scope_families_and_exact_states(self) -> None:
        summary = render_result_summary("extended", EXTENDED, RustCiFullDecision(True, ()))
        for fragment in ("Resolved scope: `extended`", "Retained families: x64 musl dev lint/build;", "| `tests_linux_x64` | `skipped` | `skipped` |", "| `tests_linux_arm64` | `success` | `success` |", "Outcome: `success`"):
            self.assertIn(fragment, summary)


if __name__ == "__main__":
    unittest.main()
