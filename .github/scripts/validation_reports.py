#!/usr/bin/env python3
"""Parsing, validation, and rendering for the shared Validation report."""

from validation_contracts import ContractError
from validation_contracts import EvidenceManifest
from validation_contracts import MAX_EVIDENCE
from validation_contracts import MAX_SERIALIZED_BYTES
from validation_contracts import OUTCOMES
from validation_contracts import SCHEMA_VERSION
from validation_contracts import ValidationReport
from validation_contracts import _keys
from validation_contracts import _load
from validation_contracts import _number
from validation_contracts import _object
from validation_contracts import _pairs
from validation_contracts import _serialized
from validation_contracts import _sha256
from validation_contracts import _text
from validation_contracts import _unique_strings
from validation_contracts import candidate_from_dict
from validation_contracts import candidate_to_dict
from validation_contracts import manifest_from_dict
from validation_contracts import manifest_to_dict
from validation_contracts import plan_from_dict
from validation_contracts import plan_to_dict
from validation_contracts import validate_candidate
from validation_contracts import validate_manifest
from validation_contracts import validate_plan


def report_from_dict(value: object) -> ValidationReport:
    payload = _object(value, "report")
    _keys(
        payload,
        {
            "schemaVersion",
            "identity",
            "plan",
            "evidence",
            "outcome",
            "outcomes",
            "durations",
            "slo",
            "fingerprints",
            "artifacts",
            "cacheFallback",
            "authorizationStatus",
            "state",
            "admissionAllowed",
            "nextActions",
            "errors",
        },
        "report",
    )
    if not isinstance(payload["schemaVersion"], int):
        raise ContractError("report.schemaVersion must be an integer")
    evidence_payload = payload["evidence"]
    if not isinstance(evidence_payload, list) or len(evidence_payload) > MAX_EVIDENCE:
        raise ContractError("report.evidence must be a bounded array")
    evidence = tuple(manifest_from_dict(item) for item in evidence_payload)

    outcomes_payload = _object(payload["outcomes"], "report.outcomes")
    outcomes = tuple(
        (_text(name, "report.outcomes.name"), _text(outcome, "report.outcomes.value"))
        for name, outcome in outcomes_payload.items()
    )
    durations_payload = _object(payload["durations"], "report.durations")
    durations = tuple(
        (_text(name, "report.durations.name"), _number(duration, "report.durations.value"))
        for name, duration in durations_payload.items()
    )
    slo_payload = _object(payload["slo"], "report.slo")
    slo = tuple(
        (_text(name, "report.slo.name"), _text(result, "report.slo.value", allow_empty=True))
        for name, result in slo_payload.items()
    )
    fingerprints = payload["fingerprints"]
    if not isinstance(fingerprints, list) or len(fingerprints) > MAX_EVIDENCE:
        raise ContractError("report.fingerprints must be a bounded array")
    fingerprint_values = tuple(
        _sha256(item, "report.fingerprints.value") for item in fingerprints
    )
    next_actions = _unique_strings(payload["nextActions"], "report.nextActions")
    errors = _unique_strings(payload["errors"], "report.errors")
    if not isinstance(payload["admissionAllowed"], bool):
        raise ContractError("report.admissionAllowed must be boolean")
    report = ValidationReport(
        schema_version=payload["schemaVersion"],
        candidate=candidate_from_dict(payload["identity"]),
        plan=plan_from_dict(payload["plan"]),
        evidence=evidence,
        outcome=_text(payload["outcome"], "report.outcome"),
        outcomes=outcomes,
        durations=durations,
        slo=slo,
        state=_text(payload["state"], "report.state"),
        admission_allowed=payload["admissionAllowed"],
        next_actions=next_actions,
        errors=errors,
        cache_fallback=_text(payload["cacheFallback"], "report.cacheFallback"),
        authorization_status=_text(
            payload["authorizationStatus"], "report.authorizationStatus"
        ),
        artifacts=_pairs(payload["artifacts"], "report.artifacts"),
    )
    validate_report(report)
    expected_fingerprints = tuple(item.fingerprint.digest for item in evidence)
    if fingerprint_values != expected_fingerprints:
        raise ContractError("report.fingerprints do not match its Evidence manifests")
    expected_artifacts = tuple(
        artifact for item in evidence for artifact in item.artifact_digests
    )
    if report.artifacts != expected_artifacts:
        raise ContractError("report.artifacts do not match its Evidence manifests")
    return report


