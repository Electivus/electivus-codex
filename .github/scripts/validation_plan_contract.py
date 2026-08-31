#!/usr/bin/env python3
"""Strict, bounded Validation plan contracts for the repository-owned seam."""

from dataclasses import dataclass
from typing import Any

from validation_contracts import PROFILES, SCHEMA_VERSION
from validation_contracts import VALIDATION_IMPLEMENTATION
from validation_contracts import CandidateIdentity, ContractError, ValidationFingerprint
from validation_contracts import _array, _keys, _object, _strings, _text
from validation_contracts import candidate_from_dict, candidate_to_dict, canonical_json
from validation_contracts import fingerprint_from_dict, fingerprint_to_dict
from validation_contracts import validate_candidate, validate_fingerprint


MAX_EVIDENCE = 64

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

FAMILY_STAGES = {
    "repository": "preflight",
    "repository-policy": "preflight",
    "repository-hygiene": "preflight",
    "rust-fast": "merge",
    "linux-x64-bazel": "merge",
    "api-protocol-sdk": "merge",
    "postgresql": "merge",
    "v8": "merge",
    "windows-x64": "merge",
    "code-quality": "merge",
    "linux-x64-cargo": "integrated",
    "linux-arm64": "integrated",
    "linux-musl": "integrated",
    "release-packaging": "release",
    "synchronization-topology": "synchronization",
}
EVIDENCE_FAMILIES = tuple(FAMILY_STAGES)
KNOWN_EVIDENCE_FAMILIES = frozenset(EVIDENCE_FAMILIES)

