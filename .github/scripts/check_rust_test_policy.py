#!/usr/bin/env python3
"""Inventory Rust test ignores and validate temporary check quarantines."""

import argparse
from collections import Counter
from dataclasses import dataclass
import datetime as dt
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib


ALLOWED_CATEGORIES = frozenset("helper-process live-external-api manual-smoke out-of-boundary-platform pending-behavior-change schema-generation specialized-environment temporary-certification".split())
TEMPORARY_CERTIFICATION_TESTS = ("injected_user_input_triggers_follow_up_request_with_deltas", "review_start_exec_approval_item_id_matches_command_execution_item")
GITHUB_TRACKING_RE = re.compile(r"https://github\.com/[^/]+/[^/]+/(?:issues|pull)/[1-9][0-9]*$")
RAW_STRING_RE = re.compile(r'(?:b|c)?r(#{0,255})"')
FUNCTION_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
ATTRIBUTE_START_RE = re.compile(r"#\s*\[")
JOB_RE = re.compile(r"  ([A-Za-z_][A-Za-z0-9_-]*):\s*(?:#.*)?$")
ACTIONLINT_IGNORE = "SC2317|SC2129"


@dataclass(frozen=True, order=True)
class IgnoreOccurrence:
    path: str
    test: str
    attribute: str

    @property
    def identity(self) -> str:
        return f"{self.path}::{self.test}::{self.attribute}"

@dataclass(frozen=True, order=True)
class RustFunctionOccurrence:
    path: str
    name: str

    @property
    def identity(self) -> str:
        return f"{self.path}::{self.name}"

def _raw_string_end(text: str, start: int) -> int | None:
    match = RAW_STRING_RE.match(text, start) if text[start] in "bcr" else None
    if match is None:
        return None
    delimiter = '"' + match.group(1)
    close = text.find(delimiter, match.end())
    return len(text) if close < 0 else close + len(delimiter)

def _quoted_end(text: str, start: int, quote: str) -> int:
    cursor = start + 1
    while cursor < len(text):
        if text[cursor] == quote:
            return cursor + 1
        cursor += 2 if text[cursor] == "\\" else 1
    return len(text)

def _block_comment_end(text: str, start: int) -> int:
    depth, cursor = 1, start + 2
    while cursor < len(text) and depth:
        marker = text[cursor : cursor + 2]
        if marker in {"/*", "*/"}:
            depth += 1 if marker == "/*" else -1
            cursor += 2
        else:
            cursor += 1
    return cursor

def rust_lexemes(text: str) -> list[tuple[str, int, int]]:
    """Split Rust source once so every consumer agrees on code versus text."""
    lexemes: list[tuple[str, int, int]] = []
    code_start = cursor = 0
    while cursor < len(text):
        end = _raw_string_end(text, cursor)
        kind = "string"
        if end is None and text.startswith("//", cursor):
            newline = text.find("\n", cursor + 2)
            end = len(text) if newline < 0 else newline
            kind = "comment"
        elif end is None and text.startswith("/*", cursor):
            end = _block_comment_end(text, cursor)
            kind = "comment"
        elif end is None and text[cursor] == '"':
            end = _quoted_end(text, cursor, '"')
        elif end is None and text[cursor] == "'" and (
            cursor + 2 < len(text) and text[cursor + 2] == "'"
            or cursor + 3 < len(text) and text[cursor + 1] == "\\" and text[cursor + 3] == "'"
        ):
            end = _quoted_end(text, cursor, "'")
        if end is None:
            cursor += 1
            continue
        if code_start < cursor:
            lexemes.append(("code", code_start, cursor))
        lexemes.append((kind, cursor, end))
        cursor = end
        code_start = cursor
    if code_start < len(text):
        lexemes.append(("code", code_start, len(text)))
    return lexemes

def _code_mask(text: str) -> str:
    return "".join(
        text[start:end] if kind == "code" else " " * (end - start)
        for kind, start, end in rust_lexemes(text)
    )

