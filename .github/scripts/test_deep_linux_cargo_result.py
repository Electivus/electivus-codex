from contextlib import redirect_stdout
from io import StringIO
import os
import unittest
from unittest.mock import patch

import deep_linux_cargo_result as cargo_result


class DeepLinuxCargoResultTests(unittest.TestCase):
    def test_only_expected_eligible_and_irrelevant_paths_pass(self) -> None:
        cases = (
            (
                "success",
                "true",
                "success",
                cargo_result.Decision(
                    True, "eligible=true; Deep Linux Cargo succeeded"
                ),
            ),
            (
                "success",
                "false",
                "skipped",
                cargo_result.Decision(
                    True, "eligible=false; Deep Linux Cargo correctly skipped"
                ),
            ),
        )
        for eligibility, eligible, cargo, expected in cases:
            with self.subTest(eligible=eligible):
                self.assertEqual(expected, cargo_result.evaluate(eligibility, eligible, cargo))

    def test_every_malformed_or_unexpected_state_fails_closed(self) -> None:
        cases = (
            ("failure", "true", "skipped", "eligibility job ended with failure"),
            ("cancelled", "true", "skipped", "eligibility job ended with cancelled"),
            ("skipped", "true", "skipped", "eligibility job ended with skipped"),
            ("unknown", "true", "skipped", "eligibility job ended with unknown"),
            ("", "true", "skipped", "eligibility job ended with missing"),
            ("success", "", "skipped", "eligibility output is malformed"),
            ("success", "TRUE", "success", "eligibility output is malformed"),
            ("success", "unknown", "success", "eligibility output is malformed"),
            ("success", "true", "failure", "eligible=true requires Cargo success, found failure"),
            ("success", "true", "cancelled", "eligible=true requires Cargo success, found cancelled"),
            ("success", "true", "skipped", "eligible=true requires Cargo success, found skipped"),
            ("success", "true", "", "eligible=true requires Cargo success, found missing"),
            ("success", "true", "unknown", "eligible=true requires Cargo success, found unknown"),
            ("success", "false", "success", "eligible=false requires Cargo skipped, found success"),
            ("success", "false", "failure", "eligible=false requires Cargo skipped, found failure"),
            ("success", "false", "cancelled", "eligible=false requires Cargo skipped, found cancelled"),
            ("success", "false", "", "eligible=false requires Cargo skipped, found missing"),
            ("success", "false", "unknown", "eligible=false requires Cargo skipped, found unknown"),
        )
        for eligibility, eligible, cargo, message in cases:
            with self.subTest(state=(eligibility, eligible, cargo)):
                self.assertEqual(
                    cargo_result.Decision(False, message),
                    cargo_result.evaluate(eligibility, eligible, cargo),
                )

    def test_main_logs_the_rejected_state_and_reason(self) -> None:
        output = StringIO()
        env = {
            "ELIGIBILITY_RESULT": "success",
            "ELIGIBLE": "true",
            "CARGO_RESULT": "cancelled",
        }
        with patch.dict(os.environ, env, clear=True), redirect_stdout(output):
            self.assertEqual(1, cargo_result.main())
        self.assertEqual(
            "## Deep Linux Cargo result\n\n"
            "- Eligibility job: `success`\n"
            "- Eligible output: `true`\n"
            "- Cargo workflow: `cancelled`\n"
            "- Decision: eligible=true requires Cargo success, found cancelled\n",
            output.getvalue(),
        )


if __name__ == "__main__":
    unittest.main()
