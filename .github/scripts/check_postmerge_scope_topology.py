#!/usr/bin/env python3
"""Fail closed when scoped Full Rust and postmerge ownership drift."""

import argparse
import json
from pathlib import Path
import sys

from check_postgres_archive_topology import EXTENDED_LANES
from check_postgres_archive_topology import FULL_LANES
from check_postgres_archive_topology import MERGE_LANES
from check_postgres_archive_topology import _block
from check_postgres_archive_topology import _job
from check_postgres_archive_topology import _planner_matrix
from check_rust_test_policy import _workflow_jobs


SOURCES = (
    ".github/workflows/rust-ci-full.yml",
    ".github/workflows/postmerge-ci.yml",
    ".github/workflows/blocking-ci.yml",
    ".github/workflows/repo-checks.yml",
    ".github/scripts/rust_ci_full_plan.py",
    ".github/scripts/rust_ci_full_result.py",
    ".github/scripts/v8_canary_changes.py",
    ".github/ci-validation-inventory.json",
)
def _inventory_valid(source: str) -> bool:
    try:
        inventory = json.loads(source)
        rust_rows = inventory["rustCiFull"]
        v8_rows = inventory["v8"]
    except (json.JSONDecodeError, KeyError, TypeError):
        return False
    existing = {"general-format-benchmark": 2, "cargo-shear": 1, "argument-comment-lint-package": 1, "argument-comment-lint-prebuilt": 1}
    issue_86 = {"lint-x64-gnu-dev": 1, "tests-x64-nextest-postgres": 5}
    issue_87 = {"lint-x64-gnu-release": 1, "lint-x64-musl-release": 1}
    retained = {"lint-x64-musl-dev": 1, "lint-arm64-musl-dev": 1, "lint-arm64-gnu-dev": 1, "lint-arm64-musl-release": 1, "tests-arm64-nextest": 4}
    rust_expected = {name: ("promoted", "existing", ("full",), count) for name, count in existing.items()}
    rust_expected.update({name: ("promoted", "#86", ("merge-gate", "full"), count) for name, count in issue_86.items()})
    rust_expected.update({name: ("promoted", "#87", ("merge-gate", "full"), count) for name, count in issue_87.items()})
    rust_expected.update({name: ("retained", None, ("extended", "full"), count) for name, count in retained.items()})
    v8_ids = {"v8-x64-gnu-release", "v8-x64-gnu-ptrcomp-sandbox", "v8-arm64-gnu-release", "v8-arm64-gnu-ptrcomp-sandbox", "v8-x64-musl-release", "v8-x64-musl-ptrcomp-sandbox", "v8-arm64-musl-release", "v8-arm64-musl-ptrcomp-sandbox"}
    rust_actual = {row.get("id"): (row.get("disposition"), row.get("promotionSource"), tuple(row.get("activeScopes", ())), row.get("cardinality")) for row in rust_rows if isinstance(row, dict)}
    actual_v8_ids = {row.get("id") for row in v8_rows if isinstance(row, dict)}
    v8_metadata_valid = all(row.get("disposition") == "promoted" and row.get("promotionSource") == "#88" and row.get("activeScopes") == ["change-triggered", "manual"] for row in v8_rows if isinstance(row, dict))
    return (
        inventory.get("schemaVersion") == 1
        and len(rust_rows) == len(rust_actual) == len(rust_expected)
        and rust_actual == rust_expected
        and len(v8_rows) == len(actual_v8_ids) == 8
        and actual_v8_ids == v8_ids
        and v8_metadata_valid
        and inventory.get("outOfBoundary") == ["macOS", "Windows"]
    )


