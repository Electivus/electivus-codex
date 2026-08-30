#!/usr/bin/env python3
"""Compare legacy and replacement validation observations for one candidate."""

import argparse
from dataclasses import dataclass
import json
import math
from pathlib import Path
from typing import Any

from validation_contracts import ContractError
from validation_contracts import ValidationReport
from validation_reports import parse_report
from validation_contracts import SHA1_PATTERN
from validation_contracts import SHA256_PATTERN


COMPARISON_SCHEMA_VERSION = 1
LEGACY_CHECK = "CI required"
MAX_EVIDENCE = 64


@dataclass(frozen=True)
class ValidationObservation:
    candidate_sha: str
    base_sha: str
    head_sha: str
    plan_fingerprint: str
    evidence: tuple[tuple[str, tuple[str, str]], ...]
    outcome: str
    check_name: str
    durations: tuple[tuple[str, float], ...]


@dataclass(frozen=True)
class ComparisonDecision:
    comparable: bool
    equivalent: bool
    differences: tuple[str, ...]
    latency_deltas: tuple[tuple[str, float, float], ...] = ()


def _string(payload: dict[str, Any], key: str) -> str:
    value = payload.get(key)
    if not isinstance(value, str) or not value:
        raise ContractError(f"observation.{key} must be a non-empty string")
    return value


def _sha(payload: dict[str, Any], key: str, *, allow_empty: bool = False) -> str:
    value = payload.get(key)
    if allow_empty and value == "":
        return value
    if not isinstance(value, str) or SHA1_PATTERN.fullmatch(value) is None:
        raise ContractError(f"observation.{key} must be a lowercase 40-character SHA")
    return value


def _durations(value: object) -> tuple[tuple[str, float], ...]:
    if not isinstance(value, dict):
        raise ContractError("observation.durations must be an object")
    result = []
    for name, duration in value.items():
        if not isinstance(name, str) or not name:
            raise ContractError("observation duration names must be strings")
        if (
            isinstance(duration, bool)
            or not isinstance(duration, (int, float))
            or not math.isfinite(duration)
            or duration < 0
        ):
            raise ContractError("observation durations must be non-negative numbers")
        result.append((name, float(duration)))
    return tuple(sorted(result))


def observation_from_dict(value: object) -> ValidationObservation:
    if not isinstance(value, dict):
        raise ContractError("validation observation must be an object")
    expected = {
        "candidateSha",
        "baseSha",
        "headSha",
        "planFingerprint",
        "evidence",
        "outcome",
        "checkName",
        "durations",
    }
    if set(value) not in {expected, expected | {"schemaVersion"}}:
        raise ContractError("validation observation has invalid fields")
    if "schemaVersion" in value and value["schemaVersion"] != COMPARISON_SCHEMA_VERSION:
        raise ContractError("unsupported validation comparison schema version")
    evidence_payload = value["evidence"]
    if not isinstance(evidence_payload, dict) or not evidence_payload:
        raise ContractError("validation observation requires evidence")
    if len(evidence_payload) > MAX_EVIDENCE:
        raise ContractError("validation observation evidence exceeds its item budget")
    evidence = []
    for family, item in evidence_payload.items():
        if not isinstance(family, str) or not family or not isinstance(item, dict):
            raise ContractError("validation observation evidence is malformed")
        if set(item) != {"disposition", "outcome"}:
            raise ContractError("validation observation evidence has invalid fields")
        disposition = item["disposition"]
        outcome = item["outcome"]
        if not isinstance(disposition, str) or not isinstance(outcome, str):
            raise ContractError("validation observation evidence values must be strings")
        evidence.append((family, (disposition, outcome)))
    plan_fingerprint = _string(value, "planFingerprint")
    if SHA256_PATTERN.fullmatch(plan_fingerprint) is None:
        raise ContractError("observation.planFingerprint must be a SHA-256")
    return ValidationObservation(
        candidate_sha=_sha(value, "candidateSha"),
        base_sha=_sha(value, "baseSha", allow_empty=True),
        head_sha=_sha(value, "headSha", allow_empty=True),
        plan_fingerprint=plan_fingerprint,
        evidence=tuple(sorted(evidence)),
        outcome=_string(value, "outcome"),
        check_name=_string(value, "checkName"),
        durations=_durations(value["durations"]),
    )


