#!/usr/bin/env python3
"""Compute bounded Validation latency, reliability, and SLO reports."""

import argparse
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import ValidationReport
from validation_reports import parse_report
from validation_observability import LatencySample
from validation_observability import SLO_OBJECTIVES
from validation_observability import evaluate_slo
from validation_observability import reliability_counts
from validation_observability import render_slo
from validation_observability import validate_sample


def _sample_from_report(report: ValidationReport, path: Path) -> LatencySample:
    candidate_sha = report.candidate.candidate_sha
    profile = report.plan.profile
    outcome = report.outcome
    durations = dict(report.durations)
    values = {
        "first_actionable_failure": durations.get("firstActionableFailure", 0),
        "merge_gate": durations.get("mergeGate", 0),
        "automated_merge_readiness": durations.get("automatedMergeReadiness", 0),
        "certification_required": durations.get("certificationRequired", 0),
        "integrated_certification": durations.get("integratedCertification", 0),
    }
    if not isinstance(candidate_sha, str) or not isinstance(profile, str) or not isinstance(outcome, str):
        raise ContractError(f"Validation report {path} has invalid latency identity")
    cache_mode = report.cache_fallback
    if cache_mode == "not-applicable":
        cache_mode = "not-used"
    sample = LatencySample(candidate_sha, profile, outcome, cache_mode=cache_mode, **values)
    validate_sample(sample)
    return sample


def _sample(payload: object, path: Path) -> LatencySample:
    if not isinstance(payload, dict):
        raise ContractError(f"Validation report {path} must be an object")
    return _sample_from_report(
        parse_report(json.dumps(payload, ensure_ascii=False)),
        path,
    )


def load_samples(reports_dir: Path) -> tuple[LatencySample, ...]:
    samples = []
    for path in sorted(reports_dir.rglob("validation-report.json")):
        try:
            report = parse_report(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError, ContractError) as error:
            raise ContractError(f"cannot read Validation report {path}: {error}") from error
        samples.append(_sample_from_report(report, path))
    return tuple(samples)


def run(args: argparse.Namespace) -> int:
    samples = tuple(
        sample for sample in load_samples(args.reports_dir) if sample.profile == args.profile
    )
    previous = {}
    if args.previous is not None and args.previous.is_file():
        payload = json.loads(args.previous.read_text(encoding="utf-8"))
        if isinstance(payload, dict) and payload.get("profile") == args.profile:
            previous = payload.get("slo", {})
    evaluations = {}
    for metric, objective in SLO_OBJECTIVES.items():
        prior = previous.get(metric, {})
        prior_breached = isinstance(prior, dict) and prior.get("currentBreach") is True
        evaluations[metric] = render_slo(
            evaluate_slo(
                samples,
                metric=metric,
                objective_seconds=objective,
                previous_evaluation_breached=prior_breached,
            )
        )
    payload = {
        "schemaVersion": 1,
        "profile": args.profile,
        "sampleCount": len(samples),
        "reliability": dict(reliability_counts(samples)),
        "slo": evaluations,
        "retention": {
            "ordinary": "ordinary-pull-request:7d",
            "certification-required": "certification-required-pull-request:30d",
            "integrated": "integrated-certification:90d",
            "release-unpublished": "unpublished-release-candidate:30d",
            "release-published": "published-release:lifetime",
            "surveillance": "surveillance:30d",
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    lines = ["# Validation SLO report", "", f"- Samples observed: `{len(samples)}`", ""]
    lines.extend(
        f"- `{metric}`: p50 `{evaluation['p50Seconds']}`s / p95 `{evaluation['p95Seconds']}`s / "
        f"objective `{evaluation['objectiveSeconds']}`s / breached `{str(evaluation['breached']).lower()}`"
        for metric, evaluation in evaluations.items()
    )
    lines.extend(("", "## Reliability", ""))
    lines.extend(f"- `{outcome}`: {count}" for outcome, count in sorted(payload["reliability"].items()))
    args.markdown.parent.mkdir(parents=True, exist_ok=True)
    args.markdown.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return 1 if args.fail_on_breach and any(item["breached"] for item in evaluations.values()) else 0


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--reports-dir", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    command.add_argument("--markdown", type=Path, required=True)
    command.add_argument("--previous", type=Path)
    command.add_argument(
        "--profile",
        choices=("ordinary", "certification-required", "integrated", "release"),
        default="ordinary",
    )
    command.add_argument("--fail-on-breach", action="store_true")
    return command


def main(argv: list[str] | None = None) -> int:
    try:
        return run(parser().parse_args(argv))
    except (ContractError, OSError, json.JSONDecodeError, ValueError) as error:
        print(f"Validation observability failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