def validate_topology(rust: str, postmerge: str, blocking: str, repo_checks: str, planner: str, result_helper: str, v8_detector: str, inventory: str) -> list[str]:
    rust_on = _block(rust, r"^on:\s*$", r"^jobs:\s*$")
    plan = _job(rust, "plan")
    general_jobs = tuple(_job(rust, name) for name in ("general", "cargo_shear", "argument_comment_lint_package", "argument_comment_lint_prebuilt"))
    lint = _job(rust, "lint_build")
    x64 = _job(rust, "tests_linux_x64")
    arm64 = _job(rust, "tests_linux_arm64")
    rust_results = _job(rust, "results")
    post_rust = _job(postmerge, "rust-ci-full")
    post_results = _job(postmerge, "results")
    blocking_on = _block(blocking, r"^on:\s*$", r"^jobs:\s*$")
    merge_rust = _job(blocking, "deep-linux-cargo")
    checks = (
        ("blocking trigger ownership", "pull_request: {}" in blocking_on and "workflow_dispatch:" in blocking_on and "push:" not in blocking_on),
        ("planner workflow contract", "workflow_dispatch:" in rust_on and "default: full" in rust_on and "resolved_scope:" in rust_on and "rust_ci_full_plan.py" in plan and all(f"{name}: ${{{{ steps.scope.outputs.{name} }}}}" in plan for name in ("resolved_scope", "reason", "lint_matrix", "run_general", "run_x64", "run_arm64", "retained_families"))),
        ("exact merge lint plan", _planner_matrix(planner, "MERGE_GATE_LINT_MATRIX") == MERGE_LANES),
        ("exact Extended lint plan", _planner_matrix(planner, "EXTENDED_LINT_MATRIX") == EXTENDED_LANES),
        ("exact full lint plan", _planner_matrix(planner, "FULL_LINT_MATRIX") == FULL_LANES and set(FULL_LANES) == set(MERGE_LANES) | set(EXTENDED_LANES)),
        ("scoped general scheduling", all("needs: plan" in job and "needs.plan.result == 'success'" in job and "needs.plan.outputs.run_general == 'true'" in job for job in general_jobs)),
        ("scoped lint scheduling", "needs: plan" in lint and "needs.plan.result == 'success'" in lint and "include: ${{ fromJSON(needs.plan.outputs.lint_matrix) }}" in lint and "inputs.validation_scope" not in lint),
        ("scoped test scheduling", "needs.plan.outputs.run_x64 == 'true'" in x64 and "needs.plan.outputs.run_arm64 == 'true'" in arm64 and all("needs: plan" in job and "needs.plan.result == 'success'" in job for job in (x64, arm64))),
        ("merge-gate Cargo preserved", "validation_scope: merge-gate" in merge_rust and "postgres_contracts: true" in x64 and "test_threads: 1" in x64),
        ("full result fail closed", "needs:" in rust_results and all(f"{name}," in rust_results for name in ("plan", "general", "cargo_shear", "argument_comment_lint_package", "argument_comment_lint_prebuilt", "lint_build", "tests_linux_x64", "tests_linux_arm64")) and "if: always()" in rust_results and "rust_ci_full_result.py" in rust_results and all(f"${{{{ needs.{name}.result }}}}" in rust_results for name in ("plan", "general", "cargo_shear", "argument_comment_lint_package", "argument_comment_lint_prebuilt", "lint_build", "tests_linux_x64", "tests_linux_arm64"))),
        ("result helper exact states", "plan expected success" in result_helper and 'wanted = "success" if should_run else "skipped"' in result_helper and "actual != wanted" in result_helper and "return 0 if decision.success else 1" in result_helper and "Resolved scope" in result_helper and "Retained families" in result_helper),
        ("postmerge only Extended Rust", "validation_scope: extended" in post_rust and "uses: ./.github/workflows/v8-canary.yml" not in postmerge and "v8-canary:" not in postmerge and "needs:\n      - rust-ci-full" in post_results and "- v8-canary" not in post_results and "if: ${{ always() }}" in post_results and "check_ci_results.py" in post_results and "Extended" in post_results),
        ("V8 postmerge ownership removed", '".github/workflows/postmerge-ci.yml"' not in v8_detector and '".github/scripts/v8_canary_changes.py"' in v8_detector),
        ("validation inventory complete", _inventory_valid(inventory)),
        ("postmerge repository check", "python3 .github/scripts/check_postmerge_scope_topology.py" in repo_checks),
        ("planner fallback fail safe", "if not scope:" in planner and "empty scope defaults to full" in planner and "unknown scope" in planner and "defaults fail-safe to full" in planner),
    )
    return [f"Postmerge scope topology drift: {label}" for label, valid in checks if not valid]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    issues: list[str] = []
    for workflow in ("blocking-ci.yml", "postmerge-ci.yml"):
        _, workflow_issues = _workflow_jobs(repo, workflow)
        issues.extend(workflow_issues)
    try:
        sources = [(repo / path).read_text(encoding="utf-8") for path in SOURCES]
    except (OSError, UnicodeError) as error:
        issues.append(f"cannot read postmerge scope sources: {error}")
    else:
        issues.extend(validate_topology(*sources))
    if issues:
        print("Postmerge scope topology failed:\n" + "\n".join(f"- {issue}" for issue in issues), file=sys.stderr)
        return 1
    print("Postmerge scope topology passed: main runs only retained Extended Rust")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
