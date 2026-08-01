#!/usr/bin/env python3
"""Inventory Rust test ignores and validate temporary check quarantines."""

import argparse
from collections import Counter
from dataclasses import dataclass
import datetime as dt
from pathlib import Path
import re
import subprocess
import sys
import tomllib


ALLOWED_CATEGORIES = {
    "helper-process",
    "live-external-api",
    "manual-smoke",
    "out-of-boundary-platform",
    "pending-behavior-change",
    "schema-generation",
    "specialized-environment",
    "temporary-certification",
}
TEMPORARY_CERTIFICATION_TESTS = {
    "injected_user_input_triggers_follow_up_request_with_deltas",
    "review_start_exec_approval_item_id_matches_command_execution_item",
}
GITHUB_TRACKING_RE = re.compile(
    r"https://github\.com/[^/]+/[^/]+/(?:issues|pull)/[1-9][0-9]*$"
)
FUNCTION_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
RAW_STRING_RE = re.compile(r'(?:b|c)?r(#{0,255})"')


@dataclass(frozen=True, order=True)
class IgnoreOccurrence:
    path: str
    test: str
    attribute: str

    @property
    def identity(self) -> str:
        return f"{self.path}::{self.test}::{self.attribute}"


def _raw_string_end(text: str, start: int) -> int | None:
    if text[start] not in "bcr":
        return None
    match = RAW_STRING_RE.match(text, start)
    if match is None:
        return None
    delimiter = '"' + match.group(1)
    content_start = match.end()
    close = text.find(delimiter, content_start)
    return len(text) if close < 0 else close + len(delimiter)


def _quoted_end(text: str, start: int, quote: str) -> int:
    cursor = start + 1
    while cursor < len(text):
        if text[cursor] == "\\":
            cursor += 2
        elif text[cursor] == quote:
            return cursor + 1
        else:
            cursor += 1
    return len(text)


def _block_comment_end(text: str, start: int) -> int:
    depth = 1
    cursor = start + 2
    while cursor < len(text) and depth:
        if text.startswith("/*", cursor):
            depth += 1
            cursor += 2
        elif text.startswith("*/", cursor):
            depth -= 1
            cursor += 2
        else:
            cursor += 1
    return cursor


def _attribute_end(text: str, opening_bracket: int) -> int:
    depth = 1
    cursor = opening_bracket + 1
    while cursor < len(text) and depth:
        raw_end = _raw_string_end(text, cursor)
        if raw_end is not None:
            cursor = raw_end
        elif text.startswith("//", cursor):
            newline = text.find("\n", cursor + 2)
            cursor = len(text) if newline < 0 else newline + 1
        elif text.startswith("/*", cursor):
            cursor = _block_comment_end(text, cursor)
        elif text[cursor] == '"':
            cursor = _quoted_end(text, cursor, '"')
        elif text[cursor] == "'" and (
            (cursor + 2 < len(text) and text[cursor + 2] == "'")
            or (
                cursor + 3 < len(text)
                and text[cursor + 1] == "\\"
                and text[cursor + 3] == "'"
            )
        ):
            cursor = _quoted_end(text, cursor, "'")
        elif text[cursor] == "[":
            depth += 1
            cursor += 1
        elif text[cursor] == "]":
            depth -= 1
            cursor += 1
        else:
            cursor += 1
    return cursor


def _scan_attributes(text: str) -> list[tuple[str, int]]:
    attributes: list[tuple[str, int]] = []
    cursor = 0
    while cursor < len(text):
        raw_end = _raw_string_end(text, cursor)
        if raw_end is not None:
            cursor = raw_end
        elif text.startswith("//", cursor):
            newline = text.find("\n", cursor + 2)
            cursor = len(text) if newline < 0 else newline + 1
        elif text.startswith("/*", cursor):
            cursor = _block_comment_end(text, cursor)
        elif text[cursor] == '"':
            cursor = _quoted_end(text, cursor, '"')
        elif text[cursor] == "#":
            opening = cursor + 1
            while opening < len(text) and text[opening].isspace():
                opening += 1
            if opening < len(text) and text[opening] == "[":
                end = _attribute_end(text, opening)
                attributes.append((text[opening + 1 : end - 1], end))
                cursor = end
            else:
                cursor += 1
        else:
            cursor += 1
    return attributes


