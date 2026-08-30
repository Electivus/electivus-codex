#!/usr/bin/env python3
"""Strict, bounded Validation plan contracts for the repository-owned seam."""

from dataclasses import dataclass
import json
from typing import Any

from validation_contracts import MAX_ITEMS
from validation_contracts import PROFILES
from validation_contracts import SCHEMA_VERSION
from validation_contracts import VALIDATION_IMPLEMENTATION
from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_contracts import ValidationFingerprint
from validation_contracts import _array
from validation_contracts import _keys
from validation_contracts import _object
from validation_contracts import _strings
from validation_contracts import _text
from validation_contracts import candidate_from_dict
from validation_contracts import candidate_to_dict
from validation_contracts import canonical_json
from validation_contracts import fingerprint_from_dict
from validation_contracts import fingerprint_to_dict
from validation_contracts import validate_candidate
from validation_contracts import validate_fingerprint


MAX_EVIDENCE = 64
MAX_PLAN_ITEMS = MAX_ITEMS
MAX_PLAN_TEXT_BYTES = 64_000
MAX_PLAN_INPUT_BYTES = 256_000
MAX_PLAN_OUTPUT_BYTES = 256_000
MAX_SERIALIZED_BYTES = MAX_PLAN_OUTPUT_BYTES
MAX_INPUT_BYTES = MAX_PLAN_INPUT_BYTES

CHANGE_SURFACES = (
    "repository",
    "documentation",
    "repository/documentation",
    "rust",
    "api/protocol/SDK",
    "Runtime State/Postgres",
    "execution/sandbox/V8",
    "platform/build",
    "package/release",
    "validation architecture",
    "upstream sync",
)
SURFACES = frozenset(CHANGE_SURFACES)

RISK_MODIFIERS = (
    "security",
    "breaking",
    "migration",
    "publication",
    "validation-authority",
    "synchronization",
    "unknown",
)
KNOWN_RISK_MODIFIERS = frozenset(RISK_MODIFIERS)

EVIDENCE_FAMILIES = (
    "repository",
    "repository-policy",
    "repository-hygiene",
    "rust-fast",
    "linux-x64-bazel",
    "api-protocol-sdk",
    "postgresql",
    "v8",
    "windows-x64",
    "code-quality",
    "linux-x64-cargo",
    "linux-arm64",
    "linux-musl",
    "release-packaging",
    "synchronization-topology",
)
KNOWN_EVIDENCE_FAMILIES = frozenset(EVIDENCE_FAMILIES)
DISPOSITIONS = frozenset({"required", "not-required"})
STAGES = frozenset({"preflight", "merge", "integrated", "release", "surveillance"})
RETENTION_CLASSES = frozenset(
    {
        "intra-run",
        "ordinary-pull-request",
        "certification-required-pull-request",
        "integrated-certification",
        "test-reactivation-certification",
        "unpublished-release-candidate",
        "published-release",
        "surveillance",
    }
)


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
    if not isinstance(requirement, EvidenceRequirement):
        raise ContractError("evidence requirement has an invalid structure")
    _text(requirement.family, "requirement.family")
    if requirement.family not in KNOWN_EVIDENCE_FAMILIES:
        raise ContractError("requirement.family is unsupported")
    _text(requirement.stage, "requirement.stage")
    if requirement.stage not in STAGES:
        raise ContractError("requirement.stage is unsupported")
    if not isinstance(requirement.selected, bool):
        raise ContractError("requirement.selected must be boolean")
    _text(requirement.disposition, "requirement.disposition")
    if requirement.disposition not in DISPOSITIONS:
        raise ContractError("requirement.disposition is unsupported")
    if requirement.selected != (requirement.disposition == "required"):
        raise ContractError("requirement selection and disposition disagree")
    _text(requirement.reason, "requirement.reason")
    _text(requirement.retention_class, "requirement.retentionClass")
    if requirement.retention_class not in RETENTION_CLASSES:
        raise ContractError("requirement.retentionClass is unsupported")


def requirement_from_dict(value: object) -> EvidenceRequirement:
    payload = _object(value, "requirement")
    _keys(
        payload,
        {"family", "stage", "selected", "disposition", "reason", "retentionClass"},
        "requirement",
    )
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
        "evidence": [requirement_to_dict(item) for item in plan.requirements],
        "fingerprint": fingerprint_to_dict(plan.fingerprint),
        "policyErrors": list(plan.policy_errors),
    }


