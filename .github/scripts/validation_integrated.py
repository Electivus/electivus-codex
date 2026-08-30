#!/usr/bin/env python3
"""Convert one complete Validation report into Integrated authority evidence."""

import argparse
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import EvidenceRequirement
from validation_contracts import ValidationReport
from validation_reports import parse_report
from validation_contracts import serialize_manifest
from validation_result import manifest_for_requirement


INTEGRATED_OUTCOMES = {
    "passed",
    "product-failure",
    "infrastructure-failure",
    "indeterminate",
    "stale",
}


def _integrated_manifest(report: ValidationReport):
    plan = report.plan
    candidate = report.candidate
    if candidate != plan.candidate or candidate.kind != "integrated":
        raise ContractError("Integrated report identity does not match its plan")
    outcome = report.outcome
    if outcome not in INTEGRATED_OUTCOMES:
        raise ContractError("Integrated report has an unsupported outcome")
    if outcome == "passed" and not report.admission_allowed:
        raise ContractError("a passed Integrated report must allow admission")
    duration = dict(report.durations).get("integratedCertification", 0)
    if isinstance(duration, bool) or not isinstance(duration, (int, float)) or duration < 0:
        raise ContractError("Integrated certification duration is invalid")
    errors = report.errors
    requirement = EvidenceRequirement(
        family="integrated-certification",
        stage="integrated",
        selected=True,
        disposition="required",
        reason="exact main commit was evaluated by the complete Validation graph",
        retention_class="integrated-certification",
    )
    attempt = max(
        (
            manifest.attempt
            for manifest in report.evidence
            if manifest.disposition == "required"
        ),
        default=1,
    )
    return manifest_for_requirement(
        plan,
        requirement,
        outcome=outcome,
        producer="integrated-certification",
        reason=(
            "complete Integrated certification passed"
            if outcome == "passed"
            else "; ".join(errors) or f"Integrated certification outcome: {outcome}"
        ),
        duration_seconds=float(duration),
        critical_path_seconds=float(duration),
        candidate=candidate,
        fingerprint=plan.fingerprint,
        attempt=attempt,
    )


def integrated_manifest(payload: object):
    if not isinstance(payload, dict):
        raise ContractError("Validation report must be an object")
    return _integrated_manifest(parse_report(json.dumps(payload, ensure_ascii=False)))


def run(args: argparse.Namespace) -> int:
    try:
        report = parse_report(args.report.read_text(encoding="utf-8"))
        manifest = _integrated_manifest(report)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(serialize_manifest(manifest), encoding="utf-8")
        return 0 if manifest.outcome == "passed" else 1
    except (ContractError, OSError, json.JSONDecodeError, ValueError) as error:
        print(f"Integrated certification emission failed: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--report", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    return command


def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