def _compact_meta(text: str) -> str:
    result: list[str] = []
    cursor = 0
    while cursor < len(text):
        if text[cursor].isspace():
            cursor += 1
        elif text.startswith("//", cursor):
            newline = text.find("\n", cursor + 2)
            cursor = len(text) if newline < 0 else newline + 1
        elif text.startswith("/*", cursor):
            cursor = _block_comment_end(text, cursor)
        elif text[cursor] == '"':
            end = _quoted_end(text, cursor, '"')
            result.append(text[cursor:end])
            cursor = end
        else:
            result.append(text[cursor])
            cursor += 1
    return "".join(result)


def _split_top_level(arguments: str) -> list[str]:
    parts: list[str] = []
    start = 0
    depth = 0
    cursor = 0
    while cursor < len(arguments):
        if arguments[cursor] == '"':
            cursor = _quoted_end(arguments, cursor, '"')
            continue
        if arguments[cursor] in "([{":
            depth += 1
        elif arguments[cursor] in ")]}":
            depth -= 1
        elif arguments[cursor] == "," and depth == 0:
            parts.append(arguments[start:cursor])
            start = cursor + 1
        cursor += 1
    parts.append(arguments[start:])
    return parts


def _ignore_forms(attribute: str) -> list[str]:
    compact = _compact_meta(attribute)
    if re.fullmatch(r"ignore(?:=\"(?:\\.|[^\"\\])*\")?", compact):
        return [compact]
    if not compact.startswith("cfg_attr(") or not compact.endswith(")"):
        return []
    arguments = _split_top_level(compact[len("cfg_attr(") : -1])
    if len(arguments) < 2:
        return []
    condition = arguments[0]
    return [
        f"cfg_attr({condition},{candidate})"
        for candidate in arguments[1:]
        if re.fullmatch(r"ignore(?:=\"(?:\\.|[^\"\\])*\")?", candidate)
    ]


def _code_without_comments_or_strings(text: str) -> str:
    output = list(text)
    cursor = 0
    while cursor < len(text):
        raw_end = _raw_string_end(text, cursor)
        if raw_end is not None:
            output[cursor:raw_end] = " " * (raw_end - cursor)
            cursor = raw_end
        elif text.startswith("//", cursor):
            newline = text.find("\n", cursor + 2)
            end = len(text) if newline < 0 else newline
            output[cursor:end] = " " * (end - cursor)
            cursor = end
        elif text.startswith("/*", cursor):
            end = _block_comment_end(text, cursor)
            output[cursor:end] = " " * (end - cursor)
            cursor = end
        elif text[cursor] == '"':
            end = _quoted_end(text, cursor, '"')
            output[cursor:end] = " " * (end - cursor)
            cursor = end
        else:
            cursor += 1
    return "".join(output)


def inventory_file(path: str, text: str) -> list[IgnoreOccurrence]:
    candidates = [
        (forms, end)
        for attribute, end in _scan_attributes(text)
        if (forms := _ignore_forms(attribute))
    ]
    if not candidates:
        return []
    code = _code_without_comments_or_strings(text)
    occurrences: list[IgnoreOccurrence] = []
    for forms, end in candidates:
        function = FUNCTION_RE.search(code, end)
        test = function.group(1) if function else "<missing-test-function>"
        occurrences.extend(IgnoreOccurrence(path, test, form) for form in forms)
    return occurrences


def tracked_rust_files(repo: Path) -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=repo,
        check=True,
        capture_output=True,
    )
    return sorted(path for path in result.stdout.decode().split("\0") if path)


def inventory_ignores(repo: Path) -> list[IgnoreOccurrence]:
    occurrences: list[IgnoreOccurrence] = []
    for relative_path in tracked_rust_files(repo):
        text = (repo / relative_path).read_text(encoding="utf-8")
        if "ignore" not in text:
            continue
        occurrences.extend(inventory_file(relative_path, text))
    return sorted(occurrences)


