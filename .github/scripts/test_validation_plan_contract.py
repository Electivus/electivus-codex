from dataclasses import replace
import json
import unittest

from validation_contracts import (
    CandidateIdentity,
    ContractError,
    VALIDATION_IMPLEMENTATION,
)
from validation_contracts import ValidationFingerprint, candidate_to_dict
from validation_plan_contract import EVIDENCE_FAMILIES, FAMILY_STAGES, MAX_EVIDENCE
from validation_plan_contract import MAX_PLAN_INPUT_BYTES, MAX_PLAN_ITEMS
from validation_plan_contract import EvidenceRequirement, ValidationPlan
from validation_plan_contract import parse_plan, plan_from_dict, plan_to_dict
from validation_plan_contract import serialize_plan, validate_plan


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


def requirement(family="repository-policy", selected=True, disposition="required"):
    return EvidenceRequirement(
        family,
        FAMILY_STAGES[family],
        selected,
        disposition,
        "bounded repository policy checks",
        "ordinary-pull-request",
    )


def evidence_ledger(selected_families=("repository-policy",)):
    selected = set(selected_families)
    return tuple(
        requirement(
            family,
            family in selected,
            "required" if family in selected else "not-required",
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
    def encode(values: tuple[str, ...]) -> str:
        return json.dumps(list(values), separators=(",", ":"))

    candidate_value = candidate()
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
            ("selectedEvidence", encode(selected_evidence)),
        ),
        (("python", "3.11"),),
        tuple(f"validation:{family}" for family in selected_evidence),
        ("linux-x64",),
        profile,
        parameters
        or (
            ("changeSurfaces", '["repository"]'),
            ("riskModifiers", encode(risk_modifiers)),
            ("policyErrors", encode(policy_errors)),
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
    requirements = requirements or evidence_ledger(selected_families)
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
    def test_repository_only_plan_round_trips_as_exact_object(self) -> None:
        plan = repository_only_plan()
        payload = plan_to_dict(plan)
        expected_evidence = [
            dict(
                family=family,
                stage=FAMILY_STAGES[family],
                selected=family == "repository-policy",
                disposition="required"
                if family == "repository-policy"
                else "not-required",
                reason="bounded repository policy checks",
                retentionClass="ordinary-pull-request",
            )
            for family in EVIDENCE_FAMILIES
        ]

        self.assertEqual(plan, parse_plan(serialize_plan(plan)))
        self.assertEqual(
            {
                "schemaVersion": 1,
                "validationImplementation": VALIDATION_IMPLEMENTATION,
                "candidate": candidate_to_dict(candidate()),
                "changeSurfaces": ["repository"],
                "riskModifiers": [],
                "profile": "ordinary",
                "evidence": expected_evidence,
                "fingerprint": payload["fingerprint"],
                "policyErrors": [],
            },
            payload,
        )

    def test_evidence_ledger_is_complete_canonical_and_bounded(self) -> None:
        plan = repository_only_plan()
        items = plan.requirements
        mutations = (
            replace(plan, requirements=items[:-1]),
            replace(plan, requirements=(*items[1:], items[0])),
            replace(plan, requirements=(items[0], items[0], *items[2:])),
            replace(plan, requirements=(*items, requirement("repository-policy"))),
            replace(plan, surfaces=("repository", "repository")),
            replace(plan, risk_modifiers=("unknown", "unknown")),
        )
        for mutated in mutations:
            assert_invalid(self, validate_plan, mutated)

        for count in (MAX_EVIDENCE, MAX_EVIDENCE + 1):
            assert_invalid(
                self,
                validate_plan,
                repository_only_plan(
                    requirements=tuple(requirement() for _ in range(count))
                ),
            )
        with self.assertRaisesRegex(ContractError, "invalid size"):
            plan_from_dict(
                {**plan_to_dict(plan), "evidence": [object()] * (MAX_EVIDENCE + 1)}
            )

    def test_each_family_rejects_a_wrong_stage(self) -> None:
        plan = repository_only_plan()
        for index, item in enumerate(plan.requirements):
            requirements = list(plan.requirements)
            requirements[index] = replace(
                item, stage="preflight" if item.stage != "preflight" else "merge"
            )
            assert_invalid(
                self, validate_plan, replace(plan, requirements=requirements)
            )

    def test_fingerprint_binding_is_bidirectional(self) -> None:
        plan = repository_only_plan()
        fingerprint_base = plan.fingerprint

        candidate_changes = (
            ("event_name", "workflow_dispatch"),
            ("repository", "Other/repo"),
            ("default_branch", "trunk"),
            ("candidate_sha", "f" * 40),
            ("base_sha", "e" * 40),
            ("head_sha", "f" * 40),
            ("kind", "integrated"),
            ("pull_request_number", 182),
            ("branch", "other-branch"),
        )
        for field, value in candidate_changes:
            assert_invalid(
                self,
                validate_plan,
                replace(plan, candidate=replace(plan.candidate, **{field: value})),
            )

        mutations = (
            with_fingerprint(
                plan, source=(("candidateSha", "f" * 40), *fingerprint_base.source[1:])
            ),
            with_fingerprint(
                plan,
                parameters=(
                    ("changeSurfaces", '["documentation"]'),
                    *fingerprint_base.parameters[1:],
                ),
            ),
            with_fingerprint(
                plan,
                dependencies=(("schemaVersion", "1"), ("selectedEvidence", "[]")),
                commands=(),
            ),
            with_fingerprint(plan, profile="certification-required"),
        )
        for mutated in mutations:
            assert_invalid(self, validate_plan, mutated)

    def test_unknown_and_missing_values_fail_closed(self) -> None:
        plan = repository_only_plan()
        mutations = (
            lambda item: item.update(schemaVersion=2),
            lambda item: item.update(validationImplementation="other"),
            lambda item: item.update(profile="unknown"),
            lambda item: item.update(changeSurfaces=[]),
            lambda item: item.update(changeSurfaces=["repository-documentation"]),
            lambda item: item.update(riskModifiers=["not-a-modifier"]),
            lambda item: item["evidence"][1].update(family="unknown-family"),
            lambda item: item["evidence"][1].update(disposition="optional"),
            lambda item: item["evidence"][1].update(retentionClass="forever"),
            lambda item: item["evidence"][1].update(selected=False),
            lambda item: item["evidence"][1].pop("reason"),
            lambda item: item["evidence"][1].update(stage="merg"),
            lambda item: item["evidence"][1].update(stage="code" + "ql"),
            lambda item: item.update(unexpected=True),
            lambda item: item["fingerprint"].pop("digest"),
        )

        for mutate in mutations:
            mutated = json.loads(json.dumps(plan_to_dict(plan)))
            mutate(mutated)
            assert_invalid(self, plan_from_dict, mutated)
        uncertain = {
            "risk_modifiers": ("unknown",),
            "policy_errors": ("classifier uncertainty",),
        }
        assert_invalid(self, validate_plan, repository_only_plan(**uncertain))
        validate_plan(certified_plan(**uncertain))

    def test_arrays_are_not_delimited_strings(self) -> None:
        errors = ("path contains comma, safely", "second\tmessage")
        encoded = json.dumps(list(errors), separators=(",", ":"))
        plan = certified_plan(policy_errors=errors)
        plan = with_fingerprint(
            plan,
            parameters=(
                ("changeSurfaces", '["repository"]'),
                ("riskModifiers", "[]"),
                ("policyErrors", encoded),
            ),
        )
        validate_plan(plan)

        delimiter = with_fingerprint(
            plan,
            parameters=(
                ("changeSurfaces", "repository,documentation"),
                *plan.fingerprint.parameters[1:],
            ),
        )
        assert_invalid(self, validate_plan, delimiter)

    def test_strict_json_rejects_bad_values(self) -> None:
        serialized = serialize_plan(repository_only_plan())
        invalids = (
            serialized.replace(
                '"schemaVersion": 1,', '"schemaVersion": 1,\n"schemaVersion": 1,'
            ),
            serialized.replace('"schemaVersion": 1', '"schemaVersion": NaN', 1),
            b"\xff",
            serialized.replace("Electivus/electivus-codex", r"\ud800", 1),
        )
        for invalid in invalids:
            assert_invalid(self, parse_plan, invalid)

    def test_input_output_and_aggregate_budgets_are_bounded(self) -> None:
        plan = repository_only_plan()
        assert_invalid(
            self, parse_plan, serialize_plan(plan) + " " * MAX_PLAN_INPUT_BYTES
        )

        too_many_items = with_fingerprint(
            plan,
            inputs=tuple((f"input-{index}", "") for index in range(MAX_PLAN_ITEMS)),
        )
        assert_invalid(self, validate_plan, too_many_items)

        long_errors = tuple(f"error-{index}-" + "x" * 4_000 for index in range(64))
        large = certified_plan(policy_errors=long_errors)
        encoded = json.dumps(list(long_errors), separators=(",", ":"))
        large = with_fingerprint(
            large,
            parameters=(
                ("changeSurfaces", '["repository"]'),
                ("riskModifiers", "[]"),
                ("policyErrors", encoded),
            ),
        )
        assert_invalid(self, serialize_plan, large)
