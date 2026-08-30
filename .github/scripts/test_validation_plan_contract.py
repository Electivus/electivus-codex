from dataclasses import replace
import json
import unittest

from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_contracts import VALIDATION_IMPLEMENTATION
from validation_contracts import ValidationFingerprint
from validation_plan_contract import CHANGE_SURFACES
from validation_plan_contract import MAX_EVIDENCE
from validation_plan_contract import MAX_PLAN_INPUT_BYTES
from validation_plan_contract import MAX_PLAN_ITEMS
from validation_plan_contract import RISK_MODIFIERS
from validation_plan_contract import EvidenceRequirement
from validation_plan_contract import ValidationPlan
from validation_plan_contract import parse_plan
from validation_plan_contract import plan_from_dict
from validation_plan_contract import plan_to_dict
from validation_plan_contract import serialize_plan
from validation_plan_contract import validate_plan


CANDIDATE_SHA = "a" * 40
BASE_SHA = "b" * 40
HEAD_SHA = "c" * 40
CHANGED_FILES_DIGEST = "d" * 64


def candidate() -> CandidateIdentity:
    return CandidateIdentity(
        event_name="pull_request",
        repository="Electivus/electivus-codex",
        default_branch="main",
        candidate_sha=CANDIDATE_SHA,
        base_sha=BASE_SHA,
        head_sha=HEAD_SHA,
        kind="pull-request",
        pull_request_number=181,
        branch="codex/issue-181-plan-contract-impl",
    )


def requirement(
    family: str = "repository-policy",
    *,
    selected: bool = True,
    disposition: str = "required",
    retention_class: str = "ordinary-pull-request",
) -> EvidenceRequirement:
    return EvidenceRequirement(
        family=family,
        stage="preflight",
        selected=selected,
        disposition=disposition,
        reason="bounded repository policy checks",
        retention_class=retention_class,
    )


def fingerprint(
    *,
    source: tuple[tuple[str, str], ...] | None = None,
    dependencies: tuple[tuple[str, str], ...] | None = None,
    parameters: tuple[tuple[str, str], ...] | None = None,
    commands: tuple[str, ...] = ("validation:repository-policy",),
    profile: str = "ordinary",
) -> ValidationFingerprint:
    return ValidationFingerprint(
        source=source
        or (
            ("candidateSha", CANDIDATE_SHA),
            ("baseSha", BASE_SHA),
            ("headSha", HEAD_SHA),
        ),
        validation_implementation=VALIDATION_IMPLEMENTATION,
        dependencies=dependencies
        or (
            ("schemaVersion", "1"),
            ("selectedEvidence", '["repository-policy"]'),
        ),
        toolchains=(("python", "3.11"),),
        commands=commands,
        platforms=("linux-x64",),
        profile=profile,
        parameters=parameters
        or (
            ("changeSurfaces", '["repository"]'),
            ("riskModifiers", "[]"),
            ("policyErrors", "[]"),
        ),
        inputs=(("changedFilesDigest", CHANGED_FILES_DIGEST),),
    )


def repository_only_plan(
    *,
    surfaces: tuple[str, ...] = ("repository",),
    risk_modifiers: tuple[str, ...] = (),
    requirements: tuple[EvidenceRequirement, ...] = (requirement(),),
    policy_errors: tuple[str, ...] = (),
    plan_profile: str = "ordinary",
    plan_fingerprint: ValidationFingerprint | None = None,
) -> ValidationPlan:
    return ValidationPlan(
        schema_version=1,
        validation_implementation=VALIDATION_IMPLEMENTATION,
        candidate=candidate(),
        surfaces=surfaces,
        risk_modifiers=risk_modifiers,
        profile=plan_profile,
        requirements=requirements,
        fingerprint=plan_fingerprint or fingerprint(profile=plan_profile),
        policy_errors=policy_errors,
    )


