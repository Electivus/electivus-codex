"""Validation plan contract built on immutable candidate and fingerprint identity."""

from dataclasses import dataclass

from validation_contracts import MAX_ITEMS
from validation_contracts import PROFILES
from validation_contracts import SCHEMA_VERSION
from validation_contracts import VALIDATION_IMPLEMENTATION
from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_contracts import ValidationFingerprint
from validation_contracts import _array
from validation_contracts import _integer
from validation_contracts import _keys
from validation_contracts import _load
from validation_contracts import _object
from validation_contracts import _serialized
from validation_contracts import _strings
from validation_contracts import _text
from validation_contracts import candidate_from_dict
from validation_contracts import candidate_to_dict
from validation_contracts import fingerprint_from_dict
from validation_contracts import fingerprint_to_dict
from validation_contracts import validate_candidate
from validation_contracts import validate_fingerprint


MAX_EVIDENCE = min(MAX_ITEMS, 64)
DISPOSITIONS = frozenset({"required", "not-required"})
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
    _text(requirement.family, "requirement.family")
    _text(requirement.stage, "requirement.stage")
    if not isinstance(requirement.selected, bool):
        raise ContractError("requirement.selected must be boolean")
    if requirement.disposition not in DISPOSITIONS:
        raise ContractError("requirement.disposition is unsupported")
    if requirement.selected != (requirement.disposition == "required"):
        raise ContractError("requirement selection and disposition disagree")
    _text(requirement.reason, "requirement.reason")
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


def validate_plan(plan: ValidationPlan) -> None:
    if plan.schema_version != SCHEMA_VERSION:
        raise ContractError(
            f"unsupported Validation plan version: {plan.schema_version}"
        )
    if plan.validation_implementation != VALIDATION_IMPLEMENTATION:
        raise ContractError("plan.validationImplementation is unsupported")
    validate_candidate(plan.candidate)
    if not plan.surfaces:
        raise ContractError("plan.changeSurfaces must not be empty")
    _strings(plan.surfaces, "plan.changeSurfaces")
    _strings(plan.risk_modifiers, "plan.riskModifiers")
    _strings(plan.policy_errors, "plan.policyErrors")
    if plan.profile not in PROFILES:
        raise ContractError("plan.profile is unsupported")
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
    expected_source = {
        "candidateSha": plan.candidate.candidate_sha,
        "baseSha": plan.candidate.base_sha or "",
        "headSha": plan.candidate.head_sha or "",
    }
    if any(source.get(name) != value for name, value in expected_source.items()):
        raise ContractError("plan fingerprint is bound to a different candidate")
    selected = ",".join(item.family for item in plan.requirements if item.selected)
    dependencies = dict(plan.fingerprint.dependencies)
    if dependencies.get("schemaVersion") != str(SCHEMA_VERSION):
        raise ContractError("plan fingerprint has an inconsistent schema version")
    if dependencies.get("selectedEvidence") != selected:
        raise ContractError(
            "plan fingerprint selected evidence does not match the plan"
        )
    expected_parameters = {
        "changeSurfaces": ",".join(plan.surfaces),
        "riskModifiers": ",".join(plan.risk_modifiers),
        "policyErrors": "\n".join(plan.policy_errors),
    }
    parameters = dict(plan.fingerprint.parameters)
    if any(
        parameters.get(name) != value for name, value in expected_parameters.items()
    ):
        raise ContractError("plan fingerprint parameters do not match the plan")
    expected_commands = tuple(
        f"validation:{item.family}" for item in plan.requirements if item.selected
    )
    if plan.fingerprint.commands != expected_commands:
        raise ContractError("plan fingerprint commands do not match the plan")


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
        schema_version=_integer(payload["schemaVersion"], "plan.schemaVersion"),
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


def serialize_plan(plan: ValidationPlan) -> str:
    validate_plan(plan)
    return _serialized(plan_to_dict(plan), "Validation plan")


def parse_plan(text: str) -> ValidationPlan:
    return plan_from_dict(_load(text, "Validation plan"))
