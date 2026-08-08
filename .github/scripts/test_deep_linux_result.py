from contextlib import redirect_stdout
from io import StringIO
import os
import unittest
from unittest.mock import patch

import deep_linux_result as result


class DeepLinuxResultTests(unittest.TestCase):
    def test_cargo_and_bazel_accept_only_the_two_bounded_states(self) -> None:
        for label in ("Deep Linux Cargo", "Deep Linux Bazel release"):
            with self.subTest(label=label, eligible="true"):
                self.assertEqual(
                    result.Decision(True, f"eligible=true; {label} succeeded"),
                    result.evaluate("success", "true", "success", label),
                )
            with self.subTest(label=label, eligible="false"):
                self.assertEqual(
                    result.Decision(True, f"eligible=false; {label} correctly skipped"),
                    result.evaluate("success", "false", "skipped", label),
                )

    def test_all_other_conclusions_fail_closed_for_each_caller(self) -> None:
        cases = (
            ("failure", "true", "skipped", "eligibility job ended with failure"),
            ("cancelled", "true", "skipped", "eligibility job ended with cancelled"),
            ("skipped", "true", "skipped", "eligibility job ended with skipped"),
            ("unknown", "true", "skipped", "eligibility job ended with unknown"),
            ("", "true", "skipped", "eligibility job ended with missing"),
            ("success", "", "skipped", "eligibility output is malformed"),
            ("success", "TRUE", "success", "eligibility output is malformed"),
            ("success", "unknown", "success", "eligibility output is malformed"),
            ("success", "true", "failure", "eligible=true requires {label} success, found failure"),
            ("success", "true", "cancelled", "eligible=true requires {label} success, found cancelled"),
            ("success", "true", "skipped", "eligible=true requires {label} success, found skipped"),
            ("success", "true", "", "eligible=true requires {label} success, found missing"),
            ("success", "true", "unknown", "eligible=true requires {label} success, found unknown"),
            ("success", "false", "success", "eligible=false requires {label} skipped, found success"),
            ("success", "false", "failure", "eligible=false requires {label} skipped, found failure"),
            ("success", "false", "cancelled", "eligible=false requires {label} skipped, found cancelled"),
            ("success", "false", "", "eligible=false requires {label} skipped, found missing"),
            ("success", "false", "unknown", "eligible=false requires {label} skipped, found unknown"),
        )
        for label in ("Deep Linux Cargo", "Deep Linux Bazel release"):
            for eligibility, eligible, validation, message in cases:
                with self.subTest(label=label, state=(eligibility, eligible, validation)):
                    self.assertEqual(
                        result.Decision(False, message.format(label=label)),
                        result.evaluate(eligibility, eligible, validation, label),
                    )

    def test_missing_or_malformed_labels_fail_closed(self) -> None:
        for label in ("", " bad", "bad\nlabel", "x" * 81):
            with self.subTest(label=label):
                self.assertEqual(
                    result.Decision(False, "validation label is malformed"),
                    result.evaluate("success", "true", "success", label),
                )

    def test_main_logs_the_family_and_rejected_state(self) -> None:
        output = StringIO()
        env = {
            "ELIGIBILITY_RESULT": "success",
            "ELIGIBLE": "true",
            "VALIDATION_LABEL": "Deep Linux Bazel release",
            "VALIDATION_RESULT": "cancelled",
        }
        with patch.dict(os.environ, env, clear=True), redirect_stdout(output):
            self.assertEqual(1, result.main())
        self.assertEqual(
            "## Deep Linux Bazel release result\n\n"
            "- Eligibility job: `success`\n"
            "- Eligible output: `true`\n"
            "- Validation workflow: `cancelled`\n"
            "- Decision: eligible=true requires Deep Linux Bazel release success, "
            "found cancelled\n",
            output.getvalue(),
        )


if __name__ == "__main__":
    unittest.main()
