#!/usr/bin/env python3
"""Validate and print the PostgreSQL contract nextest selection."""

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import subprocess
import sys
import tomllib

from check_rust_test_policy import (
    IgnoreOccurrence,
    RustFunctionOccurrence,
    inventory_ignores,
    inventory_rust_test_functions,
    load_toml,
)


DB_REASON = 'ignore="requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"'
PROCESS_REASON = (
    'ignore="requires the PostgreSQL Runtime State process contract environment"'
)
CONTRACT_NAME_RE = re.compile(
    r"(?:postgres_contract_.*|state_(?:migrate|initialize)_process_.*)"
)
SPECIALIZED_CATEGORY = "specialized-environment"


@dataclass(frozen=True)
class InventorySummary:
    database_count: int
    process_count: int
    total_count: int
    packages: tuple[str, ...]
    nextest_filter: str


def _package_for(repo: Path, occurrence: IgnoreOccurrence) -> str:
    source = repo / occurrence.path
    for parent in source.parents:
        manifest = parent / "Cargo.toml"
        if manifest.is_file():
            with manifest.open("rb") as contents:
                package = tomllib.load(contents).get("package", {})
            name = package.get("name") if isinstance(package, dict) else None
            if isinstance(name, str):
                return name
    raise ValueError(f"no package manifest found for {occurrence.path}")


def _nextest_filter(packages: tuple[str, ...]) -> str:
    package_filter = " | ".join(f"package({package})" for package in packages)
    return (
        f"({package_filter}) & "
        "test(/(^|::)(postgres_contract_|state_(migrate|initialize)_process_)/)"
    )


def _category(classification: object) -> str | None:
    return classification.partition("|")[0] if isinstance(classification, str) else None


def validate_inventory(
    repo: Path,
    occurrences: list[IgnoreOccurrence],
    functions: list[RustFunctionOccurrence],
    policy: dict[str, object],
) -> tuple[InventorySummary | None, list[str]]:
    config = policy.get("postgres_contracts")
    if not isinstance(config, dict):
        return None, ["rust test policy field 'postgres_contracts' must be a table"]
    if set(config) != {"database_count", "process_count", "packages"}:
        return None, [
            "postgres_contracts must contain exactly database_count, process_count, and packages"
        ]
    database_count = config.get("database_count")
    process_count = config.get("process_count")
    configured_packages = config.get("packages")
    issues: list[str] = []
    for field, count in (("database_count", database_count), ("process_count", process_count)):
        if type(count) is not int or count < 0:
            issues.append(f"postgres_contracts {field} must be a nonnegative integer")
    if (
        not isinstance(configured_packages, list)
        or not configured_packages
        or any(not isinstance(package, str) or not package for package in configured_packages)
        or configured_packages != sorted(set(configured_packages))
    ):
        issues.append(
            "postgres_contracts packages must be a nonempty sorted list of unique package names"
        )
    records = policy.get("ignores")
    if not isinstance(records, dict):
        issues.append("rust test policy field 'ignores' must be a table")
    if issues:
        return None, issues
    expected_packages = tuple(configured_packages)
    inventory = {occurrence.identity: occurrence for occurrence in occurrences}
    database: list[IgnoreOccurrence] = []
    process: list[IgnoreOccurrence] = []
    for occurrence in sorted(occurrences):
        classification = records.get(occurrence.identity)
        category = _category(classification)
        accepted_reason = occurrence.attribute in {DB_REASON, PROCESS_REASON}
        accepted_name = CONTRACT_NAME_RE.fullmatch(occurrence.test) is not None
        if accepted_name and not accepted_reason:
            issues.append(
                f"{occurrence.identity}: PostgreSQL contract has an unsupported ignore reason"
            )
            continue
        if accepted_reason and not accepted_name:
            issues.append(
                f"{occurrence.identity}: PostgreSQL contract test name does not match the selection convention"
            )
            continue
        if not accepted_reason:
            continue
        (database if occurrence.attribute == DB_REASON else process).append(occurrence)
        if category is None:
            issues.append(f"unclassified PostgreSQL contract: {occurrence.identity}")
        elif category != SPECIALIZED_CATEGORY:
            issues.append(
                f"{occurrence.identity}: PostgreSQL contract category must be specialized-environment"
            )

    for identity, classification in records.items():
        category = _category(classification)
        parts = identity.rsplit("::", 2)
        recognized = len(parts) == 3 and (
            parts[2] in {DB_REASON, PROCESS_REASON}
            or CONTRACT_NAME_RE.fullmatch(parts[1]) is not None
        )
        if category == SPECIALIZED_CATEGORY and recognized and identity not in inventory:
            issues.append(f"stale specialized-environment classification: {identity}")

    mentioned_functions = {
        f"{occurrence.path}::{occurrence.test}"
        for occurrence in occurrences
        if CONTRACT_NAME_RE.fullmatch(occurrence.test)
    }
    for function in functions:
        if (
            CONTRACT_NAME_RE.fullmatch(function.name)
            and function.identity not in mentioned_functions
        ):
            issues.append(
                f"PostgreSQL test function lacks an accepted ignore: {function.identity}"
            )

    discovered_packages: set[str] = set()
    for occurrence in database + process:
        try:
            discovered_packages.add(_package_for(repo, occurrence))
        except (OSError, tomllib.TOMLDecodeError, ValueError) as error:
            issues.append(str(error))
    packages = tuple(sorted(discovered_packages))
    summary = InventorySummary(
        database_count=len(database),
        process_count=len(process),
        total_count=len(database) + len(process),
        packages=packages,
        nextest_filter=_nextest_filter(packages),
    )
    if summary.database_count != database_count:
        issues.append(
            "PostgreSQL database contract count changed: "
            f"expected {database_count}, found {summary.database_count}"
        )
    if summary.process_count != process_count:
        issues.append(
            "PostgreSQL process contract count changed: "
            f"expected {process_count}, found {summary.process_count}"
        )
    if packages != expected_packages:
        issues.append(
            "PostgreSQL contract packages changed: expected "
            f"{', '.join(expected_packages)}; found {', '.join(packages)}"
        )
    return (None, issues) if issues else (summary, [])


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--print-nextest-filter", action="store_true")
    parser.add_argument("--github-output", action="store_true")
    args = parser.parse_args(argv)
    repo = args.repo.resolve()
    policy, issues = load_toml(repo / ".github/rust-test-policy.toml")
    if policy is not None:
        try:
            occurrences = inventory_ignores(repo)
            functions = inventory_rust_test_functions(repo)
        except (OSError, subprocess.CalledProcessError, UnicodeError) as error:
            issues.append(f"cannot inventory tracked Rust sources: {error}")
        else:
            summary, validation_issues = validate_inventory(
                repo, occurrences, functions, policy
            )
            issues.extend(validation_issues)
    else:
        summary = None
    if issues:
        print(
            "PostgreSQL contract inventory failed:\n"
            + "\n".join(f"- {issue}" for issue in issues),
            file=sys.stderr,
        )
        return 1
    assert summary is not None
    if args.github_output:
        print(f"filter={summary.nextest_filter}")
        print(f"expected_total={summary.total_count}")
    elif args.print_nextest_filter:
        print(summary.nextest_filter)
    else:
        print(
            "PostgreSQL contract inventory passed: "
            f"{summary.database_count} database contracts + "
            f"{summary.process_count} process contracts = {summary.total_count} "
            f"across {len(summary.packages)} packages"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
