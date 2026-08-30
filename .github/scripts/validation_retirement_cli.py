#!/usr/bin/env python3
"""Evaluate the evidence-based legacy validation retirement gate."""

import argparse
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_retirement import RetirementObservation
from validation_retirement import validate_retirement


def _boolean(value: str) -> bool:
    if value not in {"true", "false"}:
        raise ContractError(f"expected boolean input, got {value!r}")
    return value == "true"


def run(args: argparse.Namespace) -> int:
    try:
        decision = validate_retirement(
            RetirementObservation(
                cutover_at=args.cutover_at,
                now=args.now,
                eligible_merge_runs=args.eligible_merge_runs,
                release_certification_passed=args.release_certification_passed,
                protection_gap=args.protection_gap,
                state_authority_ambiguous=args.state_authority_ambiguous,
                rollback_required=args.rollback_required,
                legacy_manually_runnable=args.legacy_manually_runnable,
            )
        )
        payload = {
            "allowed": decision.allowed,
            "reason": decision.reason,
            "requiredBacklinkIssues": list(decision.required_backlink_issues),
            "legacyMustRemainRunnable": not decision.allowed,
            "automaticDeletion": False,
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(payload, indent=2))
        return 0 if decision.allowed else 1
    except (ContractError, OSError, ValueError) as error:
        print(f"Legacy retirement rejected: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--cutover-at", type=int, required=True)
    command.add_argument("--now", type=int, required=True)
    command.add_argument("--eligible-merge-runs", type=int, required=True)
    command.add_argument("--release-certification-passed", type=_boolean, required=True)
    command.add_argument("--protection-gap", type=_boolean, required=True)
    command.add_argument("--state-authority-ambiguous", type=_boolean, required=True)
    command.add_argument("--rollback-required", type=_boolean, required=True)
    command.add_argument("--legacy-manually-runnable", type=_boolean, required=True)
    command.add_argument("--output", type=Path, required=True)
    return command


def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
