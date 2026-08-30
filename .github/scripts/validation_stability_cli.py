#!/usr/bin/env python3
"""Evaluate the finite hosted Stability certification input."""

import argparse
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_observability import LatencySample
from validation_stability import StabilityRecord
from validation_stability import validate_stability


def _records(payload: object) -> tuple[StabilityRecord, ...]:
    if not isinstance(payload, list):
        raise ContractError("Stability records must be an array")
    records = []
    for item in payload:
        if not isinstance(item, dict) or set(item) != {
            "candidateSha",
            "profile",
            "outcome",
            "retryCount",
            "cacheMode",
            "integratedSha",
        }:
            raise ContractError("Stability record has invalid fields")
        records.append(
            StabilityRecord(
                candidate_sha=item["candidateSha"],
                profile=item["profile"],
                outcome=item["outcome"],
                retry_count=item["retryCount"],
                cache_mode=item["cacheMode"],
                integrated_sha=item["integratedSha"],
            )
        )
    return tuple(records)


def _samples(payload: object) -> tuple[LatencySample, ...]:
    if not isinstance(payload, list):
        raise ContractError("Stability samples must be an array")
    samples = []
    for item in payload:
        if not isinstance(item, dict) or set(item) != {
            "candidateSha",
            "profile",
            "outcome",
            "firstActionableFailure",
            "mergeGate",
            "automatedMergeReadiness",
            "certificationRequired",
            "integratedCertification",
            "cacheMode",
        }:
            raise ContractError("Stability sample has invalid fields")
        samples.append(
            LatencySample(
                candidate_sha=item["candidateSha"],
                profile=item["profile"],
                outcome=item["outcome"],
                first_actionable_failure=item["firstActionableFailure"],
                merge_gate=item["mergeGate"],
                automated_merge_readiness=item["automatedMergeReadiness"],
                certification_required=item["certificationRequired"],
                integrated_certification=item["integratedCertification"],
                cache_mode=item["cacheMode"],
            )
        )
    return tuple(samples)


def run(args: argparse.Namespace) -> int:
    try:
        decision = validate_stability(
            _records(json.loads(args.records.read_text(encoding="utf-8"))),
            resulting_main_sha=args.resulting_main_sha,
            ordinary_samples=_samples(
                json.loads(args.samples.read_text(encoding="utf-8"))
            ),
        )
        payload = {
            "passed": decision.passed,
            "reason": decision.reason,
            "ordinarySloPassed": decision.ordinary_slo_passed,
            "automatedReadinessSloPassed": decision.automated_readiness_slo_passed,
            "cutoverEligible": decision.passed,
            "legacyAuthorityMustRemain": not decision.passed,
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(payload, indent=2))
        return 0 if decision.passed else 1
    except (ContractError, OSError, json.JSONDecodeError, ValueError) as error:
        print(f"Stability certification failed: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--records", type=Path, required=True)
    command.add_argument("--samples", type=Path, required=True)
    command.add_argument("--resulting-main-sha", required=True)
    command.add_argument("--output", type=Path, required=True)
    return command


def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
