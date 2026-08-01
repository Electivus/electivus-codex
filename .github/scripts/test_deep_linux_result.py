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
            ("failure", "true", "skipped"),
            ("cancelled", "true", "skipped"),
            ("skipped", "true", "skipped"),
            ("unknown", "true", "skipped"),
            ("", "true", "skipped"),
            ("success", "", "skipped"),
            ("success", "TRUE", "success"),
            ("success", "unknown", "success"),
            ("success", "true", "failure"),
            ("success", "true", "cancelled"),
            ("success", "true", "skipped"),
            ("success", "true", ""),
            ("success", "true", "unknown"),
            ("success", "false", "success"),
            ("success", "false", "failure"),
            ("success", "false", "cancelled"),
            ("success", "false", ""),
            ("success", "false", "unknown"),
        )
        for label in ("Deep Linux Cargo", "Deep Linux Bazel release"):
            for eligibility, eligible, validation in cases:
                with self.subTest(label=label, state=(eligibility, eligible, validation)):
                    self.assertFalse(
                        result.evaluate(
                            eligibility, eligible, validation, label
                        ).passed
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