def load_toml(path: Path) -> tuple[dict[str, object] | None, list[str]]:
    if not path.is_file():
        return None, [f"required manifest is missing or not a regular file: {path}"]
    try:
        with path.open("rb") as source:
            return tomllib.load(source), []
    except (OSError, tomllib.TOMLDecodeError) as error:
        return None, [f"cannot read valid TOML from {path}: {error}"]


def validate_ignore_policy(
    occurrences: list[IgnoreOccurrence], policy: dict[str, object]
) -> list[str]:
    issues: list[str] = []
    records = policy.get("ignore", [])
    compact_records = policy.get("ignores")
    if compact_records is not None:
        if records:
            return ["rust test policy must use only one of 'ignore' or 'ignores'"]
        if not isinstance(compact_records, dict):
            return ["rust test policy field 'ignores' must be a table"]
        records = []
        for identity, classification in compact_records.items():
            if not isinstance(identity, str) or not isinstance(classification, str):
                issues.append(
                    "rust test policy identities and categories must be strings"
                )
                continue
            category, separator, tracking = classification.partition("|")
            record = {
                "_identity": identity,
                "category": category,
            }
            if separator:
                record["tracking"] = tracking
            records.append(record)
    if not isinstance(records, list):
        return ["rust test policy field 'ignore' must be an array of tables"]

    inventory_counts = Counter(occurrence.identity for occurrence in occurrences)
    for identity, count in inventory_counts.items():
        if count > 1:
            issues.append(f"duplicate Rust ignore occurrence: {identity}")

    classified: dict[str, str] = {}
    inventory_by_identity = {
        occurrence.identity: occurrence for occurrence in occurrences
    }
    for index, record in enumerate(records, 1):
        label = f"ignore record {index}"
        if not isinstance(record, dict):
            issues.append(f"{label} must be a table")
            continue
        values: dict[str, str] = {}
        required_fields = (
            ("category",)
            if isinstance(record.get("_identity"), str)
            else ("path", "test", "attribute", "category")
        )
        for field in required_fields:
            value = record.get(field)
            if not isinstance(value, str) or not value.strip():
                issues.append(f"{label} requires nonblank {field}")
            else:
                values[field] = value.strip()
        if len(values) != len(required_fields):
            continue
        compact_identity = record.get("_identity")
        if isinstance(compact_identity, str):
            identity = compact_identity
            occurrence = inventory_by_identity.get(identity)
            test = occurrence.test if occurrence is not None else "<stale>"
        else:
            occurrence = IgnoreOccurrence(
                values["path"], values["test"], values["attribute"]
            )
            identity = occurrence.identity
            test = occurrence.test
        if identity in classified:
            issues.append(f"duplicate policy classification: {identity}")
        classified[identity] = values["category"]
        if values["category"] not in ALLOWED_CATEGORIES:
            issues.append(f"{identity}: unknown category {values['category']!r}")
        tracking = record.get("tracking")
        if values["category"] == "temporary-certification":
            if test not in TEMPORARY_CERTIFICATION_TESTS:
                issues.append(
                    f"{identity}: only the two #89 inherited flaky tests may use temporary-certification"
                )
            if tracking != "https://github.com/Electivus/electivus-codex/issues/89":
                issues.append(f"{identity}: temporary-certification must track #89")
        elif test in TEMPORARY_CERTIFICATION_TESTS:
            issues.append(
                f"{identity}: inherited flaky test must use temporary-certification"
            )

    for occurrence in occurrences:
        if occurrence.identity not in classified:
            issues.append(f"unclassified Rust ignore: {occurrence.identity}")
    for identity in classified:
        if identity not in inventory_counts:
            issues.append(f"stale Rust ignore classification: {identity}")
    return issues


def _required_string(
    record: dict[str, object], field: str, label: str, issues: list[str]
) -> str | None:
    value = record.get(field)
    if not isinstance(value, str) or not value.strip():
        issues.append(f"{label} requires nonblank {field}")
        return None
    return value.strip()


