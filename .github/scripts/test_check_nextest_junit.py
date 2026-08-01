import contextlib
import io
from pathlib import Path
import tempfile
import unittest

import check_nextest_junit as policy


class NextestJunitTests(unittest.TestCase):
    def run_xml(self, xml: str) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as temp_dir:
            report = Path(temp_dir) / "junit.xml"
            report.write_text(xml, encoding="utf-8")
            return self.run_path(report)

    def run_path(self, report: Path) -> tuple[int, str]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = policy.main([str(report)])
        return result, output.getvalue()

    def test_retry_free_success_is_accepted(self) -> None:
        result, _ = self.run_xml(
            '<testsuites tests="1" failures="0" errors="0"><testsuite name="suite" '
            'failures="0" errors="0"><testcase name="passed" /></testsuite></testsuites>'
        )
        self.assertEqual(0, result)

    def test_every_retry_element_is_rejected_by_local_name_and_identity(self) -> None:
        for element in policy.RETRY_ELEMENTS:
            with self.subTest(element=element):
                result, output = self.run_xml(
                    '<testsuites xmlns:j="urn:jenkins" failures="0" errors="0"><testsuite '
                    'name="suite"><testcase classname="crate::module" name="recovered">'
                    f'<j:{element} message="failed first attempt" /></testcase></testsuite></testsuites>'
                )
                self.assertEqual(1, result)
                self.assertIn(f"crate::module::recovered: retry evidence <{element}>", output)

    def test_failure_elements_and_aggregate_counts_are_rejected(self) -> None:
        cases = (
            (
                '<testsuites failures="1" errors="0"><testsuite><testcase name="failed">'
                '<failure message="assertion failed" /></testcase></testsuite></testsuites>',
                "assertion failed",
            ),
            ('<testsuites failures="0" errors="1"><testsuite /></testsuites>', "errors=1"),
        )
        for xml, expected in cases:
            with self.subTest(expected=expected):
                result, output = self.run_xml(xml)
                self.assertEqual(1, result)
                self.assertIn(expected, output)

    def test_malformed_and_wrong_root_reports_are_rejected(self) -> None:
        for xml, expected in (("<testsuites>", "valid JUnit"), ("<report />", "testsuites")):
            with self.subTest(xml=xml):
                result, output = self.run_xml(xml)
                self.assertEqual(1, result)
                self.assertIn(expected, output)

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


if __name__ == "__main__":
    unittest.main()
