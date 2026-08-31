#!/usr/bin/env python3
"""Strict, bounded Validation report contracts for the repository-owned seam."""

from dataclasses import dataclass
import json
import ntpath
from itertools import islice
from typing import Any, Iterable

from validation_contracts import (
    CandidateIdentity,
    ContractError,
    candidate_from_dict,
    candidate_to_dict,
    decode_json_input,
    parse_json_integer,
    reject_json_constant,
    reject_json_duplicate,
    require_keys,
    require_object,
    serialize_json,
    validate_candidate,
    validate_non_negative_number,
    validate_sha256,
    validate_text,
)
from validation_evidence_contract import EvidenceManifest, manifest_from_dict
from validation_evidence_contract import (
    OUTCOMES,
    manifest_to_dict,
    validate_manifest_against_plan,
)
from validation_plan_contract import ValidationPlan, plan_from_dict, plan_to_dict
from validation_plan_contract import validate_plan


MAX_EVIDENCE = MAX_OUTCOMES = MAX_FINGERPRINTS = MAX_DURATIONS = MAX_ERRORS = 64
MAX_ARTIFACTS_PER_REPORT = 256
MAX_SERIALIZED_BYTES = 256_000
MAX_DURATION_SECONDS = 604_800
OUTCOME_PRIORITY = "infrastructure-failure indeterminate product-failure stale passed".split()
REPORT_FIELDS = frozenset("schemaVersion candidate plan evidence outcome outcomes durations fingerprints artifacts errors".split())
ARTIFACT_FIELDS = frozenset({"family", "name", "digest"})
DURATION_FIELDS = frozenset({"durationSeconds", "criticalPathSeconds"})


def _array(value: object, name: str, maximum: int) -> list[Any] | tuple[Any, ...]:
    if not isinstance(value, (list, tuple)):
        raise ContractError(f"{name} must be an array")
    if len(value) > maximum:
        raise ContractError(f"{name} exceeds its item budget")
    return value


def _unique_texts(value: object, name: str, maximum: int) -> tuple[str, ...]:
    result = tuple(validate_text(item, name) for item in _array(value, name, maximum))
    if len(set(result)) != len(result):
        raise ContractError(f"{name} must not contain duplicates")
    return result


def _path_name(value: object, name: str) -> str:
    artifact_name = validate_text(value, name)
    if (
        artifact_name.startswith(("/", "\\"))
        or ntpath.splitdrive(artifact_name)[0]
        or ".." in artifact_name.replace("\\", "/").split("/")
    ):
        raise ContractError(f"{name} contains an invalid path")
    return artifact_name


def _artifact_record(value: object, name: str) -> tuple[str, str, str]:
    payload = require_object(value, name)
    require_keys(payload, ARTIFACT_FIELDS, name)
    return (
        validate_text(payload["family"], f"{name}.family"),
        _path_name(payload["name"], f"{name}.name"),
        validate_sha256(payload["digest"], f"{name}.digest"),
    )


def _projection_pairs(
    value: object,
    name: str,
    maximum: int,
    parse_value,
) -> tuple[tuple[str, Any], ...]:
    result = []
    for pair in _array(value, name, maximum):
        if not isinstance(pair, tuple) or len(pair) != 2:
            raise ContractError(f"{name} must contain key/value pairs")
        key = validate_text(pair[0], f"{name}.name")
        result.append((key, parse_value(pair[1], f"{name}.{key}")))
    if len({key for key, _ in result}) != len(result):
        raise ContractError(f"{name} must not contain duplicate keys")
    return tuple(result)


def _outcome(value: object, name: str) -> str:
    value = validate_text(value, name)
    if value not in OUTCOMES:
        raise ContractError(f"{name} is unsupported")
    return value