def _canonical_array_parameter(
    values: dict[str, str],
    section: str,
    name: str,
    *,
    allowed: frozenset[str] | None = None,
) -> tuple[str, ...]:
    path = f"fingerprint.{section}.{name}"
    encoded = values.get(name)
    if encoded is None:
        raise ContractError(f"{path} is missing")
    try:
        value = json.loads(encoded, parse_constant=_reject_constant)
    except (json.JSONDecodeError, ContractError, TypeError) as error:
        raise ContractError(f"{path} must be a JSON array") from error
    if not isinstance(value, list):
        raise ContractError(f"{path} must be a JSON array")
    result = tuple(_text(item, path) for item in value)
    if len(set(result)) != len(result):
        raise ContractError(f"{path} must not contain duplicates")
    if allowed is not None and any(item not in allowed for item in result):
        raise ContractError(f"{path} contains an unsupported value")
    if canonical_json(list(result)) != encoded:
        raise ContractError(f"{path} is not canonical")
    return result


def _validate_fingerprint_binding(plan: ValidationPlan) -> None:
    fingerprint = plan.fingerprint
    if fingerprint.validation_implementation != plan.validation_implementation:
        raise ContractError("plan and fingerprint implementations disagree")
    if fingerprint.profile != plan.profile:
        raise ContractError("plan profile and fingerprint profile disagree")

    source = dict(fingerprint.source)
    expected_source = {
        "candidateSha": plan.candidate.candidate_sha,
        "baseSha": plan.candidate.base_sha or "",
        "headSha": plan.candidate.head_sha or "",
    }
    if any(source.get(name) != value for name, value in expected_source.items()):
        raise ContractError("plan fingerprint is bound to a different candidate")

    dependencies = dict(fingerprint.dependencies)
    if dependencies.get("schemaVersion") != str(SCHEMA_VERSION):
        raise ContractError("plan fingerprint has an inconsistent schema version")
    selected = _canonical_array_parameter(
        dependencies, "dependencies", "selectedEvidence"
    )
    expected_selected = tuple(
        item.family for item in plan.requirements if item.selected
    )
    if selected != expected_selected:
        raise ContractError(
            "plan fingerprint selected evidence does not match the plan"
        )

    parameters = dict(fingerprint.parameters)
    if (
        _canonical_array_parameter(
            parameters, "parameters", "changeSurfaces", allowed=SURFACES
        )
        != plan.surfaces
    ):
        raise ContractError("plan fingerprint surfaces do not match the plan")
    if (
        _canonical_array_parameter(
            parameters, "parameters", "riskModifiers", allowed=KNOWN_RISK_MODIFIERS
        )
        != plan.risk_modifiers
    ):
        raise ContractError("plan fingerprint risk modifiers do not match the plan")
    if (
        _canonical_array_parameter(parameters, "parameters", "policyErrors")
        != plan.policy_errors
    ):
        raise ContractError("plan fingerprint policy errors do not match the plan")

    expected_commands = tuple(f"validation:{family}" for family in expected_selected)
    if fingerprint.commands != expected_commands:
        raise ContractError("plan fingerprint commands do not match the plan")


def _item_and_text_budget(value: object) -> tuple[int, int]:
    items = 0
    text_bytes = 0

    def visit(item: object) -> None:
        nonlocal items, text_bytes
        if isinstance(item, list):
            items += len(item)
            for child in item:
                visit(child)
        elif isinstance(item, str):
            try:
                text_bytes += len(item.encode("utf-8"))
            except UnicodeEncodeError as error:
                raise ContractError("Validation plan contains invalid UTF-8") from error
        elif isinstance(item, dict):
            for child in item.values():
                visit(child)

    visit(value)
    return items, text_bytes


