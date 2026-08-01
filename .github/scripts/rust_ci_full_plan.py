#!/usr/bin/env python3
"""Plan the bounded Rust CI families for one validation scope."""

import argparse
from dataclasses import asdict
from dataclasses import dataclass
import json
import os
from pathlib import Path


@dataclass(frozen=True)
class LintLane:
    runner: str
    target: str
    profile: str


@dataclass(frozen=True)
class RustCiFullPlan:
    requested_scope: str
    resolved_scope: str
    reason: str
    lint_matrix: tuple[LintLane, ...]
    run_general: bool
    run_x64: bool
    run_arm64: bool
    retained_families: tuple[str, ...]


MERGE_GATE_LINT_MATRIX = (
    LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "dev"),
    LintLane("ubuntu-24.04", "x86_64-unknown-linux-gnu", "release"),
    LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "release"),
)
EXTENDED_LINT_MATRIX = (
    LintLane("ubuntu-24.04", "x86_64-unknown-linux-musl", "dev"),
    LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-musl", "dev"),
    LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "dev"),
    LintLane("ubuntu-24.04-arm", "aarch64-unknown-linux-musl", "release"),
)
FULL_LINT_MATRIX = (
    EXTENDED_LINT_MATRIX[0], MERGE_GATE_LINT_MATRIX[0], EXTENDED_LINT_MATRIX[1],
    EXTENDED_LINT_MATRIX[2], MERGE_GATE_LINT_MATRIX[2], EXTENDED_LINT_MATRIX[3],
    MERGE_GATE_LINT_MATRIX[1],
)
SCOPE_DEFINITIONS = {
    "merge-gate": (
        MERGE_GATE_LINT_MATRIX, False, True, False,
        ("x64 GNU dev lint/build", "x64 GNU release lint/build", "x64 musl release lint/build", "x64 nextest 4+1 PostgreSQL"),
    ),
    "extended": (
        EXTENDED_LINT_MATRIX, False, False, True,
        ("x64 musl dev lint/build", "ARM64 musl dev lint/build", "ARM64 GNU dev lint/build", "ARM64 musl release lint/build", "ARM64 nextest"),
    ),
    "full": (
        FULL_LINT_MATRIX, True, True, True,
        ("general formatting and benchmark", "cargo shear", "argument comment lint package", "argument comment lint prebuilt", "all seven lint/build lanes", "x64 nextest 4+1 PostgreSQL", "ARM64 nextest"),
    ),
}


def plan_for_scope(scope: str) -> RustCiFullPlan:
    requested_scope = scope
    if not scope:
        scope, reason = "full", "empty scope defaults to full"
    elif scope not in SCOPE_DEFINITIONS:
        scope, reason = "full", f"unknown scope '{requested_scope}' defaults fail-safe to full"
    else:
        reason = f"requested {scope} scope"
    lint_matrix, run_general, run_x64, run_arm64, families = SCOPE_DEFINITIONS[scope]
    return RustCiFullPlan(requested_scope, scope, reason, lint_matrix, run_general, run_x64, run_arm64, families)


def github_outputs(plan: RustCiFullPlan) -> dict[str, str]:
    compact = {"separators": (",", ":")}
    return {
        "resolved_scope": plan.resolved_scope,
        "reason": plan.reason,
        "lint_matrix": json.dumps([asdict(lane) for lane in plan.lint_matrix], **compact),
        "run_general": str(plan.run_general).lower(),
        "run_x64": str(plan.run_x64).lower(),
        "run_arm64": str(plan.run_arm64).lower(),
        "retained_families": json.dumps(plan.retained_families, **compact),
    }


def render_summary(plan: RustCiFullPlan) -> str:
    return (
        "## Rust CI full plan\n\n"
        f"- Requested scope: `{plan.requested_scope or '<empty>'}`\n- Resolved scope: `{plan.resolved_scope}`\n- Reason: {plan.reason}\n"
        f"- General families: `{str(plan.run_general).lower()}`\n- x64 nextest 4+1 PostgreSQL: `{str(plan.run_x64).lower()}`\n- ARM64 nextest: `{str(plan.run_arm64).lower()}`\n- Lint/build lanes: `{len(plan.lint_matrix)}`\n"
        f"- Retained families: {'; '.join(plan.retained_families)}\n"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scope", default=os.environ.get("VALIDATION_SCOPE", ""))
    plan = plan_for_scope(parser.parse_args(argv).scope)
    output = "".join(f"{name}={value}\n" for name, value in github_outputs(plan).items())
    if output_path := os.environ.get("GITHUB_OUTPUT"):
        with Path(output_path).open("a", encoding="utf-8") as stream:
            stream.write(output)
    else:
        print(output, end="")
    if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary_path).open("a", encoding="utf-8") as stream:
            stream.write(render_summary(plan))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
