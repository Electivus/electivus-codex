#!/usr/bin/env python3
"""Run the auditable preflight for the one Validation authority cutover."""

import argparse
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_cutover import CutoverAuthorization
from validation_cutover import require_no_gap
from validation_cutover import validate_cutover


def _boolean(value: str) -> bool:
    if value not in {"true", "false"}:
        raise ContractError(f"expected boolean input, got {value!r}")
    return value == "true"


def run(args: argparse.Namespace) -> int:
    try:
        require_no_gap(args.old_authoritative, args.new_authoritative)
        decision = validate_cutover(
            CutoverAuthorization(
                branch=args.branch,
                current_candidate_sha=args.candidate_sha,
                legacy_check=args.legacy_check,
                replacement_check=args.replacement_check,
                default_codeql_authoritative=args.default_codeql_authoritative,
                advanced_codeql_ready=args.advanced_codeql_ready,
                code_quality_authoritative=args.code_quality_authoritative,
                stability_passed=args.stability_passed,
                fresh_authorization=args.fresh_authorization,
                authorization_id=args.authorization_id,
                authorized_by=args.authorized_by,
            )
        )
        payload = {
            "allowed": decision.allowed,
            "reason": decision.reason,
            "atomicOperations": list(decision.atomic_operations),
            "mutated": False,
            "nextAction": "apply the listed operations together through the protected administration surface",
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(payload, indent=2))
        return 0 if decision.allowed else 1
    except (ContractError, OSError, ValueError) as error:
        print(f"Validation cutover rejected: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--branch", default="main")
    command.add_argument("--candidate-sha", required=True)
    command.add_argument("--legacy-check", default="CI required")
    command.add_argument("--replacement-check", default="CI required")
    command.add_argument("--default-codeql-authoritative", type=_boolean, required=True)
    command.add_argument("--advanced-codeql-ready", type=_boolean, required=True)
    command.add_argument("--code-quality-authoritative", type=_boolean, required=True)
    command.add_argument("--stability-passed", type=_boolean, required=True)
    command.add_argument("--fresh-authorization", type=_boolean, required=True)
    command.add_argument("--old-authoritative", type=_boolean, required=True)
    command.add_argument("--new-authoritative", type=_boolean, required=True)
    command.add_argument("--authorization-id", required=True)
    command.add_argument("--authorized-by", required=True)
    command.add_argument("--output", type=Path, required=True)
    return command


def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