def validate_plan(plan: ValidationPlan) -> None:
    if not isinstance(plan, ValidationPlan):
        raise ContractError("Validation plan has an invalid structure")
    if type(plan.schema_version) is not int or plan.schema_version != SCHEMA_VERSION:
        raise ContractError(
            f"unsupported Validation plan version: {plan.schema_version}"
        )
    _text(plan.validation_implementation, "plan.validationImplementation")
    if plan.validation_implementation != VALIDATION_IMPLEMENTATION:
        raise ContractError("plan.validationImplementation is unsupported")
    if not isinstance(plan.candidate, CandidateIdentity):
        raise ContractError("plan.candidate has an invalid structure")
    validate_candidate(plan.candidate)
    surfaces = _strings(plan.surfaces, "plan.changeSurfaces")
    if not surfaces or any(value not in SURFACES for value in surfaces):
        raise ContractError("plan.changeSurfaces contains an unsupported value")
    risk_modifiers = _strings(plan.risk_modifiers, "plan.riskModifiers")
    if any(value not in KNOWN_RISK_MODIFIERS for value in risk_modifiers):
        raise ContractError("plan.riskModifiers contains an unsupported value")
    _strings(plan.policy_errors, "plan.policyErrors")
    _text(plan.profile, "plan.profile")
    if plan.profile not in PROFILES:
        raise ContractError("plan.profile is unsupported")
    if not isinstance(plan.requirements, (tuple, list)):
        raise ContractError("plan.evidence must be an array")
    if not plan.requirements or len(plan.requirements) > MAX_EVIDENCE:
        raise ContractError("plan.evidence has an invalid size")
    for requirement in plan.requirements:
        validate_requirement(requirement)
    if tuple(item.family for item in plan.requirements) != EVIDENCE_FAMILIES:
        raise ContractError("plan.evidence must be the complete canonical ledger")
    if ("unknown" in risk_modifiers or plan.policy_errors) and (
        plan.profile != "certification-required"
        or not all(item.selected for item in plan.requirements)
    ):
        raise ContractError(
            "uncertain plans require certification-required and complete evidence"
        )
    if not isinstance(plan.fingerprint, ValidationFingerprint):
        raise ContractError("plan.fingerprint has an invalid structure")
    validate_fingerprint(plan.fingerprint)
    _validate_fingerprint_binding(plan)
    items, text_bytes = _item_and_text_budget(plan_to_dict(plan))
    if items > MAX_PLAN_ITEMS:
        raise ContractError("Validation plan exceeds its aggregate item budget")
    if text_bytes > MAX_PLAN_TEXT_BYTES:
        raise ContractError("Validation plan exceeds its aggregate text budget")


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
            "evidence",
            "fingerprint",
            "policyErrors",
        },
        "plan",
    )
    plan = ValidationPlan(
        schema_version=payload["schemaVersion"],
        validation_implementation=_text(
            payload["validationImplementation"], "plan.validationImplementation"
        ),
        candidate=candidate_from_dict(payload["candidate"]),
        surfaces=_strings(payload["changeSurfaces"], "plan.changeSurfaces"),
        risk_modifiers=_strings(payload["riskModifiers"], "plan.riskModifiers"),
        profile=_text(payload["profile"], "plan.profile"),
        requirements=tuple(
            requirement_from_dict(item)
            for item in _array(payload["evidence"], "plan.evidence")
        ),
        fingerprint=fingerprint_from_dict(payload["fingerprint"]),
        policy_errors=_strings(payload["policyErrors"], "plan.policyErrors"),
    )
    validate_plan(plan)
    return plan


def _serialize_payload(payload: dict[str, object], name: str) -> str:
    try:
        text = (
            json.dumps(
                payload,
                ensure_ascii=False,
                allow_nan=False,
                sort_keys=True,
                indent=2,
            )
            + "\n"
        )
        size = len(text.encode("utf-8"))
    except (TypeError, UnicodeEncodeError, ValueError) as error:
        raise ContractError(f"{name} cannot be canonically serialized") from error
    if size > MAX_PLAN_OUTPUT_BYTES:
        raise ContractError(f"{name} exceeds its serialized byte budget")
    return text


def serialize_plan(plan: ValidationPlan) -> str:
    validate_plan(plan)
    return _serialize_payload(plan_to_dict(plan), "Validation plan")


def _reject_duplicate(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ContractError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _reject_constant(value: str) -> Any:
    raise ContractError(f"invalid JSON constant: {value}")


def _input_text(value: object) -> str:
    if isinstance(value, bytes):
        try:
            text = value.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ContractError("Validation plan JSON must be valid UTF-8") from error
    elif isinstance(value, str):
        text = value
    else:
        raise ContractError("Validation plan JSON must be text or UTF-8 bytes")
    try:
        size = len(text.encode("utf-8"))
    except UnicodeEncodeError as error:
        raise ContractError("Validation plan JSON must be valid UTF-8") from error
    if size > MAX_PLAN_INPUT_BYTES:
        raise ContractError("Validation plan exceeds its input byte budget")
    return text


def parse_plan(value: object) -> ValidationPlan:
    text = _input_text(value)
    try:
        payload = json.loads(
            text,
            object_pairs_hook=_reject_duplicate,
            parse_constant=_reject_constant,
        )
    except ContractError:
        raise
    except (json.JSONDecodeError, TypeError, UnicodeDecodeError) as error:
        raise ContractError(f"invalid Validation plan JSON: {error}") from error
    plan = plan_from_dict(payload)
    if serialize_plan(plan) != text:
        raise ContractError("Validation plan is not canonically serialized")
    return plan
