#!/usr/bin/env python3
"""Emit one attributable Evidence manifest from a validation producer."""

import argparse
from dataclasses import replace
import os
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import parse_plan
from validation_result import manifest_for_requirement
from validation_result import write_manifest


OUTCOME_BY_CONCLUSION = {
    "success": "passed",
    "failure": "product-failure",
    "cancelled": "stale",
    "skipped": "indeterminate",
    "timed_out": "infrastructure-failure",
}


def _artifact(value: str) -> tuple[str, str]:
    if "=" not in value:
        raise ContractError("artifact must use name=sha256 syntax")
    name, digest = value.split("=", 1)
    if not name or not digest:
        raise ContractError("artifact must use name=sha256 syntax")
    return name, digest


def _requirement(plan, family: str, stage: str | None, retention_class: str | None):
    try:
        requirement = next(item for item in plan.requirements if item.family == family)
    except StopIteration as error:
        raise ContractError(f"evidence family is not selected by the plan: {family}") from error
    if not requirement.selected:
        raise ContractError(f"cannot emit evidence for a not-required family: {family}")
    if stage is not None or retention_class is not None:
        requirement = replace(
            requirement,
            stage=stage or requirement.stage,
            retention_class=retention_class or requirement.retention_class,
        )
    return requirement


def emit(
    *,
    plan_path: Path,
    output_path: Path,
    family: str,
    outcome: str,
    producer: str,
    reason: str,
    stage: str | None,
    retention_class: str | None,
    artifacts: tuple[tuple[str, str], ...],
    duration_seconds: float,
    attempt: int,
    cache_mode: str,
    created_at: int,
) -> int:
    plan = parse_plan(plan_path.read_text(encoding="utf-8"))
    if outcome == "conclusion":
        raise ContractError("conclusion must be mapped before emit")
    requirement = _requirement(plan, family, stage, retention_class)
    manifest = manifest_for_requirement(
        plan,
        requirement,
        outcome=outcome,
        producer=producer,
        reason=reason,
        artifact_digests=artifacts,
        duration_seconds=duration_seconds,
        attempt=attempt,
        cache_mode=cache_mode,
        created_at=created_at,
    )
    write_manifest(manifest, output_path)
    output_file = os.environ.get("GITHUB_OUTPUT")
    if output_file:
        with Path(output_file).open("a", encoding="utf-8") as stream:
            stream.write(f"outcome={manifest.outcome}\nmanifest_path={output_path}\n")
    return 0


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--plan", required=True, type=Path)
    command.add_argument("--output", required=True, type=Path)
    command.add_argument("--family", required=True)
    command.add_argument("--outcome", choices=tuple({*OUTCOME_BY_CONCLUSION.values(), "not-required"}), default="passed")
    command.add_argument("--conclusion", choices=tuple(OUTCOME_BY_CONCLUSION), help="map a GitHub-style producer conclusion")
    command.add_argument("--producer", default="validation-producer")
    command.add_argument("--reason", default="producer completed")
    command.add_argument("--stage")
    command.add_argument("--retention-class")
    command.add_argument("--artifact", action="append", default=[])
    command.add_argument("--duration-seconds", type=float, default=0)
    command.add_argument("--attempt", type=int, default=1)
    command.add_argument("--cache-mode", default="not-used")
    command.add_argument("--created-at", type=int, default=0)
    return command


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        outcome = OUTCOME_BY_CONCLUSION[args.conclusion] if args.conclusion else args.outcome
        if outcome == "not-required":
            raise ContractError("a producer cannot emit a not-required manifest")
        return emit(
            plan_path=args.plan,
            output_path=args.output,
            family=args.family,
            outcome=outcome,
            producer=args.producer,
            reason=args.reason,
            stage=args.stage,
            retention_class=args.retention_class,
            artifacts=tuple(_artifact(value) for value in args.artifact),
            duration_seconds=args.duration_seconds,
            attempt=args.attempt,
            cache_mode=args.cache_mode,
            created_at=args.created_at,
        )
    except (ContractError, OSError, ValueError) as error:
        print(f"Validation evidence emission failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
