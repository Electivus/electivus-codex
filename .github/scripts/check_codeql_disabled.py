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
SHELL_QUOTE_TRANSLATION = str.maketrans("", "", "'\"")
AUTOMATION_IMPLEMENTATION_SUFFIXES = {
    ".bat",
    ".bazel",
    ".bzl",
    ".cjs",
    ".cmd",
    ".js",
    ".mk",
    ".mjs",
    ".ps1",
    ".py",
    ".sh",
    ".star",
    ".ts",
}
AUTOMATION_IMPLEMENTATION_FILENAMES = {
    "build",
    "dockerfile",
    "gnumakefile",
    "justfile",
    "makefile",
    "workspace",
}
IGNORED_IMPLEMENTATION_DIRECTORIES = {
    ".babysit-pr",
    ".git",
    ".venv",
    "__pycache__",
    "node_modules",
    "target",
}
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


def _normalize_automation_text(source: str) -> str:
    return SHELL_LINE_CONTINUATION.sub("", source.casefold()).translate(
        SHELL_QUOTE_TRANSLATION
    )


def validate_workflows(sources: dict[str, str]) -> list[str]:
    issues = []
    for path, source in sorted(sources.items()):
        workflow_name = Path(path).name.casefold()
        policy_source = "\n".join(
            "" if line.strip() == POLICY_CHECK_LINE else line
            for line in source.splitlines()
        )
        if Path(path).suffix.casefold() not in {".yaml", ".yml"}:
            normalized = _normalize_automation_text(policy_source)
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
        normalized_scalars = [_normalize_automation_text(scalar) for scalar in scalars]
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
    for directory, child_directories, filenames in os.walk(repo):
        child_directories[:] = sorted(
            name
            for name in child_directories
            if name not in IGNORED_IMPLEMENTATION_DIRECTORIES
        )
        for filename in filenames:
            path = Path(directory) / filename
            relative = path.relative_to(repo).as_posix()
            executable_text = False
            if path.stat().st_mode & 0o111:
                with path.open("rb") as source:
                    executable_text = b"\0" not in source.read(4096)
            if (
                path.suffix.casefold() in AUTOMATION_IMPLEMENTATION_SUFFIXES
                or path.name.casefold() in AUTOMATION_IMPLEMENTATION_FILENAMES
                or executable_text
            ) and relative not in POLICY_IMPLEMENTATION_EXCLUSIONS:
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