def _normalize_meta(meta: str) -> str:
    return "".join(
        "".join(meta[start:end].split())
        if kind == "code"
        else meta[start:end]
        if kind == "string"
        else ""
        for kind, start, end in rust_lexemes(meta)
    )

def _split_top_level(arguments: str) -> list[str]:
    masked = _code_mask(arguments)
    parts: list[str] = []
    start = depth = 0
    for cursor, character in enumerate(masked):
        if character in "([{":
            depth += 1
        elif character in ")]}":
            depth -= 1
        elif character == "," and depth == 0:
            parts.append(arguments[start:cursor])
            start = cursor + 1
    parts.append(arguments[start:])
    return parts

def _is_string_literal(value: str) -> bool:
    if value.startswith('"'):
        return _quoted_end(value, 0, '"') == len(value)
    return bool(value) and _raw_string_end(value, 0) == len(value)

def _is_ignore_meta(meta: str) -> bool:
    return meta == "ignore" or (
        meta.startswith("ignore=") and _is_string_literal(meta[len("ignore=") :])
    )

def _ignore_forms(attribute: str) -> list[str]:
    compact = _normalize_meta(attribute)
    if _is_ignore_meta(compact):
        return [compact]
    if not compact.startswith("cfg_attr(") or not compact.endswith(")"):
        return []
    arguments = _split_top_level(compact[len("cfg_attr(") : -1])
    if len(arguments) < 2:
        return []
    return [
        f"cfg_attr({arguments[0]},{candidate})"
        for candidate in arguments[1:]
        if _is_ignore_meta(candidate)
    ]

def _function_attributes(text: str) -> list[tuple[str, str]]:
    masked = _code_mask(text)
    attributes: list[tuple[str, str]] = []
    for match in ATTRIBUTE_START_RE.finditer(masked):
        opening = match.end() - 1
        depth = 1
        cursor = opening + 1
        while cursor < len(masked) and depth:
            depth += masked[cursor] == "["
            depth -= masked[cursor] == "]"
            cursor += 1
        if depth:
            continue
        function = FUNCTION_RE.search(masked, cursor)
        test = function.group(1) if function else "<missing-test-function>"
        attributes.append((test, text[opening + 1 : cursor - 1]))
    return attributes

def inventory_file(path: str, text: str) -> list[IgnoreOccurrence]:
    return [
        IgnoreOccurrence(path, test, form)
        for test, attribute in _function_attributes(text)
        for form in _ignore_forms(attribute)
    ]

def _tracked_rust_sources(repo: Path) -> list[tuple[str, str]]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=repo,
        check=True,
        capture_output=True,
    )
    return [
        (relative_path, (repo / relative_path).read_text(encoding="utf-8"))
        for relative_path in filter(None, sorted(result.stdout.decode().split("\0")))
    ]

def inventory_ignores(repo: Path) -> list[IgnoreOccurrence]:
    occurrences: list[IgnoreOccurrence] = []
    for relative_path, text in _tracked_rust_sources(repo):
        if "ignore" in text:
            occurrences.extend(inventory_file(relative_path, text))
    return sorted(occurrences)

