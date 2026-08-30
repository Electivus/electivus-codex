#!/usr/bin/env python3
"""Fail closed if active repository workflows reintroduce CodeQL authority."""

from pathlib import Path
import re


WORKFLOW_DIRECTORY = Path(".github/workflows")
CODEQL_ACTION = re.compile(r"github/codeql-action/", re.IGNORECASE)
CODEQL_JOB_OR_STEP = re.compile(
    r"^\s*(?:codeql(?:[-_]\w*)?|[- ]+name:\s*.*\bcodeql\b)\s*:?\s*$",
    re.IGNORECASE | re.MULTILINE,
)
SECURITY_EVENTS_WRITE = re.compile(
    r"^\s*security-events:\s*write\s*$", re.IGNORECASE | re.MULTILINE
)


def validate_workflows(sources: dict[str, str]) -> list[str]:
    issues = []
    for path, text in sorted(sources.items()):
        if CODEQL_ACTION.search(text):
            issues.append(f"{path} reintroduces the CodeQL action")
        if CODEQL_JOB_OR_STEP.search(text):
            issues.append(f"{path} reintroduces a CodeQL job or step")
        if SECURITY_EVENTS_WRITE.search(text):
            issues.append(f"{path} grants code-scanning write authority")
    return issues


def main() -> int:
    sources = {
        str(path): path.read_text(encoding="utf-8")
        for path in WORKFLOW_DIRECTORY.glob("*.y*ml")
    }
    issues = validate_workflows(sources)
    if issues:
        for issue in issues:
            print(f"CodeQL policy violation: {issue}")
        return 1
    print("CodeQL policy: no active workflow grants scanning authority")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
