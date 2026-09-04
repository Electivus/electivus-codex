import contextlib
import io
from pathlib import Path
import tempfile
import unittest

import check_nextest_junit as policy


class NextestJunitTests(unittest.TestCase):
    def run_xml(self, xml: str, *args: str) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as temp_dir:
            report = Path(temp_dir) / "junit.xml"
            report.write_text(xml, encoding="utf-8")
            return self.run_path(report, *args)

    def run_path(self, report: Path, *args: str) -> tuple[int, str]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = policy.main([str(report), *args])
        return result, output.getvalue()

    def test_every_retry_element_is_strict_by_default_but_can_be_allowed(self) -> None:
        for element in policy.RETRY_ELEMENTS:
            with self.subTest(element=element):
                xml = (
                    '<testsuites xmlns:j="urn:jenkins" failures="0" errors="0">'
                    '<testsuite name="suite"><testcase classname="crate::module" '
                    f'name="recovered"><j:{element} message="failed first attempt" />'
                    "</testcase></testsuite></testsuites>"
                )
                result, output = self.run_xml(xml)
                self.assertEqual(1, result)
                self.assertIn(f"crate::module::recovered: retry evidence <{element}>", output)
                allowed_result, allowed_output = self.run_xml(xml, "--allow-retries")
                self.assertEqual(0, allowed_result)
                self.assertIn("policy passed", allowed_output)

    def test_allow_retries_does_not_allow_a_final_failure(self) -> None:
        result, output = self.run_xml(
            '<testsuites failures="1"><testsuite><testcase name="failed">'
            '<failure message="still failing" /></testcase></testsuite></testsuites>',
            "--allow-retries",
        )
        self.assertEqual(1, result)
        self.assertIn("still failing", output)

    def test_success_and_invalid_report_shapes(self) -> None:
        cases = (
            ('<testsuites failures="0"><testsuite><testcase name="passed" /></testsuite></testsuites>', 0, ""),
            (
                '<testsuites failures="1" errors="0"><testsuite><testcase name="failed">'
                '<failure message="assertion failed" /></testcase></testsuite></testsuites>',
                1,
                "assertion failed",
            ),
            ('<testsuites failures="0" errors="1"><testsuite /></testsuites>', 1, "errors=1"),
            ("<testsuites>", 1, "valid JUnit"),
            ("<report />", 1, "testsuites"),
        )
        for xml, expected_result, expected_output in cases:
            with self.subTest(expected=expected_output):
                result, output = self.run_xml(xml)
                self.assertEqual(expected_result, result)
                self.assertIn(expected_output, output)

    def test_missing_and_non_file_reports_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            for report, expected in (
                (Path(temp_dir) / "missing.xml", "missing"),
                (Path(temp_dir), "regular file"),
            ):
                with self.subTest(report=report):
                    result, output = self.run_path(report)
                    self.assertEqual(1, result)
                self.assertIn(expected, output)

    def test_expected_testcase_count_rejects_a_green_subset(self) -> None:
        result, output = self.run_xml(
            '<testsuites><testsuite><testcase name="only-one" /></testsuite></testsuites>',
            "--expected-testcases",
            "2",
        )
        self.assertEqual((1, True), (result, "expected 2 testcases, found 1" in output))

    def test_unexpected_skip_is_rejected_even_with_exact_cardinality(self) -> None:
        for xml in (
            '<testsuites skipped="1"><testsuite><testcase name="ignored"><skipped /></testcase></testsuite></testsuites>',
            '<testsuites><testsuite skipped="1"><testcase name="ignored" /></testsuite></testsuites>',
        ):
            with self.subTest(xml=xml):
                default_result, _ = self.run_xml(xml, "--expected-testcases", "1")
                result, output = self.run_xml(xml, "--expected-testcases", "1", "--reject-skipped")
                self.assertEqual(0, default_result)
                self.assertEqual(1, result)
                self.assertIn("skip", output)


if __name__ == "__main__":
    unittest.main()
