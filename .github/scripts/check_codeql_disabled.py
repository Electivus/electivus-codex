#!/usr/bin/env python3
"""Fail when repository automation reintroduces CodeQL authority."""

import argparse
import re
import sys
from pathlib import Path

POLICY_CHECK_COMMAND = "python3 .github/scripts/check_codeql_disabled.py"
POLICY_CHECK_LINE = f"run: {POLICY_CHECK_COMMAND}"
SECURITY_EVENTS_PERMISSION = re.compile(
    r"(?:^\s*|[,{]\s*)[\"']?security-events[\"']?\s*:", re.MULTILINE
)
CODE_SCANNING_AUTHORITY = re.compile(r"code[_ -]?scanning", re.IGNORECASE)


def validate_workflows(sources: dict[str, str]) -> list[str]:
    issues = []
    for path, source in sorted(sources.items()):
        workflow_name = Path(path).name.casefold()
        policy_source = "\n".join(
            "" if line.strip() == POLICY_CHECK_LINE else line
            for line in source.splitlines()
        )
        normalized = policy_source.casefold()
        if "codeql" in workflow_name:
            issues.append(f"CodeQL workflow name: {path}")
        if "github/codeql-action/" in normalized:
            issues.append(f"CodeQL action: {path}")
        elif "codeql" in normalized:
            issues.append(f"CodeQL reference: {path}")
        if SECURITY_EVENTS_PERMISSION.search(policy_source):
            issues.append(f"security-events permission: {path}")
        if CODE_SCANNING_AUTHORITY.search(policy_source):
            issues.append(f"code-scanning authority: {path}")
    return issues


def load_automation_sources(repo: Path) -> dict[str, str]:
    workflow_dir = repo / ".github" / "workflows"
    action_dir = repo / ".github" / "actions"
    paths = sorted(
        (
            *workflow_dir.glob("*.yml"),
            *workflow_dir.glob("*.yaml"),
            *action_dir.glob("**/action.yml"),
            *action_dir.glob("**/action.yaml"),
        )
    )
    return {
        path.relative_to(repo).as_posix(): path.read_text(encoding="utf-8")
        for path in paths
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    try:
        issues = validate_workflows(load_automation_sources(repo))
    except (OSError, UnicodeError) as error:
        print(
            f"disabled code-scanning policy could not read automation sources: {error}",
            file=sys.stderr,
        )
        return 1
    if issues:
        print(
            "disabled code-scanning policy failed:\n"
            + "\n".join(f"- {issue}" for issue in issues),
            file=sys.stderr,
        )
        return 1
    print(
        "disabled code-scanning policy passed: repository automation contains no CodeQL authority"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