def _duration(value: object, name: str) -> tuple[int | float, int | float]:
    if isinstance(value, dict):
        require_keys(value, DURATION_FIELDS, name)
        duration_value = value["durationSeconds"]
        critical_value = value["criticalPathSeconds"]
    elif isinstance(value, tuple) and len(value) == 2:
        duration_value, critical_value = value
    else:
        raise ContractError(f"{name} must contain duration and critical path")
    duration = validate_non_negative_number(
        duration_value, f"{name}.durationSeconds", maximum=MAX_DURATION_SECONDS
    )
    critical = validate_non_negative_number(
        critical_value, f"{name}.criticalPathSeconds", maximum=MAX_DURATION_SECONDS
    )
    if critical > duration:
        raise ContractError(f"{name}.criticalPathSeconds cannot exceed durationSeconds")
    return duration, critical


def _projection_from_dict(
    value: object,
    name: str,
    families: tuple[str, ...],
    maximum: int,
    parse_value,
) -> tuple[tuple[str, Any], ...]:
    payload = require_object(value, name)
    if len(payload) > maximum:
        raise ContractError(f"{name} exceeds its item budget")
    parsed = {
        validate_text(key, f"{name}.name"): parse_value(item, f"{name}.{key}")
        for key, item in payload.items()
    }
    if set(parsed) != set(families):
        raise ContractError(f"{name} must match the plan evidence families")
    return tuple((family, parsed[family]) for family in families)


def _validate_projection(
    value: object,
    name: str,
    maximum: int,
    expected: tuple[tuple[str, Any], ...],
    parse_value,
) -> None:
    if not isinstance(value, tuple):
        raise ContractError(f"{name} must be an immutable tuple")
    if _projection_pairs(value, name, maximum, parse_value) != expected:
        raise ContractError(f"{name} does not match its Evidence manifests")


@dataclass(frozen=True)
class ValidationReport:
    """Immutable, plan-bound projections of a complete evidence collection."""

    schema_version: int
    candidate: CandidateIdentity
    plan: ValidationPlan
    evidence: tuple[EvidenceManifest, ...]
    outcome: str
    outcomes: tuple[tuple[str, str], ...]
    durations: tuple[tuple[str, tuple[int | float, int | float]], ...]
    fingerprints: tuple[tuple[str, str], ...]
    artifacts: tuple[tuple[str, str, str], ...]
    errors: tuple[str, ...]


def _aggregate_outcome(
    evidence: tuple[EvidenceManifest, ...], errors: tuple[str, ...]
) -> str:
    if errors:
        return "indeterminate"
    selected = {item.outcome for item in evidence if item.disposition == "required"}
    if not selected:
        return "not-required"
    return next(outcome for outcome in OUTCOME_PRIORITY if outcome in selected)


def _validate_evidence(report: ValidationReport) -> tuple[str, ...]:
    if not isinstance(report.evidence, tuple):
        raise ContractError("report.evidence must be an immutable tuple")
    if len(report.evidence) > MAX_EVIDENCE:
        raise ContractError("report.evidence exceeds its item budget")
    requirements = report.plan.requirements
    families = tuple(item.family for item in requirements)
    if len(report.evidence) != len(requirements):
        raise ContractError("report.evidence must contain every planned family")
    seen: set[str] = set()
    for manifest, requirement in zip(report.evidence, requirements):
        if not isinstance(manifest, EvidenceManifest):
            raise ContractError("report.evidence contains an invalid manifest")
        validate_manifest_against_plan(manifest, report.plan)
        if manifest.family in seen:
            raise ContractError("report contains a duplicate Evidence family")
        seen.add(manifest.family)
        if manifest.family != requirement.family:
            raise ContractError("report.evidence is not in canonical plan order")
    if seen != set(families):
        raise ContractError("report must contain every planned Evidence family")
    return families


def _validate_artifacts(report: ValidationReport, families: tuple[str, ...]) -> None:
    if not isinstance(report.artifacts, tuple):
        raise ContractError("report.artifacts must be an immutable tuple")
    if len(report.artifacts) > MAX_ARTIFACTS_PER_REPORT:
        raise ContractError("report.artifacts exceeds its item budget")
    actual = []
    for record in report.artifacts:
        if not isinstance(record, tuple) or len(record) != 3:
            raise ContractError(
                "report.artifacts must contain family/name/digest records"
            )
        actual.append(
            _artifact_record(
                {"family": record[0], "name": record[1], "digest": record[2]},
                "report.artifacts",
            )
        )
    expected = tuple(
        (family, name, digest)
        for family, manifest in zip(families, report.evidence)
        for name, digest in manifest.artifact_digests
    )
    if tuple(actual) != expected:
        raise ContractError("report.artifacts do not match its Evidence manifests")


