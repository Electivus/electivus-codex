#!/usr/bin/env python3
"""Bounded Evidence manifest contracts for the repository-owned seam."""

from dataclasses import dataclass
import json
import re

from validation_contracts import CandidateIdentity, ContractError, ValidationFingerprint
from validation_contracts import (
    _keys,
    _object,
    _text,
    candidate_from_dict,
    candidate_to_dict,
)
from validation_contracts import fingerprint_from_dict, fingerprint_to_dict
from validation_contracts import validate_candidate, validate_fingerprint
from validation_plan_contract import DISPOSITIONS, FAMILY_STAGES, RETENTION_CLASSES
from validation_plan_contract import EvidenceRequirement, ValidationPlan
from validation_plan_contract import (
    _input_text,
    _reject_constant,
    _reject_duplicate,
    _serialize_payload,
)
from validation_plan_contract import validate_plan, validate_requirement


MAX_ARTIFACTS_PER_MANIFEST = MAX_ATTEMPT = 64
MAX_DURATION_SECONDS = 604_800
MAX_JSON_INTEGER = 2**63 - 1
OUTCOMES = frozenset(
    "passed product-failure infrastructure-failure indeterminate stale not-required".split()
)
MANIFEST_CACHE_MODES = frozenset(
    "not-used cold cache-hit-verified disabled-reconstruction cache-only".split()
)
RETENTION_DAYS: dict[str, int | None] = dict(
    zip(
        "intra-run ordinary-pull-request certification-required-pull-request integrated-certification test-reactivation-certification unpublished-release-candidate published-release surveillance".split(),
        (1, 7, 30, 90, 90, 30, None, 30),
    )
)
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")
KNOWN_FAMILIES = frozenset(FAMILY_STAGES)
MANIFEST_FIELDS = frozenset(
    "schemaVersion evidenceId family stage candidate producer outcome disposition fingerprint artifactDigests retentionClass durationSeconds criticalPathSeconds reason attempt cacheMode createdAt expiresAt".split()
)
SENTINEL_FIELDS = (
    ("outcome", "not-required"),
    ("producer", None),
    ("artifact_digests", ()),
    ("duration_seconds", 0),
    ("critical_path_seconds", 0),
    ("attempt", 1),
    ("cache_mode", "not-used"),
)


def _invalid(condition: bool, message: str) -> None:
    if condition:
        raise ContractError(message)


def _wire_name(name: str) -> str:
    head, *tail = name.split("_")
    return head + "".join(part.title() for part in tail)


def _integer(value: object, name: str, *, minimum: int, maximum: int) -> int:
    if type(value) is not int or not minimum <= value <= maximum:
        raise ContractError(f"{name} is out of range")
    return value


def _number(value: object, name: str) -> int | float:
    _invalid(
        isinstance(value, bool) or not isinstance(value, (int, float)),
        f"{name} must be numeric",
    )
    _invalid(not 0 <= value <= MAX_DURATION_SECONDS, f"{name} is out of range")
    return value


def _artifact_pairs(value: object, name: str) -> tuple[tuple[str, str], ...]:
    _invalid(not isinstance(value, (list, tuple)), f"{name} must be an array")
    _invalid(len(value) > MAX_ARTIFACTS_PER_MANIFEST, f"{name} exceeds its item budget")
    result = []
    for pair in value:
        _invalid(
            not isinstance(pair, (list, tuple)) or len(pair) != 2,
            f"{name} has an invalid pair",
        )
        artifact_name = _text(pair[0], f"{name}.name")
        _invalid(
            artifact_name.startswith(("/", "\\"))
            or "\x00" in artifact_name
            or re.match(r"^[A-Za-z]:[\\/]", artifact_name)
            or ".." in artifact_name.replace("\\", "/").split("/"),
            f"{name}.name contains an invalid path",
        )
        digest = _text(pair[1], f"{name}.digest")
        _invalid(
            SHA256_PATTERN.fullmatch(digest) is None, f"{name}.digest is not SHA-256"
        )
        result.append((artifact_name, digest))
    _invalid(
        len({item[0] for item in result}) != len(result), f"{name} has duplicate names"
    )
    return tuple(result)


@dataclass(frozen=True)
class EvidenceManifest:
    schema_version: int
    evidence_id: str
    family: str
    stage: str
    candidate: CandidateIdentity
    producer: str | None
    outcome: str
    disposition: str
    fingerprint: ValidationFingerprint
    artifact_digests: tuple[tuple[str, str], ...]
    retention_class: str
    duration_seconds: int | float
    critical_path_seconds: int | float
    reason: str
    attempt: int = 1
    cache_mode: str = "not-used"
    created_at: int = 0
    expires_at: int | None = None


