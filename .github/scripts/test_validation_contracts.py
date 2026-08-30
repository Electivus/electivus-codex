from dataclasses import replace
import unittest

from validation_contracts import MAX_ITEMS
from validation_contracts import VALIDATION_IMPLEMENTATION
from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_contracts import ValidationFingerprint
from validation_contracts import candidate_from_dict
from validation_contracts import candidate_to_dict
from validation_contracts import fingerprint_from_dict
from validation_contracts import fingerprint_to_dict
from validation_contracts import validate_candidate
from validation_contracts import validate_fingerprint


CANDIDATE_SHA = "a" * 40
BASE_SHA = "b" * 40
HEAD_SHA = "c" * 40
CHANGED_FILES_DIGEST = "d" * 64
FINGERPRINT_DIGEST = "71166692cad302a26273dd968e584da3eeb05cf6a288a76bc8db7199ef29da56"


def pull_request_candidate() -> CandidateIdentity:
    return CandidateIdentity(
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


def validation_fingerprint() -> ValidationFingerprint:
    return ValidationFingerprint(
        source=(
            ("candidateSha", CANDIDATE_SHA),
            ("baseSha", BASE_SHA),
            ("headSha", HEAD_SHA),
        ),
        validation_implementation=VALIDATION_IMPLEMENTATION,
        dependencies=(
            ("schemaVersion", "1"),
            ("selectedEvidence", "repository-policy"),
        ),
        toolchains=(("python", "3.11"),),
        commands=("validation:repository-policy",),
        platforms=("linux-x64",),
        profile="ordinary",
        parameters=(
            ("changeSurfaces", '["repository"]'),
            ("riskModifiers", "[]"),
            ("policyErrors", "[]"),
        ),
        inputs=(("changedFilesDigest", CHANGED_FILES_DIGEST),),
    )


class ValidationContractTests(unittest.TestCase):
    def test_candidate_and_fingerprint_have_exact_versioned_objects(self) -> None:
        candidate = pull_request_candidate()
        fingerprint = validation_fingerprint()
        expected_candidate = {
            "eventName": "pull_request",
            "repository": "Electivus/electivus-codex",
            "defaultBranch": "main",
            "candidateSha": CANDIDATE_SHA,
            "baseSha": BASE_SHA,
            "headSha": HEAD_SHA,
            "kind": "pull-request",
            "pullRequestNumber": 198,
            "branch": "codex/issue-181-contracts-plan",
        }
        expected_fingerprint = {
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
                ["changeSurfaces", '["repository"]'],
                ["riskModifiers", "[]"],
                ["policyErrors", "[]"],
            ],
            "inputs": [["changedFilesDigest", CHANGED_FILES_DIGEST]],
            "digest": FINGERPRINT_DIGEST,
        }

        self.assertEqual(expected_candidate, candidate_to_dict(candidate))
        self.assertEqual(candidate, candidate_from_dict(expected_candidate))
        self.assertEqual(expected_fingerprint, fingerprint_to_dict(fingerprint))
        self.assertEqual(fingerprint, fingerprint_from_dict(expected_fingerprint))

    def test_every_fingerprint_dimension_changes_the_pinned_digest(self) -> None:
        fingerprint = validation_fingerprint()
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

        self.assertEqual(FINGERPRINT_DIGEST, fingerprint.digest)
        self.assertEqual(len(mutations), len({item.digest for item in mutations}))
        self.assertNotIn(FINGERPRINT_DIGEST, {item.digest for item in mutations})

    def test_candidate_identity_guards_are_reached_directly(self) -> None:
        candidate = pull_request_candidate()
        invalid_candidates = (
            replace(candidate, candidate_sha="A" * 40),
            replace(candidate, candidate_sha="a" * 39),
            replace(candidate, repository="x" * 4_097),
            replace(candidate, pull_request_number=0),
            replace(candidate, pull_request_number=True),
            replace(candidate, kind="unknown"),
            replace(candidate, head_sha=None),
            replace(candidate, kind="integrated"),
        )

        for invalid in invalid_candidates:
            with self.subTest(invalid=invalid), self.assertRaises(ContractError):
                validate_candidate(invalid)

    def test_text_rejects_lone_surrogates_as_contract_errors(self) -> None:
        invalid = replace(pull_request_candidate(), repository="\ud800")

        with self.assertRaisesRegex(
            ContractError, "candidate.repository must be valid UTF-8"
        ):
            validate_candidate(invalid)

    def test_fingerprint_guards_reject_versions_digests_and_duplicate_keys(
        self,
    ) -> None:
        fingerprint = validation_fingerprint()
        unsupported = replace(
            fingerprint,
            validation_implementation="electivus-validation-v2",
        )
        wrong_digest = fingerprint_to_dict(fingerprint)
        wrong_digest["digest"] = "0" * 64
        duplicate_source = replace(
            fingerprint,
            source=fingerprint.source + (("candidateSha", "e" * 40),),
        )
        unknown_profile = replace(fingerprint, profile="unknown")

        for operation in (
            lambda: fingerprint_from_dict(fingerprint_to_dict(unsupported)),
            lambda: fingerprint_from_dict(wrong_digest),
            lambda: validate_fingerprint(duplicate_source),
            lambda: validate_fingerprint(unknown_profile),
        ):
            with self.subTest(operation=operation), self.assertRaises(ContractError):
                operation()

    def test_fingerprint_item_budget_accepts_limit_and_rejects_over_limit(self) -> None:
        fingerprint = validation_fingerprint()
        at_limit = replace(
            fingerprint,
            inputs=tuple((f"input-{index}", "") for index in range(MAX_ITEMS)),
        )
        over_limit = replace(
            fingerprint,
            inputs=at_limit.inputs + (("one-too-many", ""),),
        )

        validate_fingerprint(at_limit)
        with self.assertRaises(ContractError):
            validate_fingerprint(over_limit)


if __name__ == "__main__":
    unittest.main()