def compare(
    legacy: ValidationObservation,
    replacement: ValidationObservation,
) -> ComparisonDecision:
    differences = []
    if legacy.candidate_sha != replacement.candidate_sha:
        differences.append("candidate SHA differs")
    if legacy.base_sha != replacement.base_sha:
        differences.append("base SHA differs")
    if legacy.head_sha != replacement.head_sha:
        differences.append("head SHA differs")
    if legacy.plan_fingerprint != replacement.plan_fingerprint:
        differences.append("Validation fingerprint differs")
    if legacy.evidence != replacement.evidence:
        differences.append("evidence dispositions or outcomes differ")
    if legacy.outcome != replacement.outcome:
        differences.append("aggregate outcome differs")
    if legacy.check_name != LEGACY_CHECK:
        differences.append("legacy observation does not use CI required")
    if replacement.check_name == LEGACY_CHECK:
        differences.append("replacement check collides with CI required")
    legacy_durations = dict(legacy.durations)
    replacement_durations = dict(replacement.durations)
    latency_deltas = tuple(
        (name, legacy_durations[name], replacement_durations[name])
        for name in sorted(legacy_durations.keys() & replacement_durations.keys())
        if legacy_durations[name] != replacement_durations[name]
    )
    if legacy_durations.keys() != replacement_durations.keys():
        differences.append("duration metric sets differ")
    return ComparisonDecision(
        comparable=True,
        equivalent=not differences,
        differences=tuple(differences),
        latency_deltas=latency_deltas,
    )


def _observation_from_report(
    report: ValidationReport, *, check_name: str
) -> ValidationObservation:
    identity = report.candidate
    plan = report.plan
    outcomes = dict(report.outcomes)
    evidence = {
        item.family: {
            "disposition": item.disposition,
            "outcome": outcomes[item.family],
        }
        for item in plan.requirements
    }
    observation = {
        "candidateSha": identity.candidate_sha,
        "baseSha": identity.base_sha or "",
        "headSha": identity.head_sha or "",
        "planFingerprint": plan.fingerprint.digest,
        "evidence": evidence,
        "outcome": report.outcome,
        "checkName": check_name,
        "durations": dict(report.durations),
    }
    return observation_from_dict(observation)


def observation_from_report(payload: object, *, check_name: str) -> ValidationObservation:
    if not isinstance(payload, dict):
        raise ContractError("Validation report must be an object")
    return _observation_from_report(
        parse_report(json.dumps(payload, ensure_ascii=False)),
        check_name=check_name,
    )


def _read(path: Path) -> ValidationObservation:
    try:
        return observation_from_dict(json.loads(path.read_text(encoding="utf-8")))
    except (OSError, json.JSONDecodeError) as error:
        raise ContractError(f"cannot read validation observation {path}: {error}") from error


def run(args: argparse.Namespace) -> int:
    try:
        if args.report is not None:
            report = parse_report(args.report.read_text(encoding="utf-8"))
            observation = _observation_from_report(report, check_name=args.check_name)
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(
                json.dumps(
                    {
                        "schemaVersion": COMPARISON_SCHEMA_VERSION,
                        "candidateSha": observation.candidate_sha,
                        "baseSha": observation.base_sha,
                        "headSha": observation.head_sha,
                        "planFingerprint": observation.plan_fingerprint,
                        "evidence": dict(
                            (family, {"disposition": values[0], "outcome": values[1]})
                            for family, values in observation.evidence
                        ),
                        "outcome": observation.outcome,
                        "checkName": observation.check_name,
                        "durations": dict(observation.durations),
                    },
                    indent=2,
                )
                + "\n",
                encoding="utf-8",
            )
            return 0
        decision = compare(_read(args.legacy), _read(args.replacement))
        payload = {
            "schemaVersion": COMPARISON_SCHEMA_VERSION,
            "comparable": decision.comparable,
            "equivalent": decision.equivalent,
            "differences": list(decision.differences),
            "latencyDeltas": {
                name: {
                    "legacySeconds": legacy,
                    "replacementSeconds": replacement,
                }
                for name, legacy, replacement in decision.latency_deltas
            },
            "nextAction": (
                "retain legacy authority until the comparison is equivalent"
                if not decision.equivalent
                else "comparison passed; continue the finite Stability contract"
            ),
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(payload, indent=2))
        return 0 if decision.equivalent else 1
    except (ContractError, OSError, json.JSONDecodeError, ValueError) as error:
        print(f"Validation comparison failed: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--legacy", type=Path)
    command.add_argument("--replacement", type=Path)
    command.add_argument("--report", type=Path)
    command.add_argument("--check-name", default="Validation Shadow")
    command.add_argument("--output", type=Path, required=True)
    return command


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    if args.report is None and (args.legacy is None or args.replacement is None):
        parser().error("--legacy and --replacement are required unless --report is used")
    if args.report is not None and (args.legacy is not None or args.replacement is not None):
        parser().error("--report cannot be combined with --legacy or --replacement")
    return run(args)


if __name__ == "__main__":
    raise SystemExit(main())