class ValidationPlanContractTests(unittest.TestCase):
    def test_repository_only_plan_round_trips_as_one_exact_object(self) -> None:
        plan = repository_only_plan()
        payload = plan_to_dict(plan)

        self.assertEqual(plan, parse_plan(serialize_plan(plan)))
        self.assertEqual(
            {
                "schemaVersion": 1,
                "validationImplementation": VALIDATION_IMPLEMENTATION,
                "candidate": {
                    "eventName": "pull_request",
                    "repository": "Electivus/electivus-codex",
                    "defaultBranch": "main",
                    "candidateSha": CANDIDATE_SHA,
                    "baseSha": BASE_SHA,
                    "headSha": HEAD_SHA,
                    "kind": "pull-request",
                    "pullRequestNumber": 181,
                    "branch": "codex/issue-181-plan-contract-impl",
                },
                "changeSurfaces": ["repository"],
                "riskModifiers": [],
                "profile": "ordinary",
                "evidence": [
                    {
                        "family": "repository-policy",
                        "stage": "preflight",
                        "selected": True,
                        "disposition": "required",
                        "reason": "bounded repository policy checks",
                        "retentionClass": "ordinary-pull-request",
                    }
                ],
                "fingerprint": payload["fingerprint"],
                "policyErrors": [],
            },
            payload,
        )

    def test_revised_vocabulary_and_explicit_unknown_modifier_are_supported(
        self,
    ) -> None:
        self.assertTrue(
            {"repository", "api/protocol/SDK", "Runtime State/Postgres"}
            <= set(CHANGE_SURFACES)
        )
        self.assertIn("unknown", RISK_MODIFIERS)

        plan = repository_only_plan(
            risk_modifiers=("unknown",),
            policy_errors=("classifier uncertainty",),
            plan_fingerprint=fingerprint(
                parameters=(
                    ("changeSurfaces", '["repository"]'),
                    ("riskModifiers", '["unknown"]'),
                    ("policyErrors", '["classifier uncertainty"]'),
                )
            ),
        )
        validate_plan(plan)

    def test_fingerprint_binding_is_bidirectional(self) -> None:
        plan = repository_only_plan()
        normal_parameters = fingerprint().parameters
        mutations = (
            replace(
                plan,
                candidate=replace(plan.candidate, candidate_sha="f" * 40),
            ),
            replace(
                plan,
                fingerprint=fingerprint(
                    source=(("candidateSha", "f" * 40), *plan.fingerprint.source[1:])
                ),
            ),
            replace(
                plan,
                fingerprint=fingerprint(
                    parameters=(
                        ("changeSurfaces", '["documentation"]'),
                        *normal_parameters[1:],
                    )
                ),
            ),
            replace(
                plan,
                fingerprint=fingerprint(
                    dependencies=(
                        ("schemaVersion", "1"),
                        ("selectedEvidence", "[]"),
                    ),
                    commands=(),
                ),
            ),
            replace(
                plan,
                fingerprint=fingerprint(
                    profile="certification-required",
                ),
            ),
        )
        for mutated in mutations:
            with self.subTest(mutated=mutated), self.assertRaises(ContractError):
                validate_plan(mutated)

    def test_unknown_and_missing_values_fail_closed(self) -> None:
        plan = repository_only_plan()
        mutations = (
            lambda item: item.update(schemaVersion=2),
            lambda item: item.update(validationImplementation="other"),
            lambda item: item.update(profile="unknown"),
            lambda item: item.update(changeSurfaces=[]),
            lambda item: item.update(changeSurfaces=["repository-documentation"]),
            lambda item: item.update(riskModifiers=["not-a-modifier"]),
            lambda item: item["evidence"][0].update(family="unknown-family"),
            lambda item: item["evidence"][0].update(disposition="optional"),
            lambda item: item["evidence"][0].update(retentionClass="forever"),
            lambda item: item["evidence"][0].update(selected=False),
            lambda item: item["evidence"][0].pop("reason"),
            lambda item: item.update(unexpected=True),
            lambda item: item["fingerprint"].pop("digest"),
        )

        for mutate in mutations:
            mutated = json.loads(json.dumps(plan_to_dict(plan)))
            mutate(mutated)
            with self.subTest(mutated=mutated), self.assertRaises(ContractError):
                plan_from_dict(mutated)

    def test_duplicate_families_and_arrays_fail_closed(self) -> None:
        invalid_plans = (
            repository_only_plan(requirements=(requirement(), requirement())),
            repository_only_plan(surfaces=("repository", "repository")),
            repository_only_plan(risk_modifiers=("unknown", "unknown")),
        )
        for invalid in invalid_plans:
            with self.subTest(invalid=invalid), self.assertRaises(ContractError):
                validate_plan(invalid)

    def test_evidence_limit_is_checked_before_duplicate_families(self) -> None:
        for count, message in (
            (MAX_EVIDENCE, "duplicate family"),
            (MAX_EVIDENCE + 1, "invalid size"),
        ):
            with self.assertRaisesRegex(ContractError, message):
                validate_plan(
                    repository_only_plan(
                        requirements=tuple(requirement() for _ in range(count))
                    )
                )

    def test_arrays_are_not_delimited_strings(self) -> None:
        plan = repository_only_plan(
            policy_errors=("path contains comma, safely", "second\tmessage"),
            plan_fingerprint=fingerprint(
                parameters=(
                    ("changeSurfaces", '["repository"]'),
                    ("riskModifiers", "[]"),
                    (
                        "policyErrors",
                        '["path contains comma, safely","second\\tmessage"]',
                    ),
                )
            ),
        )
        validate_plan(plan)
        self.assertEqual(
            '["path contains comma, safely","second\\tmessage"]',
            dict(plan.fingerprint.parameters)["policyErrors"],
        )

        delimiter = replace(
            plan,
            fingerprint=fingerprint(
                parameters=(
                    ("changeSurfaces", "repository,documentation"),
                    ("riskModifiers", "[]"),
                    ("policyErrors", "[]"),
                )
            ),
        )
        with self.assertRaises(ContractError):
            validate_plan(delimiter)

    def test_strict_json_rejects_duplicates_nan_invalid_utf8_and_surrogates(
        self,
    ) -> None:
        plan = repository_only_plan()
        serialized = serialize_plan(plan)
        duplicate = serialized.replace(
            '  "schemaVersion": 1,',
            '  "schemaVersion": 1,\n  "schemaVersion": 1,',
            1,
        )
        malformed = serialized.replace('"schemaVersion": 1', '"schemaVersion": NaN', 1)
        for invalid in (duplicate, malformed, b"\xff"):
            with self.subTest(invalid=invalid), self.assertRaises(ContractError):
                parse_plan(invalid)

        escaped_surrogate = serialized.replace(
            '"repository": "Electivus/electivus-codex"', r'"repository": "\ud800"', 1
        )
        with self.assertRaises(ContractError):
            parse_plan(escaped_surrogate)

    def test_input_output_and_aggregate_budgets_are_bounded(self) -> None:
        plan = repository_only_plan()
        with self.assertRaises(ContractError):
            parse_plan(serialize_plan(plan) + " " * MAX_PLAN_INPUT_BYTES)

        too_many_items = replace(
            plan,
            fingerprint=replace(
                plan.fingerprint,
                inputs=tuple((f"input-{index}", "") for index in range(MAX_PLAN_ITEMS)),
            ),
        )
        with self.assertRaises(ContractError):
            validate_plan(too_many_items)

        long_errors = tuple(f"error-{index}-" + "x" * 4_000 for index in range(64))
        large = repository_only_plan(
            policy_errors=long_errors,
            plan_fingerprint=fingerprint(
                parameters=(
                    ("changeSurfaces", '["repository"]'),
                    ("riskModifiers", "[]"),
                    (
                        "policyErrors",
                        json.dumps(list(long_errors), separators=(",", ":")),
                    ),
                )
            ),
        )
        with self.assertRaises(ContractError):
            serialize_plan(large)


if __name__ == "__main__":
    unittest.main()