def _report_payload(report: ValidationReport) -> dict[str, object]:
    return {
        "schemaVersion": report.schema_version,
        "candidate": candidate_to_dict(report.candidate),
        "plan": plan_to_dict(report.plan),
        "evidence": [manifest_to_dict(item, report.plan) for item in report.evidence],
        "outcome": report.outcome,
        "outcomes": dict(report.outcomes),
        "durations": {
            family: {
                "durationSeconds": values[0],
                "criticalPathSeconds": values[1],
            }
            for family, values in report.durations
        },
        "fingerprints": dict(report.fingerprints),
        "artifacts": [
            {"family": family, "name": name, "digest": digest}
            for family, name, digest in report.artifacts
        ],
        "errors": list(report.errors),
    }


def _serialize_report(report: ValidationReport) -> str:
    return serialize_json(
        _report_payload(report),
        maximum_bytes=MAX_SERIALIZED_BYTES,
        label="Validation report",
    )


def validate_report(report: ValidationReport) -> None:
    if not isinstance(report, ValidationReport):
        raise ContractError("Validation report has an invalid structure")
    if type(report.schema_version) is not int or report.schema_version != 1:
        raise ContractError("unsupported Validation report version")
    if not isinstance(report.candidate, CandidateIdentity):
        raise ContractError("report.candidate has an invalid structure")
    validate_candidate(report.candidate)
    if not isinstance(report.plan, ValidationPlan):
        raise ContractError("report.plan has an invalid structure")
    validate_plan(report.plan)
    if report.candidate != report.plan.candidate:
        raise ContractError("report and plan candidates disagree")
    families = _validate_evidence(report)
    if not isinstance(report.errors, tuple):
        raise ContractError("report.errors must be an immutable tuple")
    errors = _unique_texts(report.errors, "report.errors", MAX_ERRORS)
    report_outcome = validate_text(report.outcome, "report.outcome")
    if report_outcome not in OUTCOMES:
        raise ContractError("report.outcome is unsupported")
    _validate_projection(
        report.outcomes,
        "report.outcomes",
        MAX_OUTCOMES,
        tuple(
            (family, item.outcome) for family, item in zip(families, report.evidence)
        ),
        _outcome,
    )
    _validate_projection(
        report.durations,
        "report.durations",
        MAX_DURATIONS,
        tuple(
            (family, (item.duration_seconds, item.critical_path_seconds))
            for family, item in zip(families, report.evidence)
        ),
        _duration,
    )
    expected_digest = report.plan.fingerprint.digest
    _validate_projection(
        report.fingerprints,
        "report.fingerprints",
        MAX_FINGERPRINTS,
        tuple((family, expected_digest) for family in families),
        validate_sha256,
    )
    _validate_artifacts(report, families)
    if not set(report.plan.policy_errors).issubset(errors):
        raise ContractError("report.errors must include plan policy errors")
    if report_outcome != _aggregate_outcome(report.evidence, errors):
        raise ContractError("report.outcome does not match its Evidence collection")
    _serialize_report(report)


