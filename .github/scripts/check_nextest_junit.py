#!/usr/bin/env python3
"""Reject failed or retry-assisted nextest JUnit reports."""

import argparse
from pathlib import Path
import sys
import xml.etree.ElementTree as ET


RETRY_ELEMENTS = {"flakyFailure", "flakyError", "rerunFailure", "rerunError"}
FAILURE_ELEMENTS = {"failure", "error"}


def _local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def inspect_report(report: Path) -> list[str]:
    if not report.is_file():
        return [f"required JUnit report is missing or not a regular file: {report}"]
    try:
        root = ET.parse(report).getroot()
    except (OSError, ET.ParseError) as error:
        return [f"cannot read a valid JUnit report at {report}: {error}"]
    root_name = _local_name(root.tag)
    if root_name != "testsuites":
        return [f"JUnit root must be <testsuites>, found <{root_name}>"]

    issues: list[str] = []
    for element in root.iter():
        element_name = _local_name(element.tag)
        if element_name in {"testsuites", "testsuite"}:
            identity = element.get("name", "root testsuites")
            for count_name in ("failures", "errors"):
                raw_count = element.get(count_name)
                if raw_count is None:
                    continue
                try:
                    count = int(raw_count)
                except ValueError:
                    issues.append(f"{identity}: invalid aggregate {count_name}={raw_count!r}")
                else:
                    if count:
                        issues.append(f"{identity}: aggregate {count_name}={count}")
        if element_name != "testcase":
            continue
        classname = element.get("classname", "").strip()
        name = element.get("name", "<unnamed test>").strip()
        identity = f"{classname}::{name}" if classname else name
        for child in element:
            child_name = _local_name(child.tag)
            if child_name not in RETRY_ELEMENTS | FAILURE_ELEMENTS:
                continue
            kind = "retry" if child_name in RETRY_ELEMENTS else "failure"
            detail = child.get("message", "").strip()
            issues.append(
                f"{identity}: {kind} evidence <{child_name}>"
                + (f": {detail}" if detail else "")
            )
    return issues


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path, help="required nextest JUnit XML")
    report = parser.parse_args(argv).report
    issues = inspect_report(report)
    if not issues:
        print(f"nextest JUnit policy passed: {report}")
        return 0
    print("nextest JUnit policy failed:", file=sys.stderr)
    for issue in issues:
        print(f"- {issue}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
