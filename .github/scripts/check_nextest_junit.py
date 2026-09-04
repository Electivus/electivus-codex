#!/usr/bin/env python3
"""Reject failed nextest JUnit reports, optionally allowing recovered retries."""

import argparse
from pathlib import Path
import sys
import xml.etree.ElementTree as ET


RETRY_ELEMENTS = {"flakyFailure", "flakyError", "rerunFailure", "rerunError"}
FAILURE_ELEMENTS = {"failure", "error"}
SKIP_ELEMENTS = {"skipped"}


def _local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def inspect_report(
    report: Path,
    expected_testcases: int | None = None,
    *,
    reject_skipped: bool = False,
    allow_retries: bool = False,
) -> list[str]:
    if not report.is_file():
        return [f"required JUnit report is missing or not a regular file: {report}"]
    try:
        root = ET.parse(report).getroot()
    except (OSError, ET.ParseError) as error:
        return [f"cannot read a valid JUnit report at {report}: {error}"]
    if (root_name := _local_name(root.tag)) != "testsuites":
        return [f"JUnit root must be <testsuites>, found <{root_name}>"]

    issues: list[str] = []
    testcase_count = 0
    for element in root.iter():
        element_name = _local_name(element.tag)
        if element_name in {"testsuites", "testsuite"}:
            identity = element.get("name", "root testsuites")
            count_names = ("failures", "errors", "skipped") if reject_skipped else ("failures", "errors")
            for count_name in count_names:
                raw_count = element.get(count_name, "0")
                try:
                    count = int(raw_count)
                except (TypeError, ValueError):
                    issues.append(f"{identity}: invalid aggregate {count_name}={raw_count!r}")
                    continue
                if count:
                    issues.append(f"{identity}: aggregate {count_name}={count}")
        if element_name != "testcase":
            continue
        testcase_count += 1
        classname = element.get("classname", "").strip()
        name = element.get("name", "<unnamed test>").strip()
        identity = f"{classname}::{name}" if classname else name
        for child in element:
            child_name = _local_name(child.tag)
            signal_elements = FAILURE_ELEMENTS | (SKIP_ELEMENTS if reject_skipped else set())
            if not allow_retries:
                signal_elements |= RETRY_ELEMENTS
            if child_name not in signal_elements:
                continue
            kind = "retry" if child_name in RETRY_ELEMENTS else "skip" if child_name in SKIP_ELEMENTS else "failure"
            detail = child.get("message", "").strip()
            issues.append(f"{identity}: {kind} evidence <{child_name}>" + (f": {detail}" if detail else ""))
    if expected_testcases is not None and testcase_count != expected_testcases:
        issues.append(
            f"expected {expected_testcases} testcases, found {testcase_count}"
        )
    return issues


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path, help="required nextest JUnit XML")
    parser.add_argument("--expected-testcases", type=int)
    parser.add_argument("--reject-skipped", action="store_true")
    parser.add_argument(
        "--allow-retries",
        action="store_true",
        help="accept tests that passed within nextest's configured retry limit",
    )
    args = parser.parse_args(argv)
    report = args.report
    issues = inspect_report(
        report,
        args.expected_testcases,
        reject_skipped=args.reject_skipped,
        allow_retries=args.allow_retries,
    )
    if issues:
        print("nextest JUnit policy failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
        return 1
    print(f"nextest JUnit policy passed: {report}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
