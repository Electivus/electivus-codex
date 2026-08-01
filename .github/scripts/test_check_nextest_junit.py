import contextlib
import io
from pathlib import Path
import tempfile
import unittest

import check_nextest_junit


class NextestJunitCliTests(unittest.TestCase):
    def run_cli(self, xml: str) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as temp_dir:
            report = Path(temp_dir) / "junit.xml"
            report.write_text(xml, encoding="utf-8")
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                result = check_nextest_junit.main([str(report)])
        return result, output.getvalue()

    def test_retry_assisted_pass_is_rejected_with_test_identity(self) -> None:
        result, output = self.run_cli(
            """\
            <testsuites tests="1" failures="0" errors="0">
              <testsuite name="suite" tests="1" failures="0" errors="0">
                <testcase classname="crate::module" name="recovered">
                  <flakyFailure message="failed first attempt" />
                </testcase>
              </testsuite>
            </testsuites>
            """
        )

        self.assertEqual(1, result)
        self.assertIn("crate::module::recovered", output)
        self.assertIn("flakyFailure", output)

    def test_retry_free_success_is_accepted(self) -> None:
        result, output = self.run_cli(
            """\
            <testsuites tests="1" failures="0" errors="0">
              <testsuite name="suite" tests="1" failures="0" errors="0">
                <testcase classname="crate::module" name="passed" />
              </testsuite>
            </testsuites>
            """
        )

        self.assertEqual(0, result)
        self.assertIn("passed", output)

    def test_every_retry_element_is_rejected_by_local_name(self) -> None:
        for element in ("flakyFailure", "flakyError", "rerunFailure", "rerunError"):
            with self.subTest(element=element):
                result, output = self.run_cli(
                    f"""\
                    <testsuites xmlns:jenkins="urn:jenkins" tests="1" failures="0" errors="0">
                      <testsuite name="suite" tests="1" failures="0" errors="0">
                        <testcase classname="crate" name="retried">
                          <jenkins:{element} />
                        </testcase>
                      </testsuite>
                    </testsuites>
                    """
                )

                self.assertEqual(1, result)
                self.assertIn(element, output)

    def test_testcase_failure_is_rejected(self) -> None:
        result, output = self.run_cli(
            """\
            <testsuites tests="1" failures="1" errors="0">
              <testsuite name="suite" tests="1" failures="1" errors="0">
                <testcase classname="crate" name="failed">
                  <failure message="assertion failed" />
                </testcase>
              </testsuite>
            </testsuites>
            """
        )

        self.assertEqual(1, result)
        self.assertIn("crate::failed", output)
        self.assertIn("assertion failed", output)

    def test_nonzero_aggregate_error_count_is_rejected(self) -> None:
        result, output = self.run_cli(
            '<testsuites tests="1" failures="0" errors="1"><testsuite name="suite" /></testsuites>'
        )

        self.assertEqual(1, result)
        self.assertIn("errors=1", output)

    def test_wrong_root_is_rejected(self) -> None:
        result, output = self.run_cli("<report />")

        self.assertEqual(1, result)
        self.assertIn("testsuites", output)

    def test_malformed_report_is_rejected(self) -> None:
        result, output = self.run_cli("<testsuites>")

        self.assertEqual(1, result)
        self.assertIn("valid JUnit report", output)

    def test_missing_report_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            report = Path(temp_dir) / "missing.xml"
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                result = check_nextest_junit.main([str(report)])

        self.assertEqual(1, result)
        self.assertIn("missing", output.getvalue())

    def test_report_path_must_be_a_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                result = check_nextest_junit.main([temp_dir])

        self.assertEqual(1, result)
        self.assertIn("regular file", output.getvalue())


if __name__ == "__main__":
    unittest.main()