def inventory_rust_test_functions(repo: Path) -> list[RustFunctionOccurrence]:
    return sorted(
        {
            RustFunctionOccurrence(path, test)
            for path, text in _tracked_rust_sources(repo)
            for test, attribute in _function_attributes(text)
            if re.fullmatch(r"(?:[A-Za-z_][A-Za-z0-9_]*::)*test(?:\(.*\))?", _normalize_meta(attribute))
        }
    )

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
    records = policy.get("ignores")
    if not isinstance(records, dict):
        return ["rust test policy field 'ignores' must be a table"]
    pending_records = policy.get("pending_upstream_ignores", {})
    if not isinstance(pending_records, dict):
        return ["rust test policy field 'pending_upstream_ignores' must be a table"]
    counts = Counter(occurrence.identity for occurrence in occurrences)
    inventory = {occurrence.identity: occurrence for occurrence in occurrences}
    issues = [f"duplicate Rust ignore occurrence: {identity}" for identity, count in counts.items() if count > 1]
    for identity, classification in records.items():
        if not isinstance(identity, str) or not isinstance(classification, str):
            issues.append("rust test policy identities and categories must be strings")
            continue
        category, separator, tracking = classification.partition("|")
        occurrence = inventory.get(identity)
        if category not in ALLOWED_CATEGORIES:
            issues.append(f"{identity}: unknown category {category!r}")
        if occurrence is None:
            issues.append(f"stale Rust ignore classification: {identity}")
            continue
        if category == "temporary-certification":
            if occurrence.test not in TEMPORARY_CERTIFICATION_TESTS:
                issues.append(f"{identity}: only the two #89 tests may be temporary")
            if not separator or tracking != "https://github.com/Electivus/electivus-codex/issues/89":
                issues.append(f"{identity}: temporary-certification must track #89")
        elif occurrence.test in TEMPORARY_CERTIFICATION_TESTS:
            issues.append(f"{identity}: inherited flaky test must be temporary-certification")
    for identity, classification in pending_records.items():
        if not isinstance(identity, str) or not isinstance(classification, str):
            issues.append("pending upstream ignore identities and classifications must be strings")
            continue
        if identity in records:
            issues.append(f"duplicate active and pending Rust ignore classification: {identity}")
        category, separator, tracking = classification.partition("|")
        if category not in ALLOWED_CATEGORIES:
            issues.append(f"{identity}: unknown pending upstream category {category!r}")
        if category == "temporary-certification":
            issues.append(f"{identity}: pending upstream ignores cannot be temporary-certification")
        occurrence = inventory.get(identity)
        if (
            identity not in records
            and occurrence is not None
            and occurrence.test in TEMPORARY_CERTIFICATION_TESTS
        ):
            issues.append(f"{identity}: inherited flaky test must be temporary-certification")
        if not separator or GITHUB_TRACKING_RE.fullmatch(tracking) is None:
            issues.append(
                f"{identity}: pending upstream ignore requires a GitHub issue or pull request URL"
            )
    classified = records.keys() | pending_records.keys()
    issues.extend(
        f"unclassified Rust ignore: {identity}"
        for identity in inventory.keys() - classified
    )
    return issues


def _required_string(
    record: dict[str, object], field: str, label: str, issues: list[str]
) -> str | None:
    value = record.get(field)
    if not isinstance(value, str) or not value.strip():
        issues.append(f"{label} requires nonblank {field}")
        return None
    return value.strip()


