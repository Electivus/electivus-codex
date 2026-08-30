#!/usr/bin/env python3
"""Create and aggregate Evidence manifests for the Validation plan."""

from dataclasses import dataclass, replace
import json
from pathlib import Path
from typing import Iterable

from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_contracts import EvidenceManifest
from validation_contracts import EvidenceRequirement
from validation_contracts import MANIFEST_CACHE_MODES
from validation_contracts import ValidationFingerprint
from validation_contracts import ValidationPlan
from validation_contracts import ValidationReport
from validation_contracts import RETENTION_DAYS
from validation_contracts import SCHEMA_VERSION
from validation_contracts import serialize_manifest
from validation_contracts import parse_manifest
from validation_contracts import validate_manifest
from validation_contracts import validate_plan


VALID_CACHE_MODES = MANIFEST_CACHE_MODES - {"cache-only"}
OUTCOME_PRIORITY = (
    "infrastructure-failure",
    "indeterminate",
    "product-failure",
    "stale",
    "passed",
)


@dataclass(frozen=True)
class Aggregation:
    report: ValidationReport
    actionable: bool


def _expiry(retention_class: str, created_at: int) -> int | None:
    days = RETENTION_DAYS[retention_class]
    if days is None or not created_at:
        return None
    return created_at + days * 86_400


def manifest_for_requirement(
    plan: ValidationPlan,
    requirement: EvidenceRequirement,
    *,
    outcome: str = "passed",
    producer: str = "validation-producer",
    reason: str = "producer completed",
    artifact_digests: tuple[tuple[str, str], ...] = (),
    duration_seconds: float = 0,
    critical_path_seconds: float | None = None,
    attempt: int = 1,
    cache_mode: str = "not-used",
    created_at: int = 0,
    candidate: CandidateIdentity | None = None,
    fingerprint: ValidationFingerprint | None = None,
) -> EvidenceManifest:
    validate_plan(plan)
    selected = requirement.selected
    if not selected:
        outcome = "not-required"
        producer = "validation-planner"
        reason = requirement.reason
    if selected and outcome == "not-required":
        raise ContractError("selected evidence cannot be emitted as not-required")
    if critical_path_seconds is None:
        critical_path_seconds = duration_seconds
    manifest = EvidenceManifest(
        schema_version=SCHEMA_VERSION,
        evidence_id=f"{plan.candidate.candidate_sha}:{requirement.family}",
        family=requirement.family,
        stage=requirement.stage,
        candidate=candidate or plan.candidate,
        producer=producer,
        outcome=outcome,
        disposition="required" if selected else "not-required",
        fingerprint=fingerprint or plan.fingerprint,
        artifact_digests=artifact_digests,
        retention_class=requirement.retention_class,
        duration_seconds=duration_seconds,
        critical_path_seconds=critical_path_seconds,
        reason=reason,
        attempt=attempt,
        cache_mode=cache_mode,
        created_at=created_at,
        expires_at=_expiry(requirement.retention_class, created_at),
    )
    validate_manifest(manifest)
    return manifest


def _stale(manifest: EvidenceManifest, reason: str) -> EvidenceManifest:
    return replace(manifest, outcome="stale", reason=reason)


def _outcome_for(evidence: Iterable[EvidenceManifest]) -> str:
    outcomes = {item.outcome for item in evidence if item.disposition == "required"}
    if not outcomes:
        return "not-required"
    for outcome in OUTCOME_PRIORITY:
        if outcome in outcomes:
            return outcome
    return "indeterminate"


def _required_by_family(plan: ValidationPlan) -> dict[str, EvidenceRequirement]:
    return {item.family: item for item in plan.requirements}


def _duration_summary(evidence: tuple[EvidenceManifest, ...]) -> tuple[tuple[str, float], ...]:
    selected = tuple(item for item in evidence if item.disposition == "required")

    def maximum(*stages: str) -> float:
        return max(
            (item.critical_path_seconds for item in selected if item.stage in stages),
            default=0,
        )

    first_failures = tuple(
        item.duration_seconds
        for item in selected
        if item.outcome not in {"passed", "not-required"}
    )
    return (
        (
            "firstActionableFailure",
            min(first_failures, default=maximum("preflight", "merge-gate", "codeql-shadow")),
        ),
        ("mergeGate", maximum("merge-gate", "preflight", "codeql-shadow")),
        ("automatedMergeReadiness", maximum("merge-gate", "codeql-shadow")),
        ("certificationRequired", maximum("certification-required")),
        ("integratedCertification", maximum("integrated")),
    )


def _validate_current_identity(
    manifest: EvidenceManifest,
    plan: ValidationPlan,
    current_candidate: CandidateIdentity | None,
    current_base_sha: str | None,
) -> str | None:
    expected = current_candidate or plan.candidate
    if manifest.candidate != expected:
        return "evidence candidate identity is stale"
    if current_base_sha is not None and manifest.candidate.base_sha != current_base_sha:
        return "evidence base identity is stale"
    if manifest.fingerprint != plan.fingerprint:
        return "evidence Validation fingerprint does not match the plan"
    return None


