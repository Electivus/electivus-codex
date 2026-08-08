from contextlib import redirect_stdout
from io import StringIO
import os
import unittest
from unittest.mock import patch

import v8_canary_result as result


class V8CanaryResultTests(unittest.TestCase):
    def test_only_full_matrix_success_and_metadata_only_skip_pass(self) -> None:
        cases = (
            (
                "success", "true", "V8 path changed", "success",
                result.Decision(True, "V8 canary required and all matrix legs succeeded"),
            ),
            (
                "success", "false", "ordinary Codex source", "skipped",
                result.Decision(True, "V8 canary not required and build correctly skipped"),
            ),
        )
        for metadata, required, reason, build, expected in cases:
            with self.subTest(required=required):
                self.assertEqual(
                    expected, result.evaluate(metadata, required, reason, build)
                )

    def test_every_incomplete_or_inconsistent_state_fails_closed(self) -> None:
        cases = (
            ("failure", "true", "reason", "skipped", "metadata job ended with failure"),
            ("cancelled", "true", "reason", "skipped", "metadata job ended with cancelled"),
            ("skipped", "true", "reason", "skipped", "metadata job ended with skipped"),
            ("", "true", "reason", "skipped", "metadata job ended with missing"),
            ("success", "", "reason", "skipped", "canary_required output is malformed"),
            ("success", "TRUE", "reason", "success", "canary_required output is malformed"),
            ("success", "true", "", "success", "canary reason is malformed"),
            ("success", "true", "bad\nreason", "success", "canary reason is malformed"),
            ("success", "true", "x" * 241, "success", "canary reason is malformed"),
            ("success", "true", "reason", "failure", "canary_required=true requires build success, found failure"),
            ("success", "true", "reason", "cancelled", "canary_required=true requires build success, found cancelled"),
            ("success", "true", "reason", "skipped", "canary_required=true requires build success, found skipped"),
            ("success", "true", "reason", "", "canary_required=true requires build success, found missing"),
            ("success", "false", "reason", "success", "canary_required=false requires build skipped, found success"),
            ("success", "false", "reason", "failure", "canary_required=false requires build skipped, found failure"),
            ("success", "false", "reason", "cancelled", "canary_required=false requires build skipped, found cancelled"),
            ("success", "false", "reason", "", "canary_required=false requires build skipped, found missing"),
        )
        for metadata, required, reason, build, message in cases:
            with self.subTest(state=(metadata, required, build)):
                self.assertEqual(
                    result.Decision(False, message),
                    result.evaluate(metadata, required, reason, build),
                )

    def test_main_writes_bounded_summary(self) -> None:
        output = StringIO()
        env = {
            "BUILD_RESULT": "skipped",
            "CANARY_REASON": "ordinary Codex source",
            "CANARY_REQUIRED": "false",
            "METADATA_RESULT": "success",
        }
        with patch.dict(os.environ, env, clear=True), redirect_stdout(output):
            self.assertEqual(0, result.main())
        self.assertIn("- Required: `false`\n", output.getvalue())
        self.assertIn("- Build matrix: `skipped`\n", output.getvalue())
        self.assertIn("- Reason: ordinary Codex source\n", output.getvalue())


if __name__ == "__main__":
    unittest.main()