def _workflow_jobs(
    repo: Path, name: str, *, actionlint: str = "actionlint"
) -> tuple[set[str], list[str]]:
    relative = Path(name)
    if relative.name != name or relative.suffix not in {".yml", ".yaml"}:
        return set(), [f"extended_workflow must be a workflow filename: {name}"]
    path = repo / ".github/workflows" / name
    if not path.is_file():
        return set(), [f"extended workflow does not exist: {name}"]
    executable = shutil.which(actionlint)
    if executable is None:
        return set(), [f"actionlint executable not found: {actionlint}"]
    try:
        result = subprocess.run(
            [
                executable,
                "-no-color",
                "-format",
                "{{json .}}",
                "-ignore",
                ACTIONLINT_IGNORE,
                str(path.resolve()),
            ],
            cwd=repo,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        return set(), [f"cannot execute actionlint for {name}: {error}"]
    if result.returncode:
        try:
            diagnostics = json.loads(result.stdout)
            detail = "; ".join(
                f"{diagnostic['message']} [{diagnostic['kind']}]"
                for diagnostic in diagnostics
            )
        except (json.JSONDecodeError, KeyError, TypeError):
            detail = (result.stdout or result.stderr).strip()
        if not detail:
            detail = f"actionlint exited with status {result.returncode}"
        return set(), [f"actionlint rejected extended workflow {name}: {detail}"]
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        return set(), [f"cannot read extended workflow {name}: {error}"]
    jobs_markers = [index for index, line in enumerate(lines) if re.fullmatch(r"jobs:\s*(?:#.*)?", line)]
    if len(jobs_markers) != 1:
        return set(), [f"cannot extract top-level jobs from validated workflow {name}"]
    jobs: set[str] = set()
    for line in lines[jobs_markers[0] + 1 :]:
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if not line.startswith(" "):
            break
        if match := JOB_RE.fullmatch(line):
            jobs.add(match.group(1))
    if not jobs:
        return set(), [f"cannot extract top-level jobs from validated workflow {name}"]
    return jobs, []


def validate_quarantines(
    policy: dict[str, object], today: dt.date, repo: Path
) -> list[str]:
    records = policy.get("quarantines", [])
    if not isinstance(records, list):
        return ["quarantine manifest must contain an array of records"]
    issues, identities = [], set()
    workflow_cache: dict[str, tuple[set[str], list[str]]] = {}
    for index, record in enumerate(records, 1):
        label = f"quarantine record {index}"
        if not isinstance(record, dict):
            issues.append(f"{label} must be a table")
            continue
        identity = _required_string(record, "check_identity", label, issues)
        scope = _required_string(record, "scope", label, issues)
        _required_string(record, "evidence", label, issues)
        justification = _required_string(record, "justification", label, issues)
        workflow = _required_string(record, "extended_workflow", label, issues)
        job = _required_string(record, "extended_job", label, issues)
        tracking = _required_string(record, "tracking", label, issues)
        if identity:
            if identity in identities:
                issues.append(f"duplicate quarantined check identity: {identity}")
            identities.add(identity)
            if any(character in identity for character in "*?[]"):
                issues.append(f"{label} check_identity must be exact, without wildcards")
        if scope and (
            any(character in scope for character in "*?[]")
            or scope.casefold() in {"all", "all checks", "all tests", "all platforms"}
        ):
            issues.append(f"{label} scope must name the narrowest affected surface")
        if justification and len(justification.split()) < 4:
            issues.append(f"{label} justification must be substantive")
        if tracking and not GITHUB_TRACKING_RE.fullmatch(tracking):
            issues.append(f"{label} tracking must be an exact GitHub issue or pull request URL")
        if workflow:
            jobs, workflow_issues = workflow_cache.setdefault(
                workflow, _workflow_jobs(repo, workflow)
            )
            issues.extend(f"{label}: {issue}" for issue in workflow_issues)
            if not workflow_issues and job and job not in jobs:
                issues.append(f"{label} extended_job does not exist in {workflow}: {job}")
        start = record.get("start_date")
        expiry = record.get("expiry_date")
        valid_start = type(start) is dt.date
        valid_expiry = type(expiry) is dt.date
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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--today", type=dt.date.fromisoformat)
    args = parser.parse_args(argv)
    repo = args.repo.resolve()
    issues: list[str] = []
    try:
        occurrences = inventory_ignores(repo)
    except (OSError, subprocess.CalledProcessError, UnicodeError) as error:
        occurrences = []
        issues.append(f"cannot inventory tracked Rust sources: {error}")
    policy, policy_issues = load_toml(repo / ".github/rust-test-policy.toml")
    issues.extend(policy_issues)
    if policy is not None:
        if policy.get("version") != 1:
            issues.append("rust test policy requires version = 1")
        else:
            issues.extend(validate_ignore_policy(occurrences, policy))
    quarantines, quarantine_issues = load_toml(repo / ".github/quarantined-checks.toml")
    issues.extend(quarantine_issues)
    if quarantines is not None:
        if quarantines.get("version") != 1:
            issues.append("quarantine manifest requires version = 1")
        else:
            issues.extend(validate_quarantines(quarantines, args.today or dt.date.today(), repo))
    if issues:
        print("Rust test signal policy failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
        return 1
    quarantine_count = len(quarantines.get("quarantines", []))
    print(f"Rust test signal policy passed: {len(occurrences)} ignore occurrences; {quarantine_count} active quarantines")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
