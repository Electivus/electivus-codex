#!/usr/bin/env python3
"""Fail when repository automation reintroduces CodeQL authority."""

import argparse
import os
import re
import sys
from pathlib import Path

import yaml
from yaml.nodes import MappingNode
from yaml.nodes import Node
from yaml.nodes import ScalarNode
from yaml.nodes import SequenceNode

POLICY_CHECK_COMMAND = (
    "uv run --project scripts --python python3 python "
    ".github/scripts/check_codeql_disabled.py"
)
POLICY_CHECK_LINE = f"run: {POLICY_CHECK_COMMAND}"
CODE_SCANNING_AUTHORITY = re.compile(r"code[_ -]?scanning", re.IGNORECASE)
SHELL_LINE_CONTINUATION = re.compile(r"\\\r?\n")
AUTOMATION_IMPLEMENTATION_ROOTS = (
    ".github/actions",
    ".github/scripts",
    "scripts",
)
AUTOMATION_IMPLEMENTATION_SUFFIXES = {
    ".cjs",
    ".js",
    ".mjs",
    ".ps1",
    ".py",
    ".sh",
    ".ts",
}
IGNORED_IMPLEMENTATION_DIRECTORIES = {".venv", "__pycache__"}
POLICY_IMPLEMENTATION_EXCLUSIONS = {
    ".github/scripts/check_codeql_disabled.py",
    ".github/scripts/test_check_codeql_disabled.py",
}


def _walk_yaml(root: Node | None) -> list[Node]:
    nodes = []
    pending = [root] if root is not None else []
    seen = set()
    while pending:
        node = pending.pop()
        if id(node) in seen:
            continue
        seen.add(id(node))
        nodes.append(node)
        if isinstance(node, MappingNode):
            for key, value in reversed(node.value):
                pending.extend((value, key))
        elif isinstance(node, SequenceNode):
            pending.extend(reversed(node.value))
    return nodes


def validate_workflows(sources: dict[str, str]) -> list[str]:
    issues = []
    for path, source in sorted(sources.items()):
        workflow_name = Path(path).name.casefold()
        policy_source = "\n".join(
            "" if line.strip() == POLICY_CHECK_LINE else line
            for line in source.splitlines()
        )
        if Path(path).suffix.casefold() not in {".yaml", ".yml"}:
            normalized = SHELL_LINE_CONTINUATION.sub("", policy_source.casefold())
            if "github/codeql-action/" in normalized:
                issues.append(f"CodeQL action: {path}")
            elif "codeql" in normalized:
                issues.append(f"CodeQL reference: {path}")
            if CODE_SCANNING_AUTHORITY.search(normalized):
                issues.append(f"code-scanning authority: {path}")
            continue
        try:
            nodes = _walk_yaml(yaml.compose(policy_source, Loader=yaml.SafeLoader))
        except yaml.YAMLError:
            issues.append(f"invalid YAML: {path}")
            continue
        scalars = [node.value for node in nodes if isinstance(node, ScalarNode)]
        normalized_scalars = [
            SHELL_LINE_CONTINUATION.sub("", scalar.casefold()) for scalar in scalars
        ]
        mapping_scalars = [
            (key.value.casefold(), value.value.casefold())
            for node in nodes
            if isinstance(node, MappingNode)
            for key, value in node.value
            if isinstance(key, ScalarNode) and isinstance(value, ScalarNode)
        ]
        if "codeql" in workflow_name:
            issues.append(f"CodeQL workflow name: {path}")
        if any("github/codeql-action/" in scalar for scalar in normalized_scalars):
            issues.append(f"CodeQL action: {path}")
        elif any("codeql" in scalar for scalar in normalized_scalars):
            issues.append(f"CodeQL reference: {path}")
        if any(key == "security-events" for key, _ in mapping_scalars):
            issues.append(f"security-events permission: {path}")
        if any(
            key == "permissions" and value == "write-all"
            for key, value in mapping_scalars
        ):
            issues.append(f"write-all permission: {path}")
        if any(CODE_SCANNING_AUTHORITY.search(scalar) for scalar in normalized_scalars):
            issues.append(f"code-scanning authority: {path}")
    return issues


def load_automation_sources(repo: Path) -> dict[str, str]:
    workflow_dir = repo / ".github" / "workflows"
    action_dir = repo / ".github" / "actions"
    paths = {
        *workflow_dir.glob("*.yml"),
        *workflow_dir.glob("*.yaml"),
        *action_dir.glob("**/action.yml"),
        *action_dir.glob("**/action.yaml"),
    }
    for relative_root in AUTOMATION_IMPLEMENTATION_ROOTS:
        implementation_root = repo / relative_root
        if not implementation_root.is_dir():
            continue
        for directory, child_directories, filenames in os.walk(implementation_root):
            child_directories[:] = sorted(
                name
                for name in child_directories
                if name not in IGNORED_IMPLEMENTATION_DIRECTORIES
            )
            for filename in filenames:
                path = Path(directory) / filename
                relative = path.relative_to(repo).as_posix()
                if (
                    path.suffix.casefold() in AUTOMATION_IMPLEMENTATION_SUFFIXES
                    and relative not in POLICY_IMPLEMENTATION_EXCLUSIONS
                ):
                    paths.add(path)
    return {
        path.relative_to(repo).as_posix(): path.read_text(encoding="utf-8")
        for path in sorted(paths)
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
