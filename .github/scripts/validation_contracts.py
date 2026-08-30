#!/usr/bin/env python3
"""Versioned contracts shared by the fork-owned validation stages.

The module deliberately contains no GitHub API or workflow code.  It is the
repository-owned seam between planning, evidence producers, and aggregation.
Every serialized object is deterministic, bounded, and carries enough
identity to reject reuse from a different candidate or validation contract.
"""

from dataclasses import dataclass
import hashlib
import json
import math
import re
from typing import Any


SCHEMA_VERSION = 1
VALIDATION_IMPLEMENTATION = "electivus-validation-v1"
MAX_PATHS = 2_000
MAX_PATH_LENGTH = 4_096
MAX_TEXT_LENGTH = 4_096
MAX_EVIDENCE = 64
MAX_SERIALIZED_BYTES = 256_000

SHA1_PATTERN = re.compile(r"^[0-9a-f]{40}$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")

OUTCOMES = frozenset(
    {
        "passed",
        "product-failure",
        "infrastructure-failure",
        "indeterminate",
        "stale",
        "not-required",
    }
)
DISPOSITIONS = frozenset({"required", "not-required"})
PROFILES = frozenset({"ordinary", "certification-required"})
CANDIDATE_KINDS = frozenset(
    {"pull-request", "integrated", "release", "surveillance", "synchronization"}
)
MANIFEST_CACHE_MODES = frozenset(
    {
        "not-used",
        "cold",
        "cache-hit-verified",
        "disabled-reconstruction",
        "cache-only",
    }
)
RETENTION_DAYS: dict[str, int | None] = {
    "intra-run": 1,
    "ordinary-pull-request": 7,
    "certification-required-pull-request": 30,
    "integrated-certification": 90,
    "test-reactivation-certification": 90,
    "unpublished-release-candidate": 30,
    "published-release": None,
    "surveillance": 30,
}


class ContractError(ValueError):
    """Raised when an untrusted validation object violates its contract."""


def _text(value: object, name: str, *, allow_empty: bool = False) -> str:
    if not isinstance(value, str):
        raise ContractError(f"{name} must be a string")
    if not allow_empty and not value:
        raise ContractError(f"{name} must not be empty")
    if len(value.encode("utf-8")) > MAX_TEXT_LENGTH:
        raise ContractError(f"{name} exceeds its byte budget")
    if any(ord(character) < 32 and character not in "\t" for character in value):
        raise ContractError(f"{name} contains a control character")
    return value


def _sha(value: object, name: str, *, allow_empty: bool = False) -> str:
    value = _text(value, name, allow_empty=allow_empty)
    if allow_empty and not value:
        return value
    if SHA1_PATTERN.fullmatch(value) is None:
        raise ContractError(f"{name} must be a lowercase 40-character SHA")
    return value


def _sha256(value: object, name: str) -> str:
    value = _text(value, name)
    if SHA256_PATTERN.fullmatch(value) is None:
        raise ContractError(f"{name} must be a lowercase 64-character SHA-256")
    return value


def _number(value: object, name: str, *, minimum: float = 0) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ContractError(f"{name} must be a number")
    if not math.isfinite(value) or value < minimum:
        raise ContractError(f"{name} must be at least {minimum}")
    return float(value)


def _paths(values: object, name: str) -> tuple[str, ...]:
    if not isinstance(values, list | tuple):
        raise ContractError(f"{name} must be an array")
    if len(values) > MAX_PATHS:
        raise ContractError(f"{name} exceeds its item budget")
    result = []
    for value in values:
        path = _text(value, name)
        if len(path.encode("utf-8")) > MAX_PATH_LENGTH or path.startswith("/") or "\x00" in path:
            raise ContractError(f"{name} contains an invalid path")
        result.append(path)
    return tuple(result)


def _unique_strings(values: object, name: str) -> tuple[str, ...]:
    if not isinstance(values, list | tuple):
        raise ContractError(f"{name} must be an array")
    result = tuple(_text(value, name) for value in values)
    if len(set(result)) != len(result):
        raise ContractError(f"{name} must not contain duplicates")
    return result