def validate_quarantines(policy: dict[str, object], today: dt.date) -> list[str]:
    issues: list[str] = []
    records = policy.get("quarantines", [])
    if not isinstance(records, list):
        return ["quarantine manifest must contain an array of records"]
    identities: set[str] = set()
    for index, record in enumerate(records, 1):
        label = f"quarantine record {index}"
        if not isinstance(record, dict):
            issues.append(f"{label} must be a table")
            continue
        identity = _required_string(record, "check_identity", label, issues)
        scope = _required_string(record, "scope", label, issues)
        _required_string(record, "evidence", label, issues)
        justification = _required_string(record, "justification", label, issues)
        _required_string(record, "extended_workflow", label, issues)
        _required_string(record, "extended_job", label, issues)
        tracking = _required_string(record, "tracking", label, issues)

        if identity:
            if identity in identities:
                issues.append(f"duplicate quarantined check identity: {identity}")
            identities.add(identity)
            if any(character in identity for character in "*?[]"):
                issues.append(
                    f"{label} check_identity must be exact, without wildcards"
                )
        if scope and (
            any(character in scope for character in "*?[]")
            or scope.casefold() in {"all", "all checks", "all tests", "all platforms"}
        ):
            issues.append(f"{label} scope must name the narrowest affected surface")
        if justification and len(justification.split()) < 4:
            issues.append(f"{label} justification must be substantive")
        if tracking and not GITHUB_TRACKING_RE.fullmatch(tracking):
            issues.append(
                f"{label} tracking must be an exact GitHub issue or pull request URL"
            )

        start = record.get("start_date")
        expiry = record.get("expiry_date")
        valid_start = isinstance(start, dt.date) and not isinstance(start, dt.datetime)
        valid_expiry = isinstance(expiry, dt.date) and not isinstance(
            expiry, dt.datetime
        )
        if not valid_start:
            issues.append(f"{label} requires TOML date start_date")
        if not valid_expiry:
            issues.append(f"{label} requires TOML date expiry_date")
        if valid_start and valid_expiry:
            if start > today:
                issues.append(f"{label} starts in the future on {start.isoformat()}")
            if expiry < start:
                issues.append(f"{label} expiry_date precedes start_date")
            if expiry > start + dt.timedelta(days=7):
                issues.append(f"{label} expiry_date exceeds the seven-day maximum")
            if expiry < today:
                issues.append(f"{label} expired on {expiry.isoformat()}")
    return issues


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Validate tracked Rust ignores and active check quarantines."
    )
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument(
        "--policy", type=Path, default=Path(".github/rust-test-policy.toml")
    )
    parser.add_argument(
        "--quarantines", type=Path, default=Path(".github/quarantined-checks.toml")
    )
    parser.add_argument("--today", type=dt.date.fromisoformat, default=None)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    repo = args.repo.resolve()
    policy_path = args.policy if args.policy.is_absolute() else repo / args.policy
    quarantine_path = (
        args.quarantines if args.quarantines.is_absolute() else repo / args.quarantines
    )
    issues: list[str] = []
    try:
        occurrences = inventory_ignores(repo)
    except (OSError, subprocess.CalledProcessError, UnicodeError) as error:
        occurrences = []
        issues.append(f"cannot inventory tracked Rust sources: {error}")

    policy, policy_issues = load_toml(policy_path)
    issues.extend(policy_issues)
    if policy is not None:
        if policy.get("version") != 1:
            issues.append("rust test policy requires version = 1")
        else:
            issues.extend(validate_ignore_policy(occurrences, policy))

    quarantines, quarantine_issues = load_toml(quarantine_path)
    issues.extend(quarantine_issues)
    if quarantines is not None:
        if quarantines.get("version") != 1:
            issues.append("quarantine manifest requires version = 1")
        else:
            issues.extend(
                validate_quarantines(quarantines, args.today or dt.date.today())
            )

    if issues:
        print("Rust test signal policy failed:", file=sys.stderr)
        for issue in issues:
            print(f"- {issue}", file=sys.stderr)
        return 1
    print(
        f"Rust test signal policy passed: {len(occurrences)} ignore occurrences; "
        f"{len(quarantines.get('quarantines', []))} active quarantines"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