def report_for_evidence(
    plan: ValidationPlan,
    evidence: Iterable[EvidenceManifest],
    *,
    errors: Iterable[str] = (),
) -> ValidationReport:
    """Derive every report projection from already-produced evidence objects."""

    validate_plan(plan)
    if isinstance(evidence, (str, bytes, dict)):
        raise ContractError("report.evidence must be an iterable of manifests")
    try:
        evidence_tuple = tuple(islice(evidence, MAX_EVIDENCE + 1))
    except TypeError as error:
        raise ContractError(
            "report.evidence must be an iterable of manifests"
        ) from error
    if len(evidence_tuple) > MAX_EVIDENCE:
        raise ContractError("report.evidence exceeds its item budget")
    if isinstance(errors, (str, bytes, dict)):
        raise ContractError("report.errors must be an iterable of strings")
    try:
        errors_tuple = tuple(islice(errors, MAX_ERRORS + 1))
    except TypeError as error:
        raise ContractError("report.errors must be an iterable of strings") from error
    errors_tuple = _unique_texts(
        (*plan.policy_errors, *errors_tuple), "report.errors", MAX_ERRORS
    )
    for manifest in evidence_tuple:
        if not isinstance(manifest, EvidenceManifest):
            raise ContractError("report.evidence contains an invalid manifest")
        validate_manifest_against_plan(manifest, plan)
    report = ValidationReport(
        schema_version=1,
        candidate=plan.candidate,
        plan=plan,
        evidence=evidence_tuple,
        outcome=_aggregate_outcome(evidence_tuple, errors_tuple),
        outcomes=tuple((item.family, item.outcome) for item in evidence_tuple),
        durations=tuple(
            (item.family, (item.duration_seconds, item.critical_path_seconds))
            for item in evidence_tuple
        ),
        fingerprints=tuple(
            (item.family, item.fingerprint.digest) for item in evidence_tuple
        ),
        artifacts=tuple(
            (item.family, name, digest)
            for item in evidence_tuple
            for name, digest in item.artifact_digests
        ),
        errors=errors_tuple,
    )
    validate_report(report)
    return report


def report_to_dict(report: ValidationReport) -> dict[str, object]:
    validate_report(report)
    return _report_payload(report)


def report_from_dict(value: object) -> ValidationReport:
    payload = require_object(value, "report")
    require_keys(payload, REPORT_FIELDS, "report")
    plan = plan_from_dict(payload["plan"])
    families = tuple(item.family for item in plan.requirements)
    evidence_payload = _array(payload["evidence"], "report.evidence", MAX_EVIDENCE)
    evidence = tuple(manifest_from_dict(item, plan) for item in evidence_payload)
    report = ValidationReport(
        schema_version=payload["schemaVersion"],
        candidate=candidate_from_dict(payload["candidate"]),
        plan=plan,
        evidence=evidence,
        outcome=validate_text(payload["outcome"], "report.outcome"),
        outcomes=_projection_from_dict(
            payload["outcomes"], "report.outcomes", families, MAX_OUTCOMES, _outcome
        ),
        durations=_projection_from_dict(
            payload["durations"],
            "report.durations",
            families,
            MAX_DURATIONS,
            _duration,
        ),
        fingerprints=_projection_from_dict(
            payload["fingerprints"],
            "report.fingerprints",
            families,
            MAX_FINGERPRINTS,
            validate_sha256,
        ),
        artifacts=tuple(
            _artifact_record(item, "report.artifacts")
            for item in _array(
                payload["artifacts"], "report.artifacts", MAX_ARTIFACTS_PER_REPORT
            )
        ),
        errors=_unique_texts(payload["errors"], "report.errors", MAX_ERRORS),
    )
    validate_report(report)
    return report


def parse_report(value: object) -> ValidationReport:
    text = decode_json_input(
        value, maximum_bytes=MAX_SERIALIZED_BYTES, label="Validation report"
    )
    try:
        payload = json.loads(
            text,
            object_pairs_hook=reject_json_duplicate,
            parse_constant=reject_json_constant,
            parse_int=parse_json_integer,
        )
    except ContractError:
        raise
    except (
        json.JSONDecodeError,
        RecursionError,
        TypeError,
        UnicodeDecodeError,
        ValueError,
    ) as error:
        raise ContractError(f"invalid Validation report JSON: {error}") from error
    report = report_from_dict(payload)
    if _serialize_report(report) != text:
        raise ContractError("Validation report is not canonically serialized")
    return report


def serialize_report(report: ValidationReport) -> str:
    validate_report(report)
    return _serialize_report(report)
