import json
from dataclasses import replace
import unittest

from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_contracts import SCHEMA_VERSION
from validation_contracts import VALIDATION_IMPLEMENTATION
from validation_contracts import ValidationFingerprint
from validation_plan_contract import EvidenceRequirement
from validation_plan_contract import ValidationPlan
from validation_plan_contract import parse_plan
from validation_plan_contract import plan_to_dict
from validation_plan_contract import serialize_plan


CANDIDATE_SHA = "a" * 40
BASE_SHA = "b" * 40
HEAD_SHA = "c" * 40
CHANGED_FILES_DIGEST = "d" * 64


def repository_only_plan() -> ValidationPlan:
    candidate = CandidateIdentity(
        event_name="pull_request",
        repository="Electivus/electivus-codex",
        default_branch="main",
        candidate_sha=CANDIDATE_SHA,
        base_sha=BASE_SHA,
        head_sha=HEAD_SHA,
        kind="pull-request",
        pull_request_number=198,
        branch="codex/issue-181-contracts-plan",
    )
    requirements = (
        EvidenceRequirement(
            family="repository-policy",
            stage="preflight",
            selected=True,
            disposition="required",
            reason="repository changes always require policy validation",
            retention_class="ordinary-pull-request",
        ),
        EvidenceRequirement(
            family="linux-x64-bazel",
            stage="merge",
            selected=False,
            disposition="not-required",
            reason="repository-only changes do not compile product code",
            retention_class="ordinary-pull-request",
        ),
    )
    fingerprint = ValidationFingerprint(
        source=(
            ("candidateSha", CANDIDATE_SHA),
            ("baseSha", BASE_SHA),
            ("headSha", HEAD_SHA),
        ),
        validation_implementation=VALIDATION_IMPLEMENTATION,
        dependencies=(
            ("schemaVersion", str(SCHEMA_VERSION)),
            ("selectedEvidence", "repository-policy"),
        ),
        toolchains=(("python", "3.11"),),
        commands=(("validation:repository-policy"),),
        platforms=(("linux-x64"),),
        profile="ordinary",
        parameters=(
            ("changeSurfaces", "repository"),
            ("riskModifiers", ""),
            ("policyErrors", ""),
        ),
        inputs=(("changedFilesDigest", CHANGED_FILES_DIGEST),),
    )
    return ValidationPlan(
        schema_version=SCHEMA_VERSION,
        validation_implementation=VALIDATION_IMPLEMENTATION,
        candidate=candidate,
        surfaces=("repository",),
        risk_modifiers=(),
        profile="ordinary",
        requirements=requirements,
        fingerprint=fingerprint,
    )


class ValidationContractTests(unittest.TestCase):
    def test_repository_only_plan_round_trips_as_one_exact_object(self) -> None:
        plan = repository_only_plan()

        self.assertEqual(plan, parse_plan(serialize_plan(plan)))
        self.assertEqual(
            {
                "schemaVersion": 1,
                "validationImplementation": "electivus-validation-v1",
                "candidate": {
                    "eventName": "pull_request",
                    "repository": "Electivus/electivus-codex",
                    "defaultBranch": "main",
                    "candidateSha": CANDIDATE_SHA,
                    "baseSha": BASE_SHA,
                    "headSha": HEAD_SHA,
                    "kind": "pull-request",
                    "pullRequestNumber": 198,
                    "branch": "codex/issue-181-contracts-plan",
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
                        "reason": "repository changes always require policy validation",
                        "retentionClass": "ordinary-pull-request",
                    },
                    {
                        "family": "linux-x64-bazel",
                        "stage": "merge",
                        "selected": False,
                        "disposition": "not-required",
                        "reason": "repository-only changes do not compile product code",
                        "retentionClass": "ordinary-pull-request",
                    },
                ],
                "fingerprint": {
                    "source": [
                        ["candidateSha", CANDIDATE_SHA],
                        ["baseSha", BASE_SHA],
                        ["headSha", HEAD_SHA],
                    ],
                    "validationImplementation": "electivus-validation-v1",
                    "dependencies": [
                        ["schemaVersion", "1"],
                        ["selectedEvidence", "repository-policy"],
                    ],
                    "toolchains": [["python", "3.11"]],
                    "commands": ["validation:repository-policy"],
                    "platforms": ["linux-x64"],
                    "profile": "ordinary",
                    "parameters": [
                        ["changeSurfaces", "repository"],
                        ["riskModifiers", ""],
                        ["policyErrors", ""],
                    ],
                    "inputs": [["changedFilesDigest", CHANGED_FILES_DIGEST]],
                    "digest": plan.fingerprint.digest,
                },
                "policyErrors": [],
            },
            plan_to_dict(plan),
        )

    def test_every_fingerprint_dimension_changes_the_digest(self) -> None:
        fingerprint = repository_only_plan().fingerprint
        mutations = (
            replace(fingerprint, source=fingerprint.source + (("tree", "e" * 40),)),
            replace(fingerprint, validation_implementation="electivus-validation-v2"),
            replace(
                fingerprint, dependencies=fingerprint.dependencies + (("lock", "1"),)
            ),
            replace(fingerprint, toolchains=(("python", "3.12"),)),
            replace(fingerprint, commands=("validation:repository-policy-v2",)),
            replace(fingerprint, platforms=("linux-arm64",)),
            replace(fingerprint, profile="certification-required"),
            replace(
                fingerprint, parameters=fingerprint.parameters + (("strict", "true"),)
            ),
            replace(fingerprint, inputs=(("changedFilesDigest", "e" * 64),)),
        )

        self.assertEqual(len(mutations), len({item.digest for item in mutations}))
        self.assertNotIn(fingerprint.digest, {item.digest for item in mutations})

    def test_mutated_plan_boundaries_fail_closed(self) -> None:
        payload = plan_to_dict(repository_only_plan())
        mutations = []
        for mutate in (
            lambda item: item.update(schemaVersion=2),
            lambda item: item["candidate"].update(candidateSha="f" * 40),
            lambda item: item["fingerprint"].update(digest="0" * 64),
            lambda item: item["fingerprint"]["dependencies"][1].__setitem__(1, ""),
            lambda item: item["evidence"][0].update(selected=False),
            lambda item: item.update(unexpected=True),
        ):
            mutated = json.loads(json.dumps(payload))
            mutate(mutated)
            mutations.append(mutated)

        for mutated in mutations:
            with self.subTest(mutated=mutated), self.assertRaises(ContractError):
                parse_plan(json.dumps(mutated))

    def test_malformed_candidate_json_and_oversized_output_fail_closed(self) -> None:
        plan = repository_only_plan()
        duplicate_version = serialize_plan(plan).replace(
            '  "schemaVersion": 1,',
            '  "schemaVersion": 1,\n  "schemaVersion": 1,',
            1,
        )
        invalid_candidate = replace(
            plan,
            candidate=replace(plan.candidate, head_sha=None),
        )
        oversized = replace(
            plan,
            policy_errors=("x" * 300_000,),
        )

        for operation in (
            lambda: parse_plan(duplicate_version),
            lambda: serialize_plan(invalid_candidate),
            lambda: serialize_plan(oversized),
        ):
            with self.subTest(operation=operation), self.assertRaises(ContractError):
                operation()


if __name__ == "__main__":
    unittest.main()