def _pairs(values: object, name: str) -> tuple[tuple[str, str], ...]:
    if not isinstance(values, list | tuple):
        raise ContractError(f"{name} must be an array")
    result = []
    for value in values:
        if not isinstance(value, list | tuple) or len(value) != 2:
            raise ContractError(f"{name} must contain key/value pairs")
        result.append((_text(value[0], name), _text(value[1], name, allow_empty=True)))
    if len({key for key, _ in result}) != len(result):
        raise ContractError(f"{name} must not contain duplicate keys")
    return tuple(result)


def _object(value: object, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ContractError(f"{name} must be an object")
    return value


def _keys(value: dict[str, Any], expected: set[str], name: str) -> None:
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        detail = []
        if missing:
            detail.append(f"missing {','.join(missing)}")
        if extra:
            detail.append(f"unexpected {','.join(extra)}")
        raise ContractError(f"{name} has invalid fields ({'; '.join(detail)})")


def _reject_duplicate(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ContractError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _reject_constant(value: str) -> Any:
    raise ContractError(f"invalid JSON constant: {value}")


def _load(text: str, name: str) -> dict[str, Any]:
    try:
        value = json.loads(
            text,
            object_pairs_hook=_reject_duplicate,
            parse_constant=_reject_constant,
        )
    except (json.JSONDecodeError, ContractError) as error:
        if isinstance(error, ContractError):
            raise
        raise ContractError(f"invalid {name} JSON: {error}") from error
    return _object(value, name)


def canonical_json(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _serialized(value: object, name: str) -> str:
    text = json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2) + "\n"
    if len(text.encode("utf-8")) > MAX_SERIALIZED_BYTES:
        raise ContractError(f"{name} exceeds its serialized byte budget")
    return text


@dataclass(frozen=True)
class CandidateIdentity:
    event_name: str
    repository: str
    default_branch: str
    candidate_sha: str
    base_sha: str | None
    head_sha: str | None
    kind: str
    pull_request_number: int | None = None
    branch: str = ""


def candidate_to_dict(candidate: CandidateIdentity) -> dict[str, object]:
    return {
        "eventName": candidate.event_name,
        "repository": candidate.repository,
        "defaultBranch": candidate.default_branch,
        "candidateSha": candidate.candidate_sha,
        "baseSha": candidate.base_sha,
        "headSha": candidate.head_sha,
        "kind": candidate.kind,
        "pullRequestNumber": candidate.pull_request_number,
        "branch": candidate.branch,
    }


def validate_candidate(candidate: CandidateIdentity) -> None:
    _text(candidate.event_name, "candidate.eventName")
    _text(candidate.repository, "candidate.repository")
    _text(candidate.default_branch, "candidate.defaultBranch")
    _sha(candidate.candidate_sha, "candidate.candidateSha")
    if candidate.base_sha is not None:
        _sha(candidate.base_sha, "candidate.baseSha")
    if candidate.head_sha is not None:
        _sha(candidate.head_sha, "candidate.headSha")
    _text(candidate.kind, "candidate.kind")
    if candidate.kind not in CANDIDATE_KINDS:
        raise ContractError("candidate.kind is unsupported")
    _text(candidate.branch, "candidate.branch", allow_empty=True)
    if candidate.pull_request_number is not None and (
        isinstance(candidate.pull_request_number, bool)
        or not isinstance(candidate.pull_request_number, int)
        or candidate.pull_request_number <= 0
    ):
        raise ContractError("candidate.pullRequestNumber must be a positive integer")
    if candidate.kind == "pull-request" and (
        candidate.base_sha is None or candidate.head_sha is None
    ):
        raise ContractError("pull-request candidates require base and head SHA")
    if candidate.kind == "integrated" and candidate.event_name not in {
        "push",
        "workflow_dispatch",
    }:
        raise ContractError("integrated candidates require a push or dispatch event")


def candidate_from_dict(value: object) -> CandidateIdentity:
    payload = _object(value, "candidate")
    _keys(
        payload,
        {
            "eventName",
            "repository",
            "defaultBranch",
            "candidateSha",
            "baseSha",
            "headSha",
            "kind",
            "pullRequestNumber",
            "branch",
        },
        "candidate",
    )
    pull_request_number = payload["pullRequestNumber"]
    if pull_request_number is not None and not isinstance(pull_request_number, int):
        raise ContractError("candidate.pullRequestNumber must be an integer or null")
    candidate = CandidateIdentity(
        event_name=_text(payload["eventName"], "candidate.eventName"),
        repository=_text(payload["repository"], "candidate.repository"),
        default_branch=_text(payload["defaultBranch"], "candidate.defaultBranch"),
        candidate_sha=_sha(payload["candidateSha"], "candidate.candidateSha"),
        base_sha=(
            None
            if payload["baseSha"] is None
            else _sha(payload["baseSha"], "candidate.baseSha")
        ),
        head_sha=(
            None
            if payload["headSha"] is None
            else _sha(payload["headSha"], "candidate.headSha")
        ),
        kind=_text(payload["kind"], "candidate.kind"),
        pull_request_number=pull_request_number,
        branch=_text(payload["branch"], "candidate.branch", allow_empty=True),
    )
    validate_candidate(candidate)
    return candidate


@dataclass(frozen=True)
class ValidationFingerprint:
    source: tuple[tuple[str, str], ...]
    validation_implementation: str
    dependencies: tuple[tuple[str, str], ...]
    toolchains: tuple[tuple[str, str], ...]
    commands: tuple[str, ...]
    platforms: tuple[str, ...]
    profile: str
    parameters: tuple[tuple[str, str], ...]
    inputs: tuple[tuple[str, str], ...]

    @property
    def digest(self) -> str:
        return hashlib.sha256(
            canonical_json(_fingerprint_payload(self)).encode()
        ).hexdigest()


def _fingerprint_pairs(values: tuple[tuple[str, str], ...]) -> list[list[str]]:
    return [[key, value] for key, value in values]


def _fingerprint_payload(fingerprint: ValidationFingerprint) -> dict[str, object]:
    return {
        "source": _fingerprint_pairs(fingerprint.source),
        "validationImplementation": fingerprint.validation_implementation,
        "dependencies": _fingerprint_pairs(fingerprint.dependencies),
        "toolchains": _fingerprint_pairs(fingerprint.toolchains),
        "commands": list(fingerprint.commands),
        "platforms": list(fingerprint.platforms),
        "profile": fingerprint.profile,
        "parameters": _fingerprint_pairs(fingerprint.parameters),
        "inputs": _fingerprint_pairs(fingerprint.inputs),
    }


def fingerprint_to_dict(fingerprint: ValidationFingerprint) -> dict[str, object]:
    return {**_fingerprint_payload(fingerprint), "digest": fingerprint.digest}


def validate_fingerprint(fingerprint: ValidationFingerprint) -> None:
    for name, values in (
        ("fingerprint.source", fingerprint.source),
        ("fingerprint.dependencies", fingerprint.dependencies),
        ("fingerprint.toolchains", fingerprint.toolchains),
        ("fingerprint.parameters", fingerprint.parameters),
        ("fingerprint.inputs", fingerprint.inputs),
    ):
        _pairs(list(values), name)
    if fingerprint.validation_implementation != VALIDATION_IMPLEMENTATION:
        raise ContractError("fingerprint.validationImplementation is unsupported")
    _unique_strings(fingerprint.commands, "fingerprint.commands")
    _unique_strings(fingerprint.platforms, "fingerprint.platforms")
    if fingerprint.profile not in PROFILES and fingerprint.profile not in {
        "integrated",
        "release",
        "surveillance",
    }:
        raise ContractError("fingerprint.profile is unsupported")


def fingerprint_from_dict(value: object) -> ValidationFingerprint:
    payload = _object(value, "fingerprint")
    _keys(
        payload,
        {
            "source",
            "validationImplementation",
            "dependencies",
            "toolchains",
            "commands",
            "platforms",
            "profile",
            "parameters",
            "inputs",
            "digest",
        },
        "fingerprint",
    )
    fingerprint = ValidationFingerprint(
        source=_pairs(payload["source"], "fingerprint.source"),
        validation_implementation=_text(
            payload["validationImplementation"],
            "fingerprint.validationImplementation",
        ),
        dependencies=_pairs(payload["dependencies"], "fingerprint.dependencies"),
        toolchains=_pairs(payload["toolchains"], "fingerprint.toolchains"),
        commands=_unique_strings(payload["commands"], "fingerprint.commands"),
        platforms=_unique_strings(payload["platforms"], "fingerprint.platforms"),
        profile=_text(payload["profile"], "fingerprint.profile"),
        parameters=_pairs(payload["parameters"], "fingerprint.parameters"),
        inputs=_pairs(payload["inputs"], "fingerprint.inputs"),
    )
    validate_fingerprint(fingerprint)
    if _sha256(payload["digest"], "fingerprint.digest") != fingerprint.digest:
        raise ContractError("fingerprint digest does not match its fields")
    return fingerprint


@dataclass(frozen=True)
class EvidenceRequirement:
    family: str
    stage: str
    selected: bool
    disposition: str
    reason: str
    retention_class: str


def requirement_to_dict(requirement: EvidenceRequirement) -> dict[str, object]:
    return {
        "family": requirement.family,
        "stage": requirement.stage,
        "selected": requirement.selected,
        "disposition": requirement.disposition,
        "reason": requirement.reason,
        "retentionClass": requirement.retention_class,
    }


def validate_requirement(requirement: EvidenceRequirement) -> None:
    _text(requirement.family, "requirement.family")
    _text(requirement.stage, "requirement.stage")
    if requirement.disposition not in DISPOSITIONS:
        raise ContractError("requirement.disposition is unsupported")
    if requirement.selected != (requirement.disposition == "required"):
        raise ContractError("requirement selection and disposition disagree")
    _text(requirement.reason, "requirement.reason")
    if requirement.retention_class not in RETENTION_DAYS:
        raise ContractError("requirement.retentionClass is unsupported")


def requirement_from_dict(value: object) -> EvidenceRequirement:
    payload = _object(value, "requirement")
    _keys(
        payload,
        {"family", "stage", "selected", "disposition", "reason", "retentionClass"},
        "requirement",
    )
    if not isinstance(payload["selected"], bool):
        raise ContractError("requirement.selected must be boolean")
    requirement = EvidenceRequirement(
        family=_text(payload["family"], "requirement.family"),
        stage=_text(payload["stage"], "requirement.stage"),
        selected=payload["selected"],
        disposition=_text(payload["disposition"], "requirement.disposition"),
        reason=_text(payload["reason"], "requirement.reason"),
        retention_class=_text(payload["retentionClass"], "requirement.retentionClass"),
    )
    validate_requirement(requirement)
    return requirement


@dataclass(frozen=True)
class ValidationPlan:
    schema_version: int
    validation_implementation: str
    candidate: CandidateIdentity
    surfaces: tuple[str, ...]
    risk_modifiers: tuple[str, ...]
    profile: str
    codeql_languages: tuple[str, ...]
    requirements: tuple[EvidenceRequirement, ...]
    fingerprint: ValidationFingerprint
    policy_errors: tuple[str, ...] = ()


def plan_to_dict(plan: ValidationPlan) -> dict[str, object]:
    return {
        "schemaVersion": plan.schema_version,
        "validationImplementation": plan.validation_implementation,
        "candidate": candidate_to_dict(plan.candidate),
        "changeSurfaces": list(plan.surfaces),
        "riskModifiers": list(plan.risk_modifiers),
        "profile": plan.profile,
        "codeqlLanguages": list(plan.codeql_languages),
        "evidence": [requirement_to_dict(item) for item in plan.requirements],
        "fingerprint": fingerprint_to_dict(plan.fingerprint),
        "policyErrors": list(plan.policy_errors),
    }


def validate_plan(plan: ValidationPlan) -> None:
    if plan.schema_version != SCHEMA_VERSION:
        raise ContractError(f"unsupported Validation plan version: {plan.schema_version}")
    if plan.validation_implementation != VALIDATION_IMPLEMENTATION:
        raise ContractError("plan.validationImplementation is unsupported")
    if plan.profile not in PROFILES:
        raise ContractError("plan.profile is unsupported")
    validate_candidate(plan.candidate)
    _unique_strings(plan.surfaces, "plan.changeSurfaces")
    _unique_strings(plan.risk_modifiers, "plan.riskModifiers")
    _unique_strings(plan.codeql_languages, "plan.codeqlLanguages")
    _unique_strings(plan.policy_errors, "plan.policyErrors")
    if not plan.requirements or len(plan.requirements) > MAX_EVIDENCE:
        raise ContractError("plan.evidence has an invalid size")
    families = set()
    for requirement in plan.requirements:
        validate_requirement(requirement)
        if requirement.family in families:
            raise ContractError("plan.evidence contains a duplicate family")
        families.add(requirement.family)
    validate_fingerprint(plan.fingerprint)
    if plan.fingerprint.profile != plan.profile:
        raise ContractError("plan profile and fingerprint profile disagree")
    source = dict(plan.fingerprint.source)
    if source.get("candidateSha") != plan.candidate.candidate_sha:
        raise ContractError("plan fingerprint is bound to a different candidate")
    if source.get("baseSha", "") != (plan.candidate.base_sha or ""):
        raise ContractError("plan fingerprint is bound to a different base")
    if source.get("headSha", "") != (plan.candidate.head_sha or ""):
        raise ContractError("plan fingerprint is bound to a different head")
    dependencies = dict(plan.fingerprint.dependencies)
    if dependencies.get("schemaVersion") != str(SCHEMA_VERSION):
        raise ContractError("plan fingerprint has an inconsistent schema version")
    selected = ",".join(
        requirement.family for requirement in plan.requirements if requirement.selected
    )
    if dependencies.get("selectedEvidence") != selected:
        raise ContractError("plan fingerprint selected evidence does not match the plan")
    parameters = dict(plan.fingerprint.parameters)
    expected_parameters = {
        "changeSurfaces": ",".join(plan.surfaces),
        "riskModifiers": ",".join(plan.risk_modifiers),
        "codeqlLanguages": ",".join(plan.codeql_languages),
        "policyErrors": "\n".join(plan.policy_errors),
    }
    if any(parameters.get(name) != value for name, value in expected_parameters.items()):
        raise ContractError("plan fingerprint parameters do not match the plan")
    expected_commands = tuple(
        f"validation:{requirement.family}"
        for requirement in plan.requirements
        if requirement.selected
    )
    if plan.fingerprint.commands != expected_commands:
        raise ContractError("plan fingerprint commands do not match the plan")
    expected_platforms = tuple(
        dict.fromkeys(
            platform
            for requirement in plan.requirements
            if requirement.selected
            for platform in {
                "linux-x64-bazel": ("linux-x64",),
                "linux-x64-cargo": ("linux-x64",),
                "linux-arm64": ("linux-arm64",),
                "linux-musl": ("linux-musl",),
                "windows-x64": ("windows-x64",),
            }.get(requirement.family, ())
        )
    )
    if plan.fingerprint.platforms != expected_platforms:
        raise ContractError("plan fingerprint platforms do not match the plan")


def plan_from_dict(value: object) -> ValidationPlan:
    payload = _object(value, "plan")
    _keys(
        payload,
        {
            "schemaVersion",
            "validationImplementation",
            "candidate",
            "changeSurfaces",
            "riskModifiers",
            "profile",
            "codeqlLanguages",
            "evidence",
            "fingerprint",
            "policyErrors",
        },
        "plan",
    )
    if not isinstance(payload["schemaVersion"], int):
        raise ContractError("plan.schemaVersion must be an integer")
    plan = ValidationPlan(
        schema_version=payload["schemaVersion"],
        validation_implementation=_text(
            payload["validationImplementation"], "plan.validationImplementation"
        ),
        candidate=candidate_from_dict(payload["candidate"]),
        surfaces=_unique_strings(payload["changeSurfaces"], "plan.changeSurfaces"),
        risk_modifiers=_unique_strings(payload["riskModifiers"], "plan.riskModifiers"),
        profile=_text(payload["profile"], "plan.profile"),
        codeql_languages=_unique_strings(
            payload["codeqlLanguages"], "plan.codeqlLanguages"
        ),
        requirements=tuple(
            requirement_from_dict(item)
            for item in _object(payload, "plan")["evidence"]
        ),
        fingerprint=fingerprint_from_dict(payload["fingerprint"]),
        policy_errors=_unique_strings(payload["policyErrors"], "plan.policyErrors"),
    )
    validate_plan(plan)
    return plan


def serialize_plan(plan: ValidationPlan) -> str:
    validate_plan(plan)
    return _serialized(plan_to_dict(plan), "Validation plan")


def parse_plan(text: str) -> ValidationPlan:
    return plan_from_dict(_load(text, "Validation plan"))


@dataclass(frozen=True)
class EvidenceManifest:
    schema_version: int
    evidence_id: str
    family: str
    stage: str
    candidate: CandidateIdentity
    producer: str
    outcome: str
    disposition: str
    fingerprint: ValidationFingerprint
    artifact_digests: tuple[tuple[str, str], ...]
    retention_class: str
    duration_seconds: float
    critical_path_seconds: float
    reason: str
    attempt: int = 1
    cache_mode: str = "not-used"
    created_at: int = 0
    expires_at: int | None = None


def manifest_to_dict(manifest: EvidenceManifest) -> dict[str, object]:
    return {
        "schemaVersion": manifest.schema_version,
        "evidenceId": manifest.evidence_id,
        "family": manifest.family,
        "stage": manifest.stage,
        "candidate": candidate_to_dict(manifest.candidate),
        "producer": manifest.producer,
        "outcome": manifest.outcome,
        "disposition": manifest.disposition,
        "fingerprint": fingerprint_to_dict(manifest.fingerprint),
        "artifactDigests": _fingerprint_pairs(manifest.artifact_digests),
        "retentionClass": manifest.retention_class,
        "durationSeconds": manifest.duration_seconds,
        "criticalPathSeconds": manifest.critical_path_seconds,
        "reason": manifest.reason,
        "attempt": manifest.attempt,
        "cacheMode": manifest.cache_mode,
        "createdAt": manifest.created_at,
        "expiresAt": manifest.expires_at,
    }


def validate_manifest(manifest: EvidenceManifest) -> None:
    if manifest.schema_version != SCHEMA_VERSION:
        raise ContractError("unsupported Evidence manifest version")
    _text(manifest.evidence_id, "manifest.evidenceId")
    _text(manifest.family, "manifest.family")
    _text(manifest.stage, "manifest.stage")
    validate_candidate(manifest.candidate)
    _text(manifest.producer, "manifest.producer")
    if manifest.outcome not in OUTCOMES:
        raise ContractError("manifest.outcome is unsupported")
    if manifest.disposition not in DISPOSITIONS:
        raise ContractError("manifest.disposition is unsupported")
    if manifest.disposition == "required" and manifest.outcome == "not-required":
        raise ContractError("required evidence cannot have a not-required outcome")
    validate_fingerprint(manifest.fingerprint)
    if manifest.retention_class not in RETENTION_DAYS:
        raise ContractError("manifest.retentionClass is unsupported")
    if manifest.disposition == "required" and manifest.retention_class == "ordinary-pull-request" and manifest.stage == "certification-required":
        raise ContractError("Certification-required evidence has ordinary retention")
    _pairs(list(manifest.artifact_digests), "manifest.artifactDigests")
    for name, digest in manifest.artifact_digests:
        _text(name, "manifest.artifactDigests.name")
        _sha256(digest, "manifest.artifactDigests.digest")
    _number(manifest.duration_seconds, "manifest.durationSeconds")
    _number(manifest.critical_path_seconds, "manifest.criticalPathSeconds")
    _text(manifest.reason, "manifest.reason")
    if isinstance(manifest.attempt, bool) or not isinstance(manifest.attempt, int) or manifest.attempt < 1:
        raise ContractError("manifest.attempt must be a positive integer")
    if manifest.cache_mode not in MANIFEST_CACHE_MODES:
        raise ContractError("manifest.cacheMode is unsupported")
    if (
        isinstance(manifest.created_at, bool)
        or not isinstance(manifest.created_at, int)
        or manifest.created_at < 0
    ):
        raise ContractError("manifest.createdAt must be a non-negative integer")
    retention_days = RETENTION_DAYS[manifest.retention_class]
    if retention_days is None:
        if manifest.expires_at is not None:
            raise ContractError("published-release evidence must not have an expiry")
    elif manifest.expires_at is not None and not manifest.created_at:
        raise ContractError("un-timestamped retained evidence must not have an expiry")
    elif manifest.created_at and manifest.expires_at is None:
        raise ContractError("retained evidence must record its expiry")
    if manifest.expires_at is not None:
        if (
            isinstance(manifest.expires_at, bool)
            or not isinstance(manifest.expires_at, int)
            or manifest.expires_at <= manifest.created_at
        ):
            raise ContractError("manifest.expiresAt must be after createdAt")
        if manifest.created_at and manifest.expires_at != manifest.created_at + retention_days * 86_400:
            raise ContractError("manifest expiry does not match its retention class")


def manifest_from_dict(value: object) -> EvidenceManifest:
    payload = _object(value, "manifest")
    _keys(
        payload,
        {
            "schemaVersion",
            "evidenceId",
            "family",
            "stage",
            "candidate",
            "producer",
            "outcome",
            "disposition",
            "fingerprint",
            "artifactDigests",
            "retentionClass",
            "durationSeconds",
            "criticalPathSeconds",
            "reason",
            "attempt",
            "cacheMode",
            "createdAt",
            "expiresAt",
        },
        "manifest",
    )
    if not isinstance(payload["schemaVersion"], int):
        raise ContractError("manifest.schemaVersion must be an integer")
    duration = _number(payload["durationSeconds"], "manifest.durationSeconds")
    critical_path = _number(
        payload["criticalPathSeconds"], "manifest.criticalPathSeconds"
    )
    manifest = EvidenceManifest(
        schema_version=payload["schemaVersion"],
        evidence_id=_text(payload["evidenceId"], "manifest.evidenceId"),
        family=_text(payload["family"], "manifest.family"),
        stage=_text(payload["stage"], "manifest.stage"),
        candidate=candidate_from_dict(payload["candidate"]),
        producer=_text(payload["producer"], "manifest.producer"),
        outcome=_text(payload["outcome"], "manifest.outcome"),
        disposition=_text(payload["disposition"], "manifest.disposition"),
        fingerprint=fingerprint_from_dict(payload["fingerprint"]),
        artifact_digests=_pairs(payload["artifactDigests"], "manifest.artifactDigests"),
        retention_class=_text(payload["retentionClass"], "manifest.retentionClass"),
        duration_seconds=duration,
        critical_path_seconds=critical_path,
        reason=_text(payload["reason"], "manifest.reason"),
        attempt=payload["attempt"],
        cache_mode=_text(payload["cacheMode"], "manifest.cacheMode"),
        created_at=payload["createdAt"],
        expires_at=payload["expiresAt"],
    )
    validate_manifest(manifest)
    return manifest


def serialize_manifest(manifest: EvidenceManifest) -> str:
    validate_manifest(manifest)
    return _serialized(manifest_to_dict(manifest), "Evidence manifest")


def parse_manifest(text: str) -> EvidenceManifest:
    return manifest_from_dict(_load(text, "Evidence manifest"))


@dataclass(frozen=True)
class ValidationReport:
    schema_version: int
    candidate: CandidateIdentity
    plan: ValidationPlan
    evidence: tuple[EvidenceManifest, ...]
    outcome: str
    outcomes: tuple[tuple[str, str], ...]
    durations: tuple[tuple[str, float], ...]
    slo: tuple[tuple[str, str], ...]
    state: str
    admission_allowed: bool
    next_actions: tuple[str, ...]
    errors: tuple[str, ...]
    cache_fallback: str = "not-applicable"
    authorization_status: str = "not-applicable"
    artifacts: tuple[tuple[str, str], ...] = ()