def manifest_to_dict(manifest: EvidenceManifest) -> dict[str, object]:
    payload = {_wire_name(key): value for key, value in vars(manifest).items()}
    payload["candidate"] = candidate_to_dict(manifest.candidate)
    payload["fingerprint"] = fingerprint_to_dict(manifest.fingerprint)
    payload["artifactDigests"] = [list(pair) for pair in manifest.artifact_digests]
    return payload


def _validate_timestamps(manifest: EvidenceManifest) -> None:
    _integer(
        manifest.created_at, "manifest.createdAt", minimum=0, maximum=MAX_JSON_INTEGER
    )
    if manifest.expires_at is not None:
        _integer(
            manifest.expires_at,
            "manifest.expiresAt",
            minimum=0,
            maximum=MAX_JSON_INTEGER,
        )
    if manifest.disposition == "not-required":
        _invalid(
            manifest.created_at != 0 or manifest.expires_at is not None,
            "not-required evidence must not have retention timestamps",
        )
        return
    _invalid(
        manifest.created_at <= 0,
        "required evidence must have a positive createdAt",
    )
    days = RETENTION_DAYS[manifest.retention_class]
    expected = None if days is None else manifest.created_at + days * 86_400
    _invalid(
        expected != manifest.expires_at or expected and expected > MAX_JSON_INTEGER,
        "manifest expiry does not match its retention class",
    )


def validate_manifest(manifest: EvidenceManifest) -> None:
    if not isinstance(manifest, EvidenceManifest):
        raise ContractError("Evidence manifest has an invalid structure")
    if type(manifest.schema_version) is not int or manifest.schema_version != 1:
        raise ContractError("unsupported Evidence manifest version")
    _text(manifest.evidence_id, "manifest.evidenceId")
    for field, allowed in (
        ("family", KNOWN_FAMILIES),
        ("disposition", DISPOSITIONS),
        ("outcome", OUTCOMES),
        ("retention_class", RETENTION_CLASSES),
        ("cache_mode", MANIFEST_CACHE_MODES),
    ):
        name = f"manifest.{_wire_name(field)}"
        if _text(getattr(manifest, field), name) not in allowed:
            raise ContractError(f"{name} is unsupported")
    _text(manifest.stage, "manifest.stage")
    if manifest.stage != FAMILY_STAGES[manifest.family]:
        raise ContractError("manifest.stage does not match its family")
    if not isinstance(manifest.candidate, CandidateIdentity):
        raise ContractError("manifest.candidate has an invalid structure")
    validate_candidate(manifest.candidate)
    if manifest.producer is not None:
        _text(manifest.producer, "manifest.producer")
    if manifest.disposition == "required":
        if manifest.outcome == "not-required" or manifest.producer is None:
            raise ContractError("required evidence needs a producer and a real outcome")
        if manifest.cache_mode == "cache-only" and manifest.outcome != "indeterminate":
            raise ContractError("cache-only evidence must be indeterminate")
    elif any(
        getattr(manifest, field) != expected for field, expected in SENTINEL_FIELDS
    ):
        raise ContractError("not-required evidence must be a structural sentinel")
    if not isinstance(manifest.fingerprint, ValidationFingerprint):
        raise ContractError("manifest.fingerprint has an invalid structure")
    validate_fingerprint(manifest.fingerprint)
    _artifact_pairs(manifest.artifact_digests, "manifest.artifactDigests")
    duration = _number(manifest.duration_seconds, "manifest.durationSeconds")
    critical = _number(manifest.critical_path_seconds, "manifest.criticalPathSeconds")
    if critical > duration:
        raise ContractError(
            "manifest.criticalPathSeconds cannot exceed durationSeconds"
        )
    _text(manifest.reason, "manifest.reason")
    _integer(manifest.attempt, "manifest.attempt", minimum=1, maximum=MAX_ATTEMPT)
    _validate_timestamps(manifest)
    expected_id = (
        f"{manifest.candidate.candidate_sha}:{manifest.family}:{manifest.stage}:"
        f"{manifest.fingerprint.digest}"
    )
    if manifest.evidence_id != expected_id:
        raise ContractError("manifest.evidenceId does not match its identity")


