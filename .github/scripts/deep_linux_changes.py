#!/usr/bin/env python3

"""Classify whether a change is eligible for Deep Linux validation."""

import argparse
import subprocess
from dataclasses import dataclass
from fnmatch import fnmatchcase
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
# Unknown paths must stay eligible. Keep this allowlist deliberately narrow and
# limited to documentation and repository community metadata.
IRRELEVANT_PATH_PATTERNS = (
    ".github/CODEOWNERS",
    ".github/ISSUE_TEMPLATE/**",
    ".github/PULL_REQUEST_TEMPLATE/**",
    ".github/pull_request_template.md",
    "CODE_OF_CONDUCT.md",
    "CONTRIBUTING.md",
    "README.md",
    "SECURITY.md",
    "docs/**",
)


@dataclass(frozen=True)
class DeepLinuxDecision:
    eligible: bool
    reason: str


def changed_files(base: str, head: str, *, root: Path = ROOT) -> set[str]:
    output = subprocess.check_output(
        ["git", "diff", "--name-only", "--no-renames", f"{base}...{head}"],
        cwd=root,
        stderr=subprocess.PIPE,
    )
    return set(output.decode().splitlines())


def decision_for_event(
    event_name: str,
    *,
    base: str | None,
    head: str | None,
    root: Path = ROOT,
) -> DeepLinuxDecision:
    if event_name != "pull_request":
        reason_event_name = event_name or "unknown"
        return DeepLinuxDecision(
            eligible=True,
            reason=(
                f"{reason_event_name} event has no pull request comparison; "
                "Deep Linux remains eligible"
            ),
        )
    if not base or not head:
        return DeepLinuxDecision(
            eligible=True,
            reason=(
                "pull request comparison is missing base or head; "
                "Deep Linux remains eligible"
            ),
        )
    try:
        files = changed_files(base, head, root=root)
        return classify_changed_files(files)
    except Exception as error:
        return DeepLinuxDecision(
            eligible=True,
            reason=(
                f"pull request comparison failed ({type(error).__name__}); "
                "Deep Linux remains eligible"
            ),
        )


def classify_changed_files(changed_files: set[str]) -> DeepLinuxDecision:
    if not changed_files:
        return DeepLinuxDecision(
            eligible=True,
            reason="comparison returned no changed paths; Deep Linux remains eligible",
        )

    count = len(changed_files)
    noun = "path" if count == 1 else "paths"
    verb = "is" if count == 1 else "are"
    relevant_count = sum(
        not any(fnmatchcase(path, pattern) for pattern in IRRELEVANT_PATH_PATTERNS)
        for path in changed_files
    )
    if relevant_count == 0:
        return DeepLinuxDecision(
            eligible=False,
            reason=(
                f"all {count} changed {noun} {verb} explicitly irrelevant "
                "documentation or repository metadata"
            ),
        )
    if relevant_count != count:
        relevant_verb = "is" if relevant_count == 1 else "are"
        return DeepLinuxDecision(
            eligible=True,
            reason=(
                f"{relevant_count} of {count} changed paths {relevant_verb} not "
                "explicitly irrelevant"
            ),
        )
    return DeepLinuxDecision(
        eligible=True,
        reason=f"{count} changed {noun} {verb} not explicitly irrelevant",
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--event-name", required=True)
    parser.add_argument("--base")
    parser.add_argument("--head")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    decision = decision_for_event(
        args.event_name,
        base=args.base,
        head=args.head,
    )
    print(f"eligible={str(decision.eligible).lower()}")
    print(f"reason={decision.reason}")


if __name__ == "__main__":
    main()
