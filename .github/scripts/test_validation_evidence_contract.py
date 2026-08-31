from dataclasses import replace
import unittest

from test_validation_plan_contract import repository_only_plan
from validation_contracts import ContractError
from validation_evidence_contract import (
    MAX_ARTIFACTS_PER_MANIFEST,
    MAX_ATTEMPT,
    MAX_DURATION_SECONDS,
    EvidenceManifest,
    manifest_for_requirement,
    manifest_from_dict,
    manifest_to_dict,
    parse_manifest,
    serialize_manifest,
    validate_manifest,
    validate_manifest_against_plan,
)


DIGEST = "b" * 64


def fixture() -> tuple[object, EvidenceManifest]:
    plan = repository_only_plan()
    return plan, manifest_for_requirement(
        plan,
        plan.requirements[1],
        producer="repository-policy",
        artifact_digests=(("report.json", DIGEST),),
        duration_seconds=8,
        critical_path_seconds=5,
        created_at=1_700_000_000,
    )


def invalid(test, function, *values):
    for value in values:
        with test.subTest(value=value), test.assertRaises(ContractError):
            function(value)


class EvidenceManifestContractTests(unittest.TestCase):
    def test_round_trip_and_whole_object_projection(self):
        plan, manifest = fixture()
        payload = manifest_to_dict(manifest)
        self.assertEqual(manifest, manifest_from_dict(payload))
        self.assertEqual(manifest, parse_manifest(serialize_manifest(manifest)))
        self.assertEqual(plan.candidate, manifest.candidate)
        self.assertEqual(plan.fingerprint, manifest.fingerprint)
        self.assertEqual(
            {"family": "repository-policy", "name": "report.json", "digest": DIGEST},
            payload["artifactDigests"]
            and {
                "family": manifest.family,
                "name": payload["artifactDigests"][0][0],
                "digest": payload["artifactDigests"][0][1],
            },
        )

    def test_plan_and_deep_identity_bindings_fail_closed(self):
        plan, manifest = fixture()

        def against_plan(value):
            validate_manifest_against_plan(value, plan)

        candidate = manifest.candidate
        candidate_changes = (
            replace(candidate, event_name="workflow_dispatch"),
            replace(candidate, repository="Other/repo"),
            replace(candidate, default_branch="trunk"),
            replace(candidate, candidate_sha="d" * 40),
            replace(candidate, base_sha="e" * 40),
            replace(candidate, head_sha="f" * 40),
            replace(candidate, kind="integrated", pull_request_number=None),
            replace(candidate, pull_request_number=182),
            replace(candidate, branch="other-branch"),
        )
        invalid(
            self,
            against_plan,
            *(replace(manifest, candidate=value) for value in candidate_changes),
        )

        fingerprint = manifest.fingerprint
        fingerprint_changes = (
            replace(
                fingerprint, source=(("eventName", "other"), *fingerprint.source[1:])
            ),
            replace(fingerprint, validation_implementation="other"),
            replace(
                fingerprint,
                dependencies=(
                    ("schemaVersion", "1"),
                    ("selectedEvidence", "[]"),
                ),
            ),
            replace(fingerprint, toolchains=(("python", "3.12"),)),
            replace(fingerprint, commands=("validation:other",)),
            replace(fingerprint, platforms=("windows-x64",)),
            replace(fingerprint, profile="certification-required"),
            replace(
                fingerprint,
                parameters=(
                    ("changeSurfaces", '["documentation"]'),
                    *fingerprint.parameters[1:],
                ),
            ),
            replace(fingerprint, inputs=(("changedFilesDigest", "c" * 64),)),
        )
        invalid(
            self,
            against_plan,
            *(replace(manifest, fingerprint=value) for value in fingerprint_changes),
        )

        requirement = plan.requirements[1]
        self.assertEqual(
            (
                requirement.family,
                requirement.stage,
                requirement.disposition,
                requirement.retention_class,
            ),
            (
                manifest.family,
                manifest.stage,
                manifest.disposition,
                manifest.retention_class,
            ),
        )
        invalid(
            self,
            against_plan,
            replace(manifest, family="rust-fast", stage="merge"),
            replace(manifest, stage="merge"),
            replace(manifest, disposition="not-required"),
            replace(manifest, retention_class="integrated-certification"),
            replace(manifest, evidence_id="tampered"),
        )

    def test_outcomes_sentinels_and_provenance(self):
        plan, manifest = fixture()

        for outcome in (
            "passed",
            "product-failure",
            "infrastructure-failure",
            "indeterminate",
            "stale",
        ):
            validate_manifest_against_plan(
                manifest_for_requirement(plan, plan.requirements[1], outcome=outcome),
                plan,
            )
        invalid(
            self,
            validate_manifest,
            replace(manifest, outcome="not-required"),
            replace(manifest, outcome="unknown"),
            replace(manifest, family="unknown"),
            replace(manifest, disposition="optional"),
            replace(manifest, retention_class="unknown"),
            replace(manifest, cache_mode="unknown"),
        )

        sentinel = manifest_for_requirement(plan, plan.requirements[0])

        def against_plan(value):
            validate_manifest_against_plan(value, plan)

        self.assertIsNone(sentinel.producer)
        self.assertEqual("not-required", sentinel.outcome)
        invalid(
            self,
            against_plan,
            replace(sentinel, producer="producer"),
            replace(sentinel, outcome="passed"),
            replace(sentinel, artifact_digests=(("x", DIGEST),)),
            replace(sentinel, duration_seconds=1),
            replace(sentinel, critical_path_seconds=1),
            replace(sentinel, attempt=2),
            replace(sentinel, cache_mode="cold"),
            replace(sentinel, created_at=1),
            replace(sentinel, reason="wrong sentinel reason"),
        )

        for cache_mode in (
            "not-used",
            "cold",
            "cache-hit-verified",
            "disabled-reconstruction",
            "cache-only",
        ):
            cached = manifest_for_requirement(
                plan, plan.requirements[1], cache_mode=cache_mode
            )
            validate_manifest_against_plan(cached, plan)
            self.assertEqual(cached, parse_manifest(serialize_manifest(cached)))

    def test_artifacts_durations_retention_and_attempt_bounds(self):
        plan, manifest = fixture()
        invalid(
            self,
            validate_manifest,
            replace(manifest, artifact_digests=(("x", "bad"),)),
            replace(manifest, artifact_digests=(("x", DIGEST), ("x", DIGEST))),
            replace(manifest, artifact_digests=(("/absolute", DIGEST),)),
            replace(manifest, artifact_digests=(("..\\secret", DIGEST),)),
            replace(manifest, artifact_digests=(("a\x00b", DIGEST),)),
            replace(manifest, duration_seconds=-1),
            replace(manifest, duration_seconds=float("inf")),
            replace(manifest, duration_seconds=float("nan")),
            replace(manifest, duration_seconds=MAX_DURATION_SECONDS + 1),
            replace(manifest, critical_path_seconds=9),
            replace(manifest, created_at=0),
            replace(manifest, expires_at=manifest.expires_at + 1),
            replace(manifest, attempt=0),
            replace(manifest, attempt=True),
            replace(manifest, attempt=MAX_ATTEMPT + 1),
        )
        artifacts = tuple(
            (f"artifact-{index}", DIGEST) for index in range(MAX_ARTIFACTS_PER_MANIFEST)
        )
        self.assertEqual(
            MAX_ARTIFACTS_PER_MANIFEST,
            len(replace(manifest, artifact_digests=artifacts).artifact_digests),
        )
        invalid(
            self,
            validate_manifest,
            replace(manifest, artifact_digests=(*artifacts, ("too-many", DIGEST))),
        )

        published_requirements = tuple(
            replace(item, retention_class="published-release")
            if item.family == manifest.family
            else item
            for item in plan.requirements
        )
        published_plan = replace(plan, requirements=published_requirements)
        published = manifest_for_requirement(published_plan, published_requirements[1])
        self.assertIsNone(published.expires_at)
        invalid(self, validate_manifest, replace(published, expires_at=1))

    def test_strict_json_and_caps_before_conversion(self):
        _, manifest = fixture()
        text = serialize_manifest(manifest)
        payload = manifest_to_dict(manifest)
        missing = dict(payload)
        missing.pop("reason")
        invalid(
            self,
            manifest_from_dict,
            [],
            missing,
            {**payload, "unexpected": 1},
            {**payload, "artifactDigests": [["x", DIGEST], ["x", DIGEST]]},
            {**payload, "fingerprint": {**payload["fingerprint"], "digest": "a" * 64}},
        )
        invalid(
            self,
            parse_manifest,
            text.replace('"schemaVersion": 1,', '"schemaVersion": NaN,', 1),
            text.replace(
                '"schemaVersion": 1,', '"schemaVersion": 1,\n  "schemaVersion": 1,', 1
            ),
            text.replace(
                '"schemaVersion": 1,',
                '"schemaVersion": 1,\n  "schemaVersion": 9' + "9" * 100 + ",",
                1,
            ),
            text.replace("Electivus/electivus-codex", r"\ud800", 1),
            text.replace("{\n", "{ \n", 1),
            b"\xff",
        )
        invalid(
            self,
            manifest_from_dict,
            {**payload, "reason": "bad\nreason"},
            {
                **payload,
                "artifactDigests": [
                    [f"artifact-{i}", DIGEST]
                    for i in range(MAX_ARTIFACTS_PER_MANIFEST + 1)
                ],
            },
        )
        oversized = replace(
            manifest,
            fingerprint=replace(
                manifest.fingerprint,
                platforms=tuple(f"{i}-" + "x" * 300 for i in range(900)),
            ),
        )
        with self.assertRaises(ContractError):
            serialize_manifest(oversized)


if __name__ == "__main__":
    unittest.main()
