#!/usr/bin/env python3
"""Fail a nextest result when its JUnit report contains retry evidence."""

import argparse
from pathlib import Path
import sys
import xml.etree.ElementTree as ET


RETRY_ELEMENTS = {"flakyFailure", "flakyError", "rerunFailure", "rerunError"}
FAILURE_ELEMENTS = {"failure", "error"}


def local_name(tag: str) -> str:
    """Return an XML local name for namespaced and unqualified elements."""
    return tag.rsplit("}", 1)[-1]


def testcase_identity(testcase: ET.Element) -> str:
    classname = testcase.get("classname", "").strip()
    name = testcase.get("name", "<unnamed test>").strip()
    return f"{classname}::{name}" if classname else name


def inspect_report(report: Path) -> list[str]:
    if not report.is_file():
        return [f"required JUnit report is missing or not a regular file: {report}"]
    try:
        root = ET.parse(report).getroot()
    except (OSError, ET.ParseError) as error:
        return [f"cannot read a valid JUnit report at {report}: {error}"]

    root_name = local_name(root.tag)
    if root_name != "testsuites":
        return [f"JUnit root must be <testsuites>, found <{root_name}>"]

    issues: list[str] = []
    for aggregate in root.iter():
        aggregate_name = local_name(aggregate.tag)
        if aggregate_name not in {"testsuites", "testsuite"}:
            continue
        aggregate_identity = aggregate.get("name", "root testsuites")
        for count_name in ("failures", "errors"):
            raw_count = aggregate.get(count_name)
            if raw_count is None:
                continue
            try:
                count = int(raw_count)
            except ValueError:
                issues.append(
                    f"{aggregate_identity}: invalid aggregate {count_name}={raw_count!r}"
                )
            else:
                if count != 0:
                    issues.append(
                        f"{aggregate_identity}: aggregate {count_name}={count}"
                    )

    for testcase in (
        element for element in root.iter() if local_name(element.tag) == "testcase"
    ):
        identity = testcase_identity(testcase)
        for child in testcase:
            child_name = local_name(child.tag)
            if child_name in RETRY_ELEMENTS | FAILURE_ELEMENTS:
                detail = child.get("message", "").strip()
                suffix = f": {detail}" if detail else ""
                kind = (
                    "retry evidence"
                    if child_name in RETRY_ELEMENTS
                    else "failure evidence"
                )
                issues.append(f"{identity}: {kind} <{child_name}>{suffix}")
    return issues


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Reject failed or retry-assisted nextest JUnit reports."
    )
    parser.add_argument(
        "report", type=Path, help="path to the required nextest JUnit XML"
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    issues = inspect_report(args.report)
    if issues:
        print("nextest JUnit policy failed:", file=sys.stderr)
        for issue in issues:
            print(f"- {issue}", file=sys.stderr)
        return 1
    print(f"nextest JUnit policy passed: {args.report}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
