#!/usr/bin/env python3
"""Aggregate producer manifests into one terminal Validation report."""

import argparse
from dataclasses import replace
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import parse_plan
from validation_reports import serialize_report
from validation_reports import render_report
from validation_result import aggregate
from validation_result import load_manifests


def run(args: argparse.Namespace) -> int:
    try:
        plan = parse_plan(args.plan.read_text(encoding="utf-8"))
        manifests = load_manifests(args.evidence)
        current_candidate = plan.candidate
        if args.current_candidate:
            current_candidate = replace(plan.candidate, candidate_sha=args.current_candidate)
        result = aggregate(
            plan,
            manifests,
            current_candidate=current_candidate,
            current_base_sha=args.current_base,
            now=args.now or None,
            state=args.state,
            cache_fallback=args.cache_fallback,
        )
        args.output.mkdir(parents=True, exist_ok=True)
        (args.output / "validation-report.json").write_text(
            serialize_report(result.report), encoding="utf-8"
        )
        (args.output / "validation-report.md").write_text(
            render_report(result.report), encoding="utf-8"
        )
        return 0 if result.report.admission_allowed else 1
    except (ContractError, OSError, ValueError) as error:
        print(f"Validation aggregation failed: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--plan", type=Path, required=True)
    command.add_argument("--evidence", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    command.add_argument("--current-candidate")
    command.add_argument("--current-base")
    command.add_argument("--now", type=int, default=0)
    command.add_argument("--state", default="not-applicable")
    command.add_argument(
        "--cache-fallback",
        choices=("not-applicable", "not-used", "disabled-reconstruction"),
        default="not-applicable",
    )
    return command


def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
