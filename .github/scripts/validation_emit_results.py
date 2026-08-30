#!/usr/bin/env python3
"""Turn workflow conclusions into manifests and one terminal report."""

import argparse
from dataclasses import replace
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import parse_plan
from validation_reports import serialize_report
from validation_reports import render_report
from validation_result import aggregate
from validation_result import load_manifests
from validation_result import manifest_for_requirement
from validation_result import write_manifest


def _outcome(result: str) -> str:
    return {
        "success": "passed",
        "failure": "infrastructure-failure",
        "cancelled": "stale",
        "skipped": "indeterminate",
        "product-failure": "product-failure",
        "infrastructure-failure": "infrastructure-failure",
    }.get(result, "indeterminate")


WORKFLOW_CANDIDATES = {
    "repository-hygiene": ("preflight",),
    "codeql-advanced": ("codeql-result", "codeql-integrated", "codeql", "codeql-shadow"),
    "linux-x64-bazel": ("linux-x64-bazel", "bazel-full", "targeted"),
    "postgresql": ("postgresql",),
    "v8": ("v8",),
    "windows-x64": ("windows-x64", "windows"),
    "linux-x64-cargo": ("rust-full", "targeted"),
    "linux-arm64": ("rust-full", "targeted"),
    "linux-musl": ("rust-full", "targeted"),
    "rust-fast": ("rust-full", "targeted"),
    "code-quality": ("rust-full", "targeted"),
    "api-protocol-sdk": ("sdk", "targeted"),
    "release-packaging": ("targeted",),
    "synchronization-topology": ("targeted",),
}


def _workflow_result(results: dict[str, object], family: str) -> tuple[str, str]:
    for workflow_name in WORKFLOW_CANDIDATES.get(family, ("targeted",)):
        value = results.get(workflow_name)
        if isinstance(value, dict):
            result = value.get("result")
            if isinstance(result, str):
                outputs = value.get("outputs")
                if (
                    result == "failure"
                    and isinstance(outputs, dict)
                    and outputs.get("validation_outcome") == "product-failure"
                ):
                    return workflow_name, "product-failure"
                return workflow_name, result
    return "missing", "missing"


def emit(
    plan_path: Path,
    results_path: Path,
    output_dir: Path,
    *,
    current_base: str | None = None,
    now: int = 0,
    duration_seconds: float = 0,
    cache_fallback: str = "not-applicable",
    attempt: int = 1,
) -> int:
    plan = parse_plan(plan_path.read_text(encoding="utf-8"))
    results = json.loads(results_path.read_text(encoding="utf-8"))
    if not isinstance(results, dict):
        raise ContractError("workflow results must be an object")
    evidence_dir = output_dir / "evidence"
    evidence_dir.mkdir(parents=True, exist_ok=True)
    existing = {manifest.family: manifest for manifest in load_manifests(evidence_dir)}
    reconstruction_family = None
    if cache_fallback == "disabled-reconstruction":
        reconstruction_family = next(
            (requirement.family for requirement in plan.requirements if requirement.selected),
            None,
        )
        if reconstruction_family is None:
            raise ContractError(
                "cache-disabled reconstruction requires selected evidence"
            )
    manifests = []
    for requirement in plan.requirements:
        if not requirement.selected:
            manifests.append(manifest_for_requirement(plan, requirement))
            continue
        existing_manifest = existing.get(requirement.family)
        if existing_manifest is not None:
            if requirement.family == reconstruction_family:
                existing_manifest = replace(
                    existing_manifest, cache_mode="disabled-reconstruction"
                )
            manifests.append(existing_manifest)
            continue
        workflow_name, result = _workflow_result(results, requirement.family)
        manifest = manifest_for_requirement(
            plan,
            requirement,
            outcome=_outcome(result),
            producer=workflow_name,
            reason=f"workflow conclusion: {result}",
            duration_seconds=duration_seconds,
            critical_path_seconds=duration_seconds,
            attempt=attempt,
            cache_mode=(
                "disabled-reconstruction"
                if requirement.family == reconstruction_family
                else "not-used"
            ),
            created_at=now,
        )
        manifests.append(manifest)
        write_manifest(manifest, evidence_dir / f"{requirement.family}.json")
    result = aggregate(
        plan,
        manifests,
        current_base_sha=current_base,
        now=now or None,
        cache_fallback=cache_fallback,
    )
    if plan.candidate.kind == "integrated":
        state = {
            "passed": "clean",
            "product-failure": "recovery",
            "infrastructure-failure": "degraded",
            "indeterminate": "degraded",
            "stale": "degraded",
        }.get(result.report.outcome, "degraded")
        authorization_status = (
            "required" if state == "recovery" else "not-applicable"
        )
        result = replace(
            result,
            report=replace(
                result.report,
                state=state,
                authorization_status=authorization_status,
            ),
        )
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "validation-report.json").write_text(
        serialize_report(result.report), encoding="utf-8"
    )
    (output_dir / "validation-report.md").write_text(
        render_report(result.report), encoding="utf-8"
    )
    return 0 if result.report.admission_allowed else 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--plan", type=Path, required=True)
    command.add_argument("--results", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    command.add_argument("--current-base")
    command.add_argument("--now", type=int, default=0)
    command.add_argument("--duration-seconds", type=float, default=0)
    command.add_argument("--attempt", type=int, default=1)
    command.add_argument(
        "--cache-fallback",
        choices=("not-applicable", "not-used", "disabled-reconstruction"),
        default="not-applicable",
    )
    return command


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        return emit(
            args.plan,
            args.results,
            args.output,
            current_base=args.current_base,
            now=args.now,
            duration_seconds=args.duration_seconds,
            cache_fallback=args.cache_fallback,
            attempt=args.attempt,
        )
    except (ContractError, OSError, ValueError) as error:
        print(f"Validation result emission failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