def validate_manifest_against_plan(
    manifest: EvidenceManifest, plan: ValidationPlan
) -> None:
    validate_manifest(manifest)
    validate_plan(plan)
    requirement = {item.family: item for item in plan.requirements}.get(manifest.family)
    if requirement is None:
        raise ContractError("manifest family is absent from the plan")
    if manifest.candidate != plan.candidate or manifest.fingerprint != plan.fingerprint:
        raise ContractError("manifest is bound to a different candidate or fingerprint")
    if (manifest.stage, manifest.disposition, manifest.retention_class) != (
        requirement.stage,
        requirement.disposition,
        requirement.retention_class,
    ):
        raise ContractError("manifest disagrees with its plan requirement")
    if (
        requirement.disposition == "not-required"
        and manifest.reason != requirement.reason
    ):
        raise ContractError("not-required manifest reason disagrees with its plan")


def manifest_for_requirement(
    plan: ValidationPlan,
    requirement: EvidenceRequirement,
    *,
    outcome: str = "passed",
    producer: str = "validation-producer",
    reason: str = "producer completed",
    artifact_digests: tuple[tuple[str, str], ...] = (),
    duration_seconds: int | float = 0,
    critical_path_seconds: int | float | None = None,
    attempt: int = 1,
    cache_mode: str = "not-used",
    created_at: int = 1,
) -> EvidenceManifest:
    validate_plan(plan)
    validate_requirement(requirement)
    expected = next(
        (item for item in plan.requirements if item.family == requirement.family), None
    )
    if expected != requirement:
        raise ContractError("manifest requirement is not the plan requirement")
    if requirement.selected:
        producer_value, outcome_value, artifacts = producer, outcome, artifact_digests
        duration, critical = duration_seconds, critical_path_seconds
        attempt_value, cache_value, created, manifest_reason = (
            attempt,
            cache_mode,
            created_at,
            reason,
        )
    else:
        producer_value, outcome_value, artifacts = None, "not-required", ()
        duration, critical = 0, 0
        attempt_value, cache_value, created, manifest_reason = (
            1,
            "not-used",
            0,
            requirement.reason,
        )
    if critical is None:
        critical = duration
    artifacts = _artifact_pairs(artifacts, "manifest.artifactDigests")
    days = RETENTION_DAYS[requirement.retention_class]
    manifest = EvidenceManifest(
        schema_version=1,
        evidence_id=(
            f"{plan.candidate.candidate_sha}:{requirement.family}:{requirement.stage}:"
            f"{plan.fingerprint.digest}"
        ),
        family=requirement.family,
        stage=requirement.stage,
        candidate=plan.candidate,
        producer=producer_value,
        outcome=outcome_value,
        disposition=requirement.disposition,
        fingerprint=plan.fingerprint,
        artifact_digests=artifacts,
        retention_class=requirement.retention_class,
        duration_seconds=duration,
        critical_path_seconds=critical,
        reason=manifest_reason,
        attempt=attempt_value,
        cache_mode=cache_value,
        created_at=created,
        expires_at=None if days is None or not created else created + days * 86_400,
    )
    validate_manifest_against_plan(manifest, plan)
    return manifest


def serialize_manifest(manifest: EvidenceManifest) -> str:
    validate_manifest(manifest)
    return _serialize_payload(manifest_to_dict(manifest), "Evidence manifest")


def manifest_from_dict(value: object) -> EvidenceManifest:
    payload = _object(value, "manifest")
    _keys(payload, MANIFEST_FIELDS, "manifest")
    values = {
        re.sub(r"(?<!^)(?=[A-Z])", "_", key).lower(): item
        for key, item in payload.items()
    }
    values["candidate"] = candidate_from_dict(payload["candidate"])
    values["fingerprint"] = fingerprint_from_dict(payload["fingerprint"])
    values["artifact_digests"] = _artifact_pairs(
        payload["artifactDigests"], "manifest.artifactDigests"
    )
    manifest = EvidenceManifest(**values)
    validate_manifest(manifest)
    return manifest


def _parse_int(value: str) -> int:
    sign = value.startswith("-")
    digits = value[1:] if sign else value
    if len(digits) > 19 or (len(digits) == 19 and digits > str(MAX_JSON_INTEGER)):
        raise ContractError("JSON integer exceeds its bounded range")
    return int(value)


def parse_manifest(value: object) -> EvidenceManifest:
    text = _input_text(value)
    try:
        payload = json.loads(
            text,
            object_pairs_hook=_reject_duplicate,
            parse_constant=_reject_constant,
            parse_int=_parse_int,
        )
    except ContractError:
        raise
    except (json.JSONDecodeError, TypeError, ValueError) as error:
        raise ContractError(f"invalid Evidence manifest JSON: {error}") from error
    manifest = manifest_from_dict(payload)
    if serialize_manifest(manifest) != text:
        raise ContractError("Evidence manifest is not canonically serialized")
    return manifest
