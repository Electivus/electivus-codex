from dataclasses import replace
import json
import unittest

from test_validation_plan_contract import repository_only_plan
from validation_contracts import ContractError, candidate_to_dict
from validation_plan_contract import validate_plan
from validation_evidence_contract import (
    MAX_ARTIFACTS_PER_MANIFEST,
    MAX_ATTEMPT,
    MAX_DURATION_SECONDS,
    MAX_SERIALIZED_BYTES,
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
    requirement = next(item for item in plan.requirements if item.selected)
    return plan, manifest_for_requirement(
        plan,
        requirement,
        producer="repository-hygiene",
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
        payload = manifest_to_dict(manifest, plan)
        with self.assertRaises(ContractError):
            validate_manifest(manifest)
        self.assertEqual(manifest, manifest_from_dict(payload, plan))
        self.assertEqual(
            manifest, parse_manifest(serialize_manifest(manifest, plan), plan)
        )
        self.assertEqual(plan.candidate, manifest.candidate)
        self.assertEqual(plan.fingerprint, manifest.fingerprint)
        self.assertEqual(
            {"family": manifest.family, "name": "report.json", "digest": DIGEST},
            payload["artifactDigests"]
            and {
                "family": manifest.family,
                "name": payload["artifactDigests"][0][0],
                "digest": payload["artifactDigests"][0][1],
            },
        )

    def test_artifact_digests_are_immutable_at_dataclass_boundary(self):
        plan, manifest = fixture()

        with self.assertRaisesRegex(
            ContractError, "manifest.artifactDigests must be a tuple"
        ):
            replace(manifest, artifact_digests=list(manifest.artifact_digests))
        with self.assertRaisesRegex(
            ContractError, "manifest.artifactDigests pairs must be tuples"
        ):
            replace(
                manifest,
                artifact_digests=(list(manifest.artifact_digests[0]),),
            )

        before = serialize_manifest(manifest, plan)
        validate_manifest_against_plan(manifest, plan)
        with self.assertRaises(TypeError):
            manifest.artifact_digests[0][0] = "tampered"
        self.assertEqual(before, serialize_manifest(manifest, plan))
        self.assertEqual(manifest, parse_manifest(before, plan))

    def test_plan_snapshot_rejects_mutation_of_external_requirements(self):
        plan = repository_only_plan()
        requirements = list(plan.requirements)
        mutable_plan = replace(plan, requirements=requirements)
        selected = next(item for item in requirements if item.selected)
        manifest = manifest_for_requirement(mutable_plan, selected)
        before = serialize_manifest(manifest, mutable_plan)

        unselected_index = next(
            index for index, item in enumerate(requirements) if not item.selected
        )
        requirements[unselected_index] = replace(
            requirements[unselected_index], reason="tampered"
        )

        self.assertEqual(
            plan.requirements[unselected_index],
            manifest.plan.requirements[unselected_index],
        )
        with self.assertRaises(ContractError):
            validate_manifest_against_plan(manifest, mutable_plan)
        with self.assertRaises(ContractError):
            serialize_manifest(manifest, mutable_plan)
        self.assertEqual(before, serialize_manifest(manifest, plan))

    def test_manifest_snapshots_mutable_fingerprint_collections(self):
        plan = repository_only_plan()
        collection_names = (
            "dependencies",
            "toolchains",
            "platforms",
            "parameters",
            "inputs",
        )

        for name in collection_names:
            with self.subTest(name=name):
                original = getattr(plan.fingerprint, name)
                mutable = (
                    list(original)
                    if name == "platforms"
                    else [list(pair) for pair in original]
                )
                fingerprint = replace(plan.fingerprint, **{name: mutable})
                mutable_plan = replace(plan, fingerprint=fingerprint)
                validate_plan(mutable_plan)
                requirement = next(
                    item for item in mutable_plan.requirements if item.selected
                )

                manifest = manifest_for_requirement(mutable_plan, requirement)
                self.assertEqual(plan.candidate, manifest.candidate)
                self.assertEqual(plan.fingerprint, manifest.fingerprint)
                self.assertIsNot(mutable_plan.candidate, manifest.candidate)
                self.assertIsNot(mutable_plan.fingerprint, manifest.fingerprint)

                before = serialize_manifest(manifest, plan)
                if name == "platforms":
                    mutable.clear()
                else:
                    mutable[0][1] = "tampered"
                    mutable.clear()

                self.assertEqual(before, serialize_manifest(manifest, plan))
                self.assertEqual(plan.fingerprint, manifest.fingerprint)

    def test_plan_and_deep_identity_bindings_fail_closed(self):
        plan, manifest = fixture()
        payload = manifest_to_dict(manifest, plan)

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

        requirement = manifest.requirement
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
        changed_candidate = dict(payload["candidate"])
        changed_candidate["candidateSha"] = "d" * 40
        recomputed = dict(payload)
        recomputed["candidate"] = changed_candidate
        recomputed["evidenceId"] = (
            f"{changed_candidate['candidateSha']}:{manifest.family}:{manifest.stage}:"
            f"{manifest.fingerprint.digest}"
        )
        self.assertEqual(payload["fingerprint"], recomputed["fingerprint"])
        canonical = (
            json.dumps(
                recomputed,
                ensure_ascii=False,
                allow_nan=False,
                sort_keys=True,
                indent=2,
            )
            + "\n"
        )
        with self.assertRaises(ContractError):
            parse_manifest(canonical, plan)

    def test_requirement_binding_cannot_be_overridden(self):
        plan, manifest = fixture()
        requirement = manifest.requirement
        mutations = (
            replace(
                requirement,
                family="repository",
                stage="preflight",
                selected=True,
                disposition="required",
            ),
            replace(requirement, stage="merge"),
            replace(requirement, selected=False, disposition="not-required"),
            replace(requirement, retention_class="integrated-certification"),
        )

        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaises(ContractError):
                manifest_for_requirement(plan, mutation)

        overridden = EvidenceManifest(
            plan=plan,
            requirement=requirement,
            schema_version=manifest.schema_version,
            evidence_id=manifest.evidence_id,
            family=manifest.family,
            stage=manifest.stage,
            candidate=manifest.candidate,
            producer=manifest.producer,
            outcome=manifest.outcome,
            disposition=manifest.disposition,
            fingerprint=manifest.fingerprint,
            artifact_digests=manifest.artifact_digests,
            retention_class="integrated-certification",
            duration_seconds=manifest.duration_seconds,
            critical_path_seconds=manifest.critical_path_seconds,
            reason=manifest.reason,
            attempt=manifest.attempt,
            cache_mode=manifest.cache_mode,
            created_at=manifest.created_at,
            expires_at=manifest.expires_at,
        )
        with self.assertRaises(ContractError):
            validate_manifest_against_plan(overridden, plan)

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
                manifest_for_requirement(plan, manifest.requirement, outcome=outcome),
                plan,
            )
        invalid(
            self,
            lambda value: validate_manifest(value, plan),
            replace(manifest, outcome="not-required"),
            replace(manifest, outcome="unknown"),
            replace(manifest, family="unknown"),
            replace(manifest, disposition="optional"),
            replace(manifest, retention_class="unknown"),
            replace(manifest, cache_mode="unknown"),
            replace(manifest, cache_mode="cache-only"),
        )

        sentinel = manifest_for_requirement(
            plan, next(item for item in plan.requirements if not item.selected)
        )

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
                plan,
                manifest.requirement,
                cache_mode=cache_mode,
                outcome="indeterminate" if cache_mode == "cache-only" else "passed",
            )
            validate_manifest_against_plan(cached, plan)
            self.assertEqual(
                cached, parse_manifest(serialize_manifest(cached, plan), plan)
            )

    def test_artifacts_durations_retention_and_attempt_bounds(self):
        plan, manifest = fixture()
        invalid(
            self,
            lambda value: validate_manifest(value, plan),
            replace(manifest, artifact_digests=(("x", "bad"),)),
            replace(manifest, artifact_digests=(("x", DIGEST), ("x", DIGEST))),
            replace(manifest, artifact_digests=(("C:secret", DIGEST),)),
            replace(manifest, artifact_digests=(("C:/secret", DIGEST),)),
            replace(manifest, artifact_digests=((r"C:\\secret", DIGEST),)),
            replace(manifest, artifact_digests=((r"\\server\\share", DIGEST),)),
            replace(manifest, artifact_digests=(("//server/share", DIGEST),)),
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
        bounded = replace(manifest, artifact_digests=artifacts)
        validate_manifest(bounded, plan)
        self.assertEqual(MAX_ARTIFACTS_PER_MANIFEST, len(bounded.artifact_digests))
        invalid(
            self,
            lambda value: validate_manifest(value, plan),
            replace(manifest, artifact_digests=(*artifacts, ("too-many", DIGEST))),
        )

        release_plan = repository_only_plan(
            plan_profile="certification-required",
            selected_families=("release-packaging",),
        )
        release_candidate = replace(
            release_plan.candidate,
            event_name="push",
            base_sha=None,
            head_sha=None,
            kind="release",
            pull_request_number=None,
        )
        candidate_source = tuple(
            (name, "" if value is None else str(value))
            for name, value in candidate_to_dict(release_candidate).items()
        )
        published_requirements = tuple(
            replace(item, retention_class="published-release")
            if item.family == "release-packaging"
            else item
            for item in release_plan.requirements
        )
        release_plan = replace(
            release_plan,
            candidate=release_candidate,
            requirements=published_requirements,
            fingerprint=replace(
                release_plan.fingerprint,
                profile="certification-required",
                source=candidate_source,
            ),
        )
        published_requirement = next(
            item
            for item in published_requirements
            if item.family == "release-packaging"
        )
        published = manifest_for_requirement(release_plan, published_requirement)
        self.assertIsNone(published.expires_at)
        invalid(
            self,
            lambda value: validate_manifest(value, release_plan),
            replace(published, expires_at=1),
        )

    def test_strict_json_and_caps_before_conversion(self):
        plan, manifest = fixture()
        text = serialize_manifest(manifest, plan)
        payload = manifest_to_dict(manifest, plan)

        def from_dict(value):
            return manifest_from_dict(value, plan)

        def parse(value):
            return parse_manifest(value, plan)

        missing = dict(payload)
        missing.pop("reason")
        invalid(
            self,
            from_dict,
            [],
            missing,
            {**payload, "unexpected": 1},
            {**payload, "artifactDigests": [["x", DIGEST], ["x", DIGEST]]},
            {**payload, "fingerprint": {**payload["fingerprint"], "digest": "a" * 64}},
        )
        invalid(
            self,
            parse,
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
        oversized_invalid_utf8 = b"\xff" * (MAX_SERIALIZED_BYTES + 1)
        with self.assertRaisesRegex(ContractError, "input byte budget"):
            parse(oversized_invalid_utf8)
        giant_integer = text.replace(
            '"pullRequestNumber": 181',
            '"pullRequestNumber": ' + "9" * 5_000,
            1,
        )
        with self.assertRaisesRegex(
            ContractError, "JSON integer exceeds its bounded range"
        ):
            parse(giant_integer)
        invalid(
            self,
            from_dict,
            {**payload, "reason": "bad\nreason"},
            {
                **payload,
                "artifactDigests": [
                    [f"artifact-{i}", DIGEST]
                    for i in range(MAX_ARTIFACTS_PER_MANIFEST + 1)
                ],
            },
        )
        oversized_payload = dict(payload)
        oversized_payload["candidate"] = "x" * MAX_SERIALIZED_BYTES
        with self.assertRaisesRegex(ContractError, "serialized byte budget"):
            from_dict(oversized_payload)
        oversized = replace(
            manifest,
            fingerprint=replace(
                manifest.fingerprint,
                platforms=tuple(f"{i}-" + "x" * 300 for i in range(900)),
            ),
        )
        with self.assertRaises(ContractError):
            serialize_manifest(oversized, plan)

        oversized_artifacts = replace(
            manifest,
            artifact_digests=tuple(
                (f"{index:02d}" + "x" * 4_088, DIGEST)
                for index in range(MAX_ARTIFACTS_PER_MANIFEST)
            ),
        )
        with self.assertRaisesRegex(ContractError, "serialized byte budget"):
            manifest_to_dict(oversized_artifacts, plan)

        with self.assertRaisesRegex(ContractError, "keys must be strings"):
            manifest_from_dict({1: "invalid"}, plan)

    def test_serialized_manifest_limit_is_exact_for_input_and_output(self):
        plan, manifest = fixture()
        artifacts = tuple(
            (f"{index:02d}-" + "x" * 3_797, DIGEST)
            for index in range(MAX_ARTIFACTS_PER_MANIFEST)
        )
        base = replace(
            manifest,
            artifact_digests=artifacts,
            producer="p" * 4_096,
            reason="r",
        )
        base_serialized = serialize_manifest(base, plan)
        reason_length = MAX_SERIALIZED_BYTES - len(base_serialized.encode()) + 1
        self.assertGreaterEqual(reason_length, 1)
        self.assertLessEqual(reason_length, 4_096)
        at_limit = replace(base, reason="r" * reason_length)
        serialized = serialize_manifest(at_limit, plan)
        self.assertEqual(MAX_SERIALIZED_BYTES, len(serialized.encode("utf-8")))
        self.assertEqual(at_limit, parse_manifest(serialized, plan))
        self.assertEqual(at_limit, parse_manifest(serialized.encode("utf-8"), plan))

        over_limit = replace(at_limit, reason="r" * (reason_length + 1))
        oversized_payload = dict(manifest_to_dict(at_limit, plan))
        oversized_payload["reason"] = over_limit.reason
        oversized = (
            json.dumps(
                oversized_payload,
                ensure_ascii=False,
                allow_nan=False,
                sort_keys=True,
                indent=2,
            )
            + "\n"
        )
        self.assertEqual(MAX_SERIALIZED_BYTES + 1, len(oversized.encode("utf-8")))
        with self.assertRaises(ContractError):
            serialize_manifest(over_limit, plan)
        with self.assertRaises(ContractError):
            manifest_from_dict(manifest_to_dict(over_limit, plan), plan)
        with self.assertRaises(ContractError):
            parse_manifest(oversized, plan)

    def test_parser_converts_deep_json_recursion_to_contract_error(self):
        plan, _ = fixture()
        deeply_nested = "[" * 2_000 + "]" * 2_000

        with self.assertRaises(ContractError):
            parse_manifest(deeply_nested, plan)

    def test_controls_reject_del_and_c1_but_preserve_tabs(self):
        plan, manifest = fixture()
        for code_point in range(0x7F, 0xA0):
            with self.subTest(code_point=code_point), self.assertRaises(ContractError):
                validate_manifest(
                    replace(manifest, reason=f"bad{chr(code_point)}text"), plan
                )

        tabbed = replace(manifest, reason="line\twith tab")
        validate_manifest_against_plan(tabbed, plan)
        self.assertEqual(tabbed, parse_manifest(serialize_manifest(tabbed, plan), plan))


if __name__ == "__main__":
    unittest.main()
