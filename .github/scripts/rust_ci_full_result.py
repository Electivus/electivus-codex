#!/usr/bin/env python3
"""Validate scoped Rust CI child results without accepting ambiguous states."""

from dataclasses import dataclass
import os
from pathlib import Path

from rust_ci_full_plan import VALIDATION_SCOPES
from rust_ci_full_plan import plan_for_scope
from rust_ci_full_plan import safe_scope_label


@dataclass(frozen=True)
class ChildResults:
    general: str
    cargo_shear: str
    argument_comment_lint_package: str
    argument_comment_lint_prebuilt: str
    argument_comment_lint_windows: str
    lint_build: str
    tests_linux_x64: str
    tests_linux_arm64: str
    tests_windows_x64: str
    tests_windows_arm64: str


@dataclass(frozen=True)
class RustCiFullDecision:
    success: bool
    issues: tuple[str, ...]


CHILDREN = ("general", "cargo_shear", "argument_comment_lint_package", "argument_comment_lint_prebuilt", "argument_comment_lint_windows", "lint_build", "tests_linux_x64", "tests_linux_arm64", "tests_windows_x64", "tests_windows_arm64")


def expected_children(resolved_scope: str) -> dict[str, bool]:
    plan = plan_for_scope(resolved_scope)
    expected = (
        (plan.run_general,) * 4
        + (plan.run_windows_x64, True)
        + (
            plan.run_linux_x64,
            plan.run_linux_arm64,
            plan.run_windows_x64,
            plan.run_windows_arm64,
        )
    )
    return dict(zip(CHILDREN, expected, strict=True))


def evaluate_results(resolved_scope: str, plan_result: str, results: ChildResults) -> RustCiFullDecision:
    if resolved_scope not in VALIDATION_SCOPES:
        return RustCiFullDecision(False, (f"resolved scope is invalid: {safe_scope_label(resolved_scope) or '<empty>'}",))
    expected = expected_children(resolved_scope)
    issues = []
    if plan_result != "success":
        issues.append(f"plan expected success, got {plan_result}")
    for name, should_run in expected.items():
        actual = getattr(results, name)
        wanted = "success" if should_run else "skipped"
        if actual != wanted:
            issues.append(f"{name} expected {wanted}, got {actual}")
    return RustCiFullDecision(success=not issues, issues=tuple(issues))


def render_result_summary(resolved_scope: str, results: ChildResults, decision: RustCiFullDecision) -> str:
    if resolved_scope not in VALIDATION_SCOPES:
        issues = "\n".join(f"- Issue: {issue}" for issue in decision.issues)
        return f"## Rust CI full result\n\n- Resolved scope: `<invalid>`\n- Outcome: `failure`\n{issues}\n"
    plan = plan_for_scope(resolved_scope)
    expected = expected_children(resolved_scope)
    lines = ["## Rust CI full result", "", f"- Resolved scope: `{plan.resolved_scope}`", f"- Selected families: {'; '.join(plan.selected_families)}", "", "| Child | Expected | Actual |", "| --- | --- | --- |"]
    for name, should_run in expected.items():
        wanted = "success" if should_run else "skipped"
        lines.append(f"| `{name}` | `{wanted}` | `{getattr(results, name)}` |")
    lines.extend(("", f"- Outcome: `{'success' if decision.success else 'failure'}`"))
    lines.extend(f"- Issue: {issue}" for issue in decision.issues)
    return "\n".join(lines) + "\n"


def main() -> int:
    resolved_scope, plan_result = os.environ.get("RESOLVED_SCOPE", ""), os.environ.get("PLAN_RESULT", "")
    results = ChildResults(*(os.environ.get(f"{name.upper()}_RESULT", "") for name in CHILDREN))
    decision = evaluate_results(resolved_scope, plan_result, results)
    summary = render_result_summary(resolved_scope, results, decision)
    if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary_path).open("a", encoding="utf-8") as output:
            output.write(summary)
    else:
        print(summary, end="")
    return 0 if decision.success else 1


if __name__ == "__main__":
    raise SystemExit(main())
