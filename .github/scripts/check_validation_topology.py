#!/usr/bin/env python3
"""Fail closed when the staged Validation authority topology drifts."""

import argparse
from pathlib import Path
import re
import sys


VALIDATION_WORKFLOWS = (
    ".github/workflows/validation-shadow.yml",
    ".github/workflows/validation-integrated.yml",
    ".github/workflows/validation-recovery.yml",
    ".github/workflows/validation-release-certification.yml",
    ".github/workflows/validation-surveillance.yml",
    ".github/workflows/validation-stability.yml",
    ".github/workflows/validation-cutover.yml",
    ".github/workflows/validation-retirement.yml",
    ".github/workflows/validation-comparison.yml",
)
BLOCKING_WORKFLOW = ".github/workflows/blocking-ci.yml"
PINNED_ACTION = re.compile(r"^\s*-?\s*uses:\s*(\S+)", re.MULTILINE)


def validate_topology(sources: dict[str, str]) -> list[str]:
    issues = []
    missing = [path for path in VALIDATION_WORKFLOWS if path not in sources]
    if missing:
        issues.append(f"missing staged Validation workflows: {', '.join(missing)}")
    for path, text in sources.items():
        for action in PINNED_ACTION.findall(text):
            if action.startswith("./") or action.startswith("docker://"):
                continue
            if "@" not in action or not re.fullmatch(r"[0-9a-f]{40}", action.rsplit("@", 1)[1]):
                issues.append(f"{path} contains an unpinned action: {action}")

    shadow = sources.get(".github/workflows/validation-shadow.yml", "")
    integrated = sources.get(".github/workflows/validation-integrated.yml", "")
    release = sources.get(".github/workflows/validation-release-certification.yml", "")
    stability = sources.get(".github/workflows/validation-stability.yml", "")
    retirement = sources.get(".github/workflows/validation-retirement.yml", "")
    comparison = sources.get(".github/workflows/validation-comparison.yml", "")
    blocking = sources.get(BLOCKING_WORKFLOW, "")
    if "cancel-in-progress: ${{ github.event_name == 'pull_request' }}" not in shadow:
        issues.append("Shadow must use latest-wins cancellation only for Pull requests")
    if "validation_emit_results.py" not in shadow or "if: ${{ always() }}" not in shadow:
        issues.append("Shadow must always aggregate every selected family")
    if (
        "cache_fallback:" not in shadow
        or '--cache-fallback "$CACHE_FALLBACK"' not in shadow
        or "disabled-reconstruction" not in shadow
    ):
        issues.append("Shadow must expose and emit an explicit cache-disabled reconstruction")
    if "upload: never" not in shadow or "security-events:" in shadow:
        issues.append("Shadow CodeQL must not publish security authority")
    if "group: validation-integrated-main" not in integrated or "cancel-in-progress: false" not in integrated:
        issues.append("Integrated must serialize the Certification lock without cancellation")
    if "--kind integrated" not in integrated or "--candidate \"$GITHUB_SHA\"" not in integrated:
        issues.append("Integrated must plan the exact triggering main SHA")
    if "linux-x64" not in release or "linux-arm64" not in release or "windows-x64" not in release or "windows-arm64" not in release:
        issues.append("Release certification must cover all four Product platforms")
    promote = release.split("\n  promote:", 1)[-1]
    if "cargo build" in promote or "repackage: true" in promote or "resign: true" in promote:
        issues.append("Release promotion must not rebuild, repackage, or resign")
    if "environment:\n      name: release-public" not in release:
        issues.append("public Release promotion requires its separate protected environment")
    if (
        "ordinary_run_ids" not in stability
        or "cache_disabled_run_id" not in stability
        or "validation_stability_input.py" not in stability
        or "records_json" in stability
        or "samples_json" in stability
    ):
        issues.append("Stability must derive bounded input from identified retained artifacts")
    if "SUPERSEDED_ISSUES" not in sources.get(".github/scripts/validation_retirement.py", ""):
        issues.append("retirement must retain explicit superseded issue backlinks")
    if "--legacy-manually-runnable" not in retirement:
        issues.append("retirement must keep the legacy graph manually runnable")
    if "validation_comparison.py" not in comparison or "legacy_run_id" not in comparison or "replacement_run_id" not in comparison:
        issues.append("comparison must use explicit legacy and replacement run identities")
    if blocking:
        required = blocking.split("\n  required:", 1)[-1]
        if "certification-lock:" not in blocking:
            issues.append("blocking CI must expose the Certification lock")
        if "\n      - certification-lock" not in required:
            issues.append("CI required must enforce the Certification lock")
    return issues


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    sources = {}
    try:
        for path in VALIDATION_WORKFLOWS:
            sources[path] = (repo / path).read_text(encoding="utf-8")
        sources[BLOCKING_WORKFLOW] = (repo / BLOCKING_WORKFLOW).read_text(encoding="utf-8")
        sources[".github/scripts/validation_retirement.py"] = (
            repo / ".github/scripts/validation_retirement.py"
        ).read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        print(f"Validation topology failed: cannot read sources: {error}", file=sys.stderr)
        return 1
    issues = validate_topology(sources)
    if issues:
        print("Validation topology failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
        return 1
    print("Validation topology passed: Shadow, Integrated, Recovery, Release, Surveillance, and retirement stages are explicit")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
