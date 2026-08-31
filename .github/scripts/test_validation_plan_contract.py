from dataclasses import replace
import unittest

from validation_contracts import (
    CandidateIdentity,
    ContractError,
    VALIDATION_IMPLEMENTATION,
    ValidationFingerprint,
    candidate_to_dict,
    canonical_json,
)
from validation_plan_contract import (
    CERTIFICATION_REQUIRED_STAGES,
    EVIDENCE_FAMILIES,
    FAMILY_PLATFORMS,
    FAMILY_STAGES,
    MAX_EVIDENCE,
    PLATFORM_ORDER,
    RETENTION_BY_PROFILE_STAGE,
    EvidenceRequirement,
    ValidationPlan,
    plan_from_dict,
    plan_to_dict,
    validate_plan,
)


CANDIDATE_SHA = "a" * 40
BASE_SHA = "b" * 40
HEAD_SHA = "c" * 40
CHANGED_FILES_DIGEST = "d" * 64


def candidate() -> CandidateIdentity:
    return CandidateIdentity(
        "pull_request",
        "Electivus/electivus-codex",
        "main",
        CANDIDATE_SHA,
        BASE_SHA,
        HEAD_SHA,
        "pull-request",
        181,
        "codex/issue-181-plan-contract-impl",
    )


def requirement(
    family="repository-policy",
    selected=True,
    disposition="required",
    profile="ordinary",
):
    return EvidenceRequirement(
        family,
        FAMILY_STAGES[family],
        selected,
        disposition,
        "bounded repository policy checks",
        RETENTION_BY_PROFILE_STAGE[(profile, FAMILY_STAGES[family])],
    )


def evidence_ledger(selected_families=("repository-policy",), profile="ordinary"):
    selected = set(selected_families)
    return tuple(
        requirement(
            family,
            family in selected,
            "required" if family in selected else "not-required",
            profile,
        )
        for family in EVIDENCE_FAMILIES
    )


def fingerprint(
    *,
    source: tuple[tuple[str, str], ...] | None = None,
    dependencies: tuple[tuple[str, str], ...] | None = None,
    parameters: tuple[tuple[str, str], ...] | None = None,
    profile: str = "ordinary",
    selected_evidence: tuple[str, ...] = ("repository-policy",),
    risk_modifiers: tuple[str, ...] = (),
    policy_errors: tuple[str, ...] = (),
) -> ValidationFingerprint:
    candidate_value = candidate()
    selected_platforms = frozenset(
        platform
        for family in selected_evidence
        for platform in FAMILY_PLATFORMS[family]
    )
    return ValidationFingerprint(
        source
        or tuple(
            (name, "" if value is None else str(value))
            for name, value in candidate_to_dict(candidate_value).items()
        ),
        VALIDATION_IMPLEMENTATION,
        dependencies
        or (
            ("schemaVersion", "1"),
            ("selectedEvidence", canonical_json(list(selected_evidence))),
        ),
        (("python", "3.11"),),
        tuple(f"validation:{family}" for family in selected_evidence),
        tuple(
            platform for platform in PLATFORM_ORDER if platform in selected_platforms
        ),
        profile,
        parameters
        or (
            ("changeSurfaces", '["repository"]'),
            ("riskModifiers", canonical_json(list(risk_modifiers))),
            ("policyErrors", canonical_json(list(policy_errors))),
        ),
        (("changedFilesDigest", CHANGED_FILES_DIGEST),),
    )


def repository_only_plan(
    *,
    surfaces: tuple[str, ...] = ("repository",),
    risk_modifiers: tuple[str, ...] = (),
    requirements: tuple[EvidenceRequirement, ...] | None = None,
    policy_errors: tuple[str, ...] = (),
    plan_profile: str = "ordinary",
    plan_fingerprint: ValidationFingerprint | None = None,
    selected_families: tuple[str, ...] = ("repository-policy",),
) -> ValidationPlan:
    if requirements is None:
        requirements = evidence_ledger(selected_families, plan_profile)
    selected = tuple(item.family for item in requirements if item.selected)
    return ValidationPlan(
        1,
        VALIDATION_IMPLEMENTATION,
        candidate(),
        surfaces,
        risk_modifiers,
        plan_profile,
        requirements,
        plan_fingerprint
        or fingerprint(
            profile=plan_profile,
            selected_evidence=selected,
            risk_modifiers=risk_modifiers,
            policy_errors=policy_errors,
        ),
        policy_errors,
    )


def certified_plan(
    *,
    risk_modifiers: tuple[str, ...] = (),
    policy_errors: tuple[str, ...] = (),
    fingerprint_override: ValidationFingerprint | None = None,
) -> ValidationPlan:
    return repository_only_plan(
        risk_modifiers=risk_modifiers,
        policy_errors=policy_errors,
        plan_profile="certification-required",
        selected_families=EVIDENCE_FAMILIES,
        plan_fingerprint=fingerprint_override,
    )


def with_fingerprint(plan: ValidationPlan, **changes: object) -> ValidationPlan:
    return replace(plan, fingerprint=replace(plan.fingerprint, **changes))


def assert_invalid(testcase, function, value):
    testcase.assertRaises(ContractError, function, value)