# These are the only platform labels owned by the fork validation contract.
# ``linux-musl`` represents the supported musl release variants; the concrete
# architecture is part of the producer matrix rather than a second plan label.
PLATFORM_ORDER = ("linux-x64", "linux-arm64", "linux-musl", "windows-x64")
KNOWN_PLATFORMS = frozenset(PLATFORM_ORDER)
FAMILY_PLATFORMS = {
    "repository": frozenset({"linux-x64"}),
    "repository-policy": frozenset({"linux-x64"}),
    "repository-hygiene": frozenset({"linux-x64"}),
    "rust-fast": frozenset({"linux-x64"}),
    "linux-x64-bazel": frozenset({"linux-x64"}),
    "api-protocol-sdk": frozenset({"linux-x64"}),
    "postgresql": frozenset({"linux-x64"}),
    "v8": frozenset({"linux-x64"}),
    "windows-x64": frozenset({"windows-x64"}),
    "code-quality": frozenset({"linux-x64"}),
    "linux-x64-cargo": frozenset({"linux-x64"}),
    "linux-arm64": frozenset({"linux-arm64"}),
    "linux-musl": frozenset({"linux-musl"}),
    "release-packaging": frozenset(KNOWN_PLATFORMS),
    "synchronization-topology": frozenset({"linux-x64"}),
}
CERTIFICATION_REQUIRED_STAGES = frozenset({"integrated", "release", "synchronization"})
CERTIFICATION_REQUIRED_CANDIDATE_KINDS = CERTIFICATION_REQUIRED_STAGES
DISPOSITIONS = frozenset({"required", "not-required"})
STAGES = frozenset(FAMILY_STAGES.values()) | {"surveillance"}
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
RETENTION_BY_PROFILE_STAGE = {
    ("ordinary", "preflight"): "ordinary-pull-request",
    ("ordinary", "merge"): "ordinary-pull-request",
    ("ordinary", "integrated"): "integrated-certification",
    ("ordinary", "release"): "unpublished-release-candidate",
    ("ordinary", "synchronization"): "certification-required-pull-request",
    ("ordinary", "surveillance"): "surveillance",
    ("certification-required", "preflight"): "certification-required-pull-request",
    ("certification-required", "merge"): "certification-required-pull-request",
    ("certification-required", "integrated"): "integrated-certification",
    ("certification-required", "release"): "unpublished-release-candidate",
    (
        "certification-required",
        "synchronization",
    ): "certification-required-pull-request",
    ("certification-required", "surveillance"): "surveillance",
}
CERTIFICATION_RISK_MODIFIERS = frozenset(
    {
        "security",
        "breaking",
        "migration",
        "publication",
        "validation-authority",
        "synchronization",
        "unknown",
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
    return dict(
        family=requirement.family,
        stage=requirement.stage,
        selected=requirement.selected,
        disposition=requirement.disposition,
        reason=requirement.reason,
        retentionClass=requirement.retention_class,
    )


def validate_requirement(requirement: EvidenceRequirement) -> None:
    if not isinstance(requirement, EvidenceRequirement):
        raise ContractError("evidence requirement has an invalid structure")
    _text(requirement.family, "requirement.family")
    if requirement.family not in KNOWN_EVIDENCE_FAMILIES:
        raise ContractError("requirement.family is unsupported")
    _text(requirement.stage, "requirement.stage")
    if requirement.stage not in STAGES:
        raise ContractError("requirement.stage is unsupported")
    if requirement.stage != FAMILY_STAGES[requirement.family]:
        raise ContractError("requirement.stage does not match its family")
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
    return dict(
        schemaVersion=plan.schema_version,
        validationImplementation=plan.validation_implementation,
        candidate=candidate_to_dict(plan.candidate),
        changeSurfaces=list(plan.surfaces),
        riskModifiers=list(plan.risk_modifiers),
        profile=plan.profile,
        evidence=[requirement_to_dict(item) for item in plan.requirements],
        fingerprint=fingerprint_to_dict(plan.fingerprint),
        policyErrors=list(plan.policy_errors),
    )


def _expected_array_parameter(
    values: dict[str, str],
    section: str,
    name: str,
    expected: tuple[str, ...],
) -> tuple[str, ...]:
    path = f"fingerprint.{section}.{name}"
    encoded = values.get(name)
    if encoded is None:
        raise ContractError(f"{path} is missing")
    if canonical_json(list(expected)) != encoded:
        raise ContractError(f"{path} does not match the plan")
    return expected


def _expected_platforms(plan: ValidationPlan) -> tuple[str, ...]:
    selected_families = (item.family for item in plan.requirements if item.selected)
    selected_platforms = frozenset(
        platform
        for family in selected_families
        for platform in FAMILY_PLATFORMS[family]
    )
    return tuple(
        platform for platform in PLATFORM_ORDER if platform in selected_platforms
    )


def _validate_fingerprint_platform_binding(plan: ValidationPlan) -> None:
    platforms = _strings(plan.fingerprint.platforms, "fingerprint.platforms")
    if any(platform not in KNOWN_PLATFORMS for platform in platforms):
        raise ContractError("fingerprint.platforms contains an unsupported value")
    if platforms != _expected_platforms(plan):
        raise ContractError("plan fingerprint platforms do not match the plan evidence")


def _validate_retention(plan: ValidationPlan) -> None:
    for requirement in plan.requirements:
        expected = RETENTION_BY_PROFILE_STAGE[(plan.profile, requirement.stage)]
        if requirement.retention_class == expected:
            continue
        if (
            requirement.retention_class == "test-reactivation-certification"
            and requirement.stage == "integrated"
        ):
            continue
        if (
            requirement.retention_class == "published-release"
            and plan.candidate.kind == "release"
            and requirement.selected
            and requirement.stage == "release"
        ):
            continue
        raise ContractError(
            "requirement.retentionClass is inconsistent with "
            f"{plan.profile}/{requirement.stage}"
        )


def _validate_fingerprint_binding(plan: ValidationPlan) -> None:
    fingerprint = plan.fingerprint
    if fingerprint.validation_implementation != plan.validation_implementation:
        raise ContractError("plan and fingerprint implementations disagree")
    if fingerprint.profile != plan.profile:
        raise ContractError("plan profile and fingerprint profile disagree")
    _validate_fingerprint_platform_binding(plan)

    candidate = candidate_to_dict(plan.candidate)
    expected_source = tuple(
        (name, "" if value is None else str(value)) for name, value in candidate.items()
    )
    if fingerprint.source != expected_source:
        raise ContractError("plan fingerprint is bound to a different candidate")

    dependencies = dict(fingerprint.dependencies)
    if dependencies.get("schemaVersion") != str(SCHEMA_VERSION):
        raise ContractError("plan fingerprint has an inconsistent schema version")
    expected_selected = tuple(
        item.family for item in plan.requirements if item.selected
    )
    if (
        _expected_array_parameter(
            dependencies, "dependencies", "selectedEvidence", expected_selected
        )
        != expected_selected
    ):
        raise ContractError(
            "plan fingerprint selected evidence does not match the plan"
        )

    parameters = dict(fingerprint.parameters)
    if (
        _expected_array_parameter(
            parameters, "parameters", "changeSurfaces", plan.surfaces
        )
        != plan.surfaces
    ):
        raise ContractError("plan fingerprint surfaces do not match the plan")
    if (
        _expected_array_parameter(
            parameters, "parameters", "riskModifiers", plan.risk_modifiers
        )
        != plan.risk_modifiers
    ):
        raise ContractError("plan fingerprint risk modifiers do not match the plan")
    if (
        _expected_array_parameter(
            parameters, "parameters", "policyErrors", plan.policy_errors
        )
        != plan.policy_errors
    ):
        raise ContractError("plan fingerprint policy errors do not match the plan")

    expected_commands = tuple(f"validation:{family}" for family in expected_selected)
    if fingerprint.commands != expected_commands:
        raise ContractError("plan fingerprint commands do not match the plan")


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
    if not any(item.selected for item in plan.requirements):
        raise ContractError("plan.evidence must select at least one requirement")
    if (
        plan.candidate.kind in CERTIFICATION_REQUIRED_CANDIDATE_KINDS
        or any(
            item.selected and item.stage in CERTIFICATION_REQUIRED_STAGES
            for item in plan.requirements
        )
    ) and plan.profile != "certification-required":
        raise ContractError(
            "integrated, release, and synchronization evidence require "
            "certification-required"
        )
    if (set(risk_modifiers) & CERTIFICATION_RISK_MODIFIERS or plan.policy_errors) and (
        plan.profile != "certification-required"
        or not all(item.selected for item in plan.requirements)
    ):
        raise ContractError(
            "uncertain plans require certification-required and complete evidence"
        )
    _validate_retention(plan)
    if not isinstance(plan.fingerprint, ValidationFingerprint):
        raise ContractError("plan.fingerprint has an invalid structure")
    validate_fingerprint(plan.fingerprint)
    _validate_fingerprint_binding(plan)


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
    evidence = _array(payload["evidence"], "plan.evidence")
    if len(evidence) > MAX_EVIDENCE:
        raise ContractError("plan.evidence has an invalid size")
    plan = ValidationPlan(
        schema_version=payload["schemaVersion"],
        validation_implementation=_text(
            payload["validationImplementation"], "plan.validationImplementation"
        ),
        candidate=candidate_from_dict(payload["candidate"]),
        surfaces=_strings(payload["changeSurfaces"], "plan.changeSurfaces"),
        risk_modifiers=_strings(payload["riskModifiers"], "plan.riskModifiers"),
        profile=_text(payload["profile"], "plan.profile"),
        requirements=tuple(requirement_from_dict(item) for item in evidence),
        fingerprint=fingerprint_from_dict(payload["fingerprint"]),
        policy_errors=_strings(payload["policyErrors"], "plan.policyErrors"),
    )
    validate_plan(plan)
    return plan


def _reject_duplicate(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ContractError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _reject_constant(value: str) -> Any:
    raise ContractError(f"invalid JSON constant: {value}")


def serialize_plan(plan: ValidationPlan) -> str:
    from validation_plan_codec import serialize_plan as encode_plan

    return encode_plan(plan)


def parse_plan(value: object) -> ValidationPlan:
    from validation_plan_codec import parse_plan as decode_plan

    return decode_plan(value)