def report_to_dict(report: ValidationReport) -> dict[str, object]:
    return {
        "schemaVersion": report.schema_version,
        "identity": candidate_to_dict(report.candidate),
        "plan": plan_to_dict(report.plan),
        "evidence": [manifest_to_dict(item) for item in report.evidence],
        "outcome": report.outcome,
        "outcomes": dict(report.outcomes),
        "durations": dict(report.durations),
        "slo": dict(report.slo),
        "fingerprints": [item.fingerprint.digest for item in report.evidence],
        "artifacts": [[key, value] for key, value in report.artifacts],
        "cacheFallback": report.cache_fallback,
        "authorizationStatus": report.authorization_status,
        "state": report.state,
        "admissionAllowed": report.admission_allowed,
        "nextActions": list(report.next_actions),
        "errors": list(report.errors),
    }


def validate_report(report: ValidationReport) -> None:
    if report.schema_version != SCHEMA_VERSION:
        raise ContractError("unsupported Validation report version")
    validate_candidate(report.candidate)
    validate_plan(report.plan)
    if report.plan.candidate != report.candidate:
        raise ContractError("report and plan candidates disagree")
    if len(report.evidence) > MAX_EVIDENCE:
        raise ContractError("report.evidence exceeds its item budget")
    requirements = {item.family: item for item in report.plan.requirements}
    evidence_by_family: dict[str, EvidenceManifest] = {}
    for item in report.evidence:
        validate_manifest(item)
        if item.family not in requirements:
            raise ContractError("report contains an Evidence family absent from its plan")
        if item.family in evidence_by_family:
            raise ContractError("report contains a duplicate Evidence family")
        if item.disposition != requirements[item.family].disposition:
            raise ContractError("report Evidence disposition contradicts its plan")
        evidence_by_family[item.family] = item
    if set(evidence_by_family) != set(requirements):
        raise ContractError("report must contain every planned Evidence family")
    if report.outcome not in OUTCOMES:
        raise ContractError("report.outcome is unsupported")
    _pairs(list(report.outcomes), "report.outcomes")
    if dict(report.outcomes) != {
        family: item.outcome for family, item in evidence_by_family.items()
    }:
        raise ContractError("report.outcomes do not match its Evidence manifests")
    for name, duration in report.durations:
        _text(name, "report.durations.name")
        _number(duration, "report.durations.value")
    _pairs(list(report.slo), "report.slo")
    _text(report.state, "report.state")
    if not isinstance(report.admission_allowed, bool):
        raise ContractError("report.admissionAllowed must be boolean")
    _unique_strings(report.next_actions, "report.nextActions")
    _unique_strings(report.errors, "report.errors")
    _text(report.cache_fallback, "report.cacheFallback")
    _text(report.authorization_status, "report.authorizationStatus")
    if report.authorization_status not in {
        "not-applicable",
        "required",
        "validated",
        "consumed",
        "invalidated",
    }:
        raise ContractError("report.authorizationStatus is unsupported")
    for name, digest in report.artifacts:
        _text(name, "report.artifacts.name")
        _sha256(digest, "report.artifacts.digest")
    if report.admission_allowed and (
        report.outcome != "passed"
        or report.errors
        or any(
            item.disposition == "required" and item.outcome != "passed"
            for item in report.evidence
        )
    ):
        raise ContractError("admitted reports must be error-free and fully passed")


def parse_report(text: str) -> ValidationReport:
    return report_from_dict(_load(text, "Validation report"))


def serialize_report(report: ValidationReport) -> str:
    validate_report(report)
    return _serialized(report_to_dict(report), "Validation report")


def render_report(report: ValidationReport) -> str:
    validate_report(report)
    selected = ", ".join(
        item.family for item in report.plan.requirements if item.selected
    ) or "none"
    lines = [
        "# Validation report",
        "",
        f"- Candidate: `{report.candidate.candidate_sha}`",
        f"- Base: `{report.candidate.base_sha or 'not-applicable'}`",
        f"- Profile: `{report.plan.profile}`",
        f"- Change surfaces: `{', '.join(report.plan.surfaces)}`",
        f"- Risk modifiers: `{', '.join(report.plan.risk_modifiers) or 'none'}`",
        f"- Selected evidence: `{selected}`",
        f"- Outcome: `{report.outcome}`",
        f"- State: `{report.state}`",
        f"- Admission allowed: `{str(report.admission_allowed).lower()}`",
        f"- Cache fallback: `{report.cache_fallback}`",
        "",
        "## Evidence dispositions",
        "",
    ]
    for requirement in report.plan.requirements:
        outcome = dict(report.outcomes).get(requirement.family, "missing")
        lines.append(
            f"- `{requirement.family}`: `{requirement.disposition}` / `{outcome}` — {requirement.reason}"
        )
    lines.extend(("", "## Next actions", ""))
    lines.extend(f"- {action}" for action in report.next_actions or ("none",))
    if report.errors:
        lines.extend(("", "## Errors", ""))
        lines.extend(f"- {error}" for error in report.errors)
    text = "\n".join(lines) + "\n"
    if len(text.encode("utf-8")) > MAX_SERIALIZED_BYTES:
        raise ContractError("Validation report exceeds its serialized byte budget")
    return text