class ValidationPlanContractTests(unittest.TestCase):
    def test_plan_projection_round_trips_as_exact_object(self) -> None:
        plan = repository_only_plan()
        self.assertEqual(plan, plan_from_dict(plan_to_dict(plan)))
        self.assertEqual(
            tuple(item.family for item in plan.requirements), EVIDENCE_FAMILIES
        )
        self.assertEqual(plan.requirements[0].disposition, "not-required")

    def test_evidence_ledger_is_complete_bounded_and_selected(self) -> None:
        plan = repository_only_plan()
        items = plan.requirements
        unselected = repository_only_plan(selected_families=())
        mutations = (
            replace(plan, requirements=items[:-1]),
            replace(plan, requirements=(*items[1:], items[0])),
            replace(plan, requirements=(items[0], items[0], *items[2:])),
            replace(
                plan, requirements=tuple(requirement() for _ in range(MAX_EVIDENCE))
            ),
            unselected,
        )
        for mutated in mutations:
            assert_invalid(self, validate_plan, mutated)
        self.assertTrue(all(not item.selected for item in unselected.requirements))
        self.assertTrue(
            all(item.disposition == "not-required" for item in unselected.requirements)
        )
        validate_plan(plan)

    def test_certification_is_required_for_risks_stages_and_policy_errors(self) -> None:
        for modifier in (
            "security",
            "breaking",
            "migration",
            "publication",
            "validation-authority",
            "synchronization",
            "unknown",
        ):
            with self.subTest(modifier=modifier):
                assert_invalid(
                    self,
                    validate_plan,
                    repository_only_plan(risk_modifiers=(modifier,)),
                )
                validate_plan(certified_plan(risk_modifiers=(modifier,)))

        assert_invalid(
            self,
            validate_plan,
            repository_only_plan(policy_errors=("classifier uncertainty",)),
        )
        validate_plan(certified_plan(policy_errors=("classifier uncertainty",)))
        assert_invalid(
            self,
            validate_plan,
            repository_only_plan(
                plan_profile="certification-required",
                selected_families=("repository-policy",),
                policy_errors=("classifier uncertainty",),
            ),
        )
        for family, stage in FAMILY_STAGES.items():
            if stage in CERTIFICATION_REQUIRED_STAGES:
                assert_invalid(
                    self,
                    validate_plan,
                    repository_only_plan(selected_families=(family,)),
                )

    def test_platforms_are_closed_and_bound_to_selected_families(self) -> None:
        plan = repository_only_plan()
        for platforms in (("macos",), ("linux-x64", "arbitrary")):
            assert_invalid(
                self, validate_plan, with_fingerprint(plan, platforms=platforms)
            )
        arm_plan = repository_only_plan(
            plan_profile="certification-required", selected_families=("linux-arm64",)
        )
        assert_invalid(
            self, validate_plan, with_fingerprint(arm_plan, platforms=("linux-x64",))
        )
        release = repository_only_plan(
            plan_profile="certification-required",
            selected_families=("release-packaging",),
        )
        self.assertEqual(PLATFORM_ORDER, release.fingerprint.platforms)
        validate_plan(release)

    def test_retention_is_bound_to_stage_and_profile(self) -> None:
        for plan in (repository_only_plan(), certified_plan()):
            validate_plan(plan)
            self.assertEqual(
                tuple(
                    RETENTION_BY_PROFILE_STAGE[(plan.profile, item.stage)]
                    for item in plan.requirements
                ),
                tuple(item.retention_class for item in plan.requirements),
            )
        certification = certified_plan()
        integrated = next(
            item for item in certification.requirements if item.family == "linux-arm64"
        )
        self.assertEqual("integrated-certification", integrated.retention_class)
        validate_plan(
            replace(
                certification,
                requirements=tuple(
                    replace(item, retention_class="test-reactivation-certification")
                    if item.family == integrated.family
                    else item
                    for item in certification.requirements
                ),
            )
        )
        ordinary = repository_only_plan()
        invalid = replace(ordinary.requirements[1], retention_class="intra-run")
        assert_invalid(
            self,
            validate_plan,
            replace(
                ordinary,
                requirements=(
                    *ordinary.requirements[:1],
                    invalid,
                    *ordinary.requirements[2:],
                ),
            ),
        )

    def test_fingerprint_binding_is_bidirectional_and_canonical(self) -> None:
        plan = repository_only_plan()
        base = plan.fingerprint
        mutations = (
            with_fingerprint(
                plan, source=(("candidateSha", "f" * 40), *base.source[1:])
            ),
            with_fingerprint(
                plan,
                parameters=(
                    ("changeSurfaces", '["documentation"]'),
                    *base.parameters[1:],
                ),
            ),
            with_fingerprint(
                plan,
                dependencies=(("schemaVersion", "1"), ("selectedEvidence", "[]")),
                commands=(),
            ),
            with_fingerprint(plan, profile="certification-required"),
            with_fingerprint(
                plan,
                parameters=(
                    ("changeSurfaces", "repository,documentation"),
                    *base.parameters[1:],
                ),
            ),
        )
        for mutated in mutations:
            assert_invalid(self, validate_plan, mutated)