def aggregate(
    plan: ValidationPlan,
    manifests: Iterable[EvidenceManifest],
    *,
    current_candidate: CandidateIdentity | None = None,
    current_base_sha: str | None = None,
    now: int | None = None,
    state: str = "not-applicable",
    authorization_status: str = "not-applicable",
    cache_fallback: str = "not-applicable",
    next_actions: tuple[str, ...] = (),
) -> Aggregation:
    validate_plan(plan)
    if cache_fallback not in {
        "not-applicable",
        "not-used",
        "disabled-reconstruction",
    }:
        raise ContractError("unsupported cache fallback disposition")
    by_family: dict[str, EvidenceManifest] = {}
    errors: list[str] = list(plan.policy_errors)
    requirements = _required_by_family(plan)
    for manifest in manifests:
        validate_manifest(manifest)
        if manifest.family not in requirements:
            errors.append(f"evidence family is absent from the plan: {manifest.family}")
            continue
        if manifest.family in by_family:
            errors.append(f"duplicate evidence family: {manifest.family}")
            continue
        by_family[manifest.family] = manifest

    ordered: list[EvidenceManifest] = []
    for family, requirement in requirements.items():
        manifest = by_family.get(family)
        if manifest is None:
            if requirement.selected:
                errors.append(f"missing required evidence: {family}")
                ordered.append(
                    manifest_for_requirement(
                        plan,
                        requirement,
                        outcome="indeterminate",
                        producer="validation-aggregator",
                        reason="required producer did not emit a manifest",
                    )
                )
            else:
                ordered.append(manifest_for_requirement(plan, requirement))
            continue
        identity_error = _validate_current_identity(
            manifest, plan, current_candidate, current_base_sha
        )
        if identity_error:
            errors.append(f"{family}: {identity_error}")
            manifest = _stale(manifest, identity_error)
        if manifest.disposition != requirement.disposition:
            errors.append(f"evidence disposition contradicts plan: {family}")
            manifest = _stale(manifest, "evidence disposition contradicts the plan")
        if requirement.selected:
            if manifest.cache_mode not in VALID_CACHE_MODES:
                errors.append(f"unsupported cache mode for evidence: {family}")
                manifest = _stale(manifest, "cache metadata cannot be Validation evidence")
            if manifest.cache_mode == "cache-only":
                errors.append(f"cache-only result cannot be Validation evidence: {family}")
                manifest = _stale(manifest, "cache-only result is not evidence")
            if manifest.retention_class != requirement.retention_class:
                errors.append(f"{family}: retention class contradicts the plan")
                manifest = _stale(manifest, "retention class contradicts the plan")
            if now is not None and manifest.expires_at is not None and now >= manifest.expires_at:
                errors.append(f"{family}: evidence manifest has expired")
                manifest = _stale(manifest, "evidence manifest has expired")
        else:
            if manifest.outcome != "not-required":
                errors.append(f"not-required evidence has a producer outcome: {family}")
                manifest = _stale(manifest, "not-required evidence must not run")
        ordered.append(manifest)

    if cache_fallback == "disabled-reconstruction":
        reconstruction_count = sum(
            item.cache_mode == "disabled-reconstruction" for item in ordered
        )
        if reconstruction_count != 1:
            errors.append("cache-disabled reconstruction must occur exactly once")

    ordered_tuple = tuple(ordered)
    outcomes = tuple((item.family, item.outcome) for item in ordered_tuple)
    outcome = _outcome_for(ordered_tuple)
    if errors and outcome in {"passed", "product-failure"}:
        outcome = "indeterminate"
    required = tuple(item for item in ordered_tuple if item.disposition == "required")
    admission_allowed = not errors and bool(required) and all(
        item.outcome == "passed" for item in required
    )
    actions = list(next_actions)
    if not admission_allowed and not actions:
        actions.append("resolve every required evidence disposition and rerun on the exact candidate")
    if plan.profile == "certification-required":
        actions.append("retain this report for the 30-day Certification-required evidence class")
    report = ValidationReport(
        schema_version=SCHEMA_VERSION,
        candidate=plan.candidate,
        plan=plan,
        evidence=ordered_tuple,
        outcome=outcome,
        outcomes=outcomes,
        durations=_duration_summary(ordered_tuple),
        slo=(),
        state=state,
        admission_allowed=admission_allowed,
        next_actions=tuple(dict.fromkeys(actions)),
        errors=tuple(dict.fromkeys(errors)),
        cache_fallback=cache_fallback,
        authorization_status=authorization_status,
        artifacts=tuple(
            artifact
            for item in ordered_tuple
            for artifact in item.artifact_digests
        ),
    )
    return Aggregation(
        report=report,
        actionable=report.outcome
        not in {"passed", "not-required"},
    )


def load_manifests(directory: Path) -> tuple[EvidenceManifest, ...]:
    manifests = []
    families: set[str] = set()
    for path in sorted(directory.glob("*.json")):
        try:
            manifest = parse_manifest(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError, ContractError) as error:
            raise ContractError(f"cannot read evidence manifest {path}: {error}") from error
        if manifest.family in families:
            raise ContractError(f"duplicate evidence family in {directory}: {manifest.family}")
        families.add(manifest.family)
        manifests.append(manifest)
    return tuple(manifests)


def write_manifest(manifest: EvidenceManifest, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_manifest(manifest), encoding="utf-8")
