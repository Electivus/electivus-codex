from dataclasses import FrozenInstanceError, replace
import unittest

from test_validation_plan_contract import evidence_ledger, repository_only_plan
from validation_contracts import ContractError, candidate_to_dict
from validation_evidence_contract import manifest_for_requirement
from validation_report_contract import MAX_ARTIFACTS_PER_REPORT, MAX_ERRORS
from validation_report_contract import MAX_SERIALIZED_BYTES
from validation_report_contract import parse_report, report_for_evidence
from validation_report_contract import report_from_dict, report_to_dict
from validation_report_contract import serialize_report, validate_report


DIGEST = "d" * 64
CREATED_AT = 1_700_000_000


def fixture(
    *,
    selected_families: tuple[str, ...] = ("repository-policy",),
    outcomes: dict[str, str] | None = None,
    errors: tuple[str, ...] = (),
    artifacts: tuple[tuple[str, str], ...] = (("report.json", DIGEST),),
):
    plan = repository_only_plan(selected_families=selected_families)
    outcomes = outcomes or {}
    manifests = tuple(
        manifest_for_requirement(
            plan,
            requirement,
            outcome=outcomes.get(requirement.family, "passed"),
            producer="repository-policy",
            artifact_digests=artifacts if requirement.selected else (),
            duration_seconds=8,
            critical_path_seconds=5,
            created_at=CREATED_AT,
        )
        for requirement in plan.requirements
    )
    return plan, manifests, report_for_evidence(plan, manifests, errors=errors)


def invalid(testcase, function, *values):
    for value in values:
        with testcase.subTest(value=value), testcase.assertRaises(ContractError):
            function(value)


class ValidationReportContractTests(unittest.TestCase):
    def test_round_trip_is_a_whole_object_and_wire_shape_is_exact(self):
        plan, manifests, report = fixture()
        payload = report_to_dict(report)

        self.assertEqual(report, report_from_dict(payload))
        self.assertEqual(report, parse_report(serialize_report(report)))
        self.assertEqual(
            {
                "schemaVersion",
                "candidate",
                "plan",
                "evidence",
                "outcome",
                "outcomes",
                "durations",
                "fingerprints",
                "artifacts",
                "errors",
            },
            set(payload),
        )
        self.assertNotIn("identity", payload)
        self.assertEqual(candidate_to_dict(plan.candidate), payload["candidate"])
        self.assertEqual(
            [{"family": "repository-policy", "name": "report.json", "digest": DIGEST}],
            payload["artifacts"],
        )
        self.assertEqual(
            tuple(item.family for item in manifests),
            tuple(family for family, _ in report.outcomes),
        )

    def test_report_is_frozen_and_uses_derived_projections(self):
        _, manifests, report = fixture()
        with self.assertRaises(FrozenInstanceError):
            report.outcome = "passed"

        invalid(
            self,
            validate_report,
            replace(
                report,
                outcomes=(
                    ("repository-policy", "product-failure"),
                    *report.outcomes[1:],
                ),
            ),
            replace(
                report,
                fingerprints=(
                    ("repository-policy", "a" * 64),
                    *report.fingerprints[1:],
                ),
            ),
            replace(
                report,
                durations=(("repository-policy", (9, 5)), *report.durations[1:]),
            ),
            replace(
                report,
                artifacts=(("repository-policy", "other.json", DIGEST),),
            ),
        )
        self.assertEqual(manifests[1].outcome, "passed")

    def test_outcome_priority_and_error_fail_closed(self):
        selected = ("repository-policy", "repository-hygiene", "rust-fast")
        for outcome, expected in (
            ("passed", "passed"),
            ("stale", "stale"),
            ("product-failure", "product-failure"),
            ("indeterminate", "indeterminate"),
            ("infrastructure-failure", "infrastructure-failure"),
        ):
            _, _, report = fixture(
                selected_families=selected,
                outcomes={selected[-1]: outcome},
            )
            self.assertEqual(expected, report.outcome)

        _, _, report = fixture(
            selected_families=selected,
            outcomes={selected[0]: "infrastructure-failure", selected[1]: "passed"},
            errors=("aggregation inconsistency",),
        )
        self.assertEqual("indeterminate", report.outcome)
        invalid(
            self, validate_report, replace(report, outcome="infrastructure-failure")
        )

    def test_all_not_required_uses_only_not_required_outcome(self):
        plan = repository_only_plan(
            requirements=evidence_ledger(()), selected_families=()
        )
        manifests = tuple(
            manifest_for_requirement(plan, requirement)
            for requirement in plan.requirements
        )
        report = report_for_evidence(plan, manifests)
        self.assertEqual("not-required", report.outcome)
        validate_report(report)
        invalid(
            self,
            validate_report,
            replace(report, outcome="passed"),
            replace(report, errors=("unexpected error",)),
        )

    def test_candidate_plan_manifest_and_fingerprint_bindings_are_deep(self):
        plan, manifests, report = fixture()
        candidate = report.candidate
        candidate_changes = (
            replace(candidate, event_name="workflow_dispatch"),
            replace(candidate, repository="Other/repo"),
            replace(candidate, default_branch="trunk"),
            replace(candidate, candidate_sha="e" * 40),
            replace(candidate, base_sha="e" * 40),
            replace(candidate, head_sha="f" * 40),
            replace(candidate, kind="integrated", pull_request_number=None),
            replace(candidate, pull_request_number=182),
            replace(candidate, branch="other-branch"),
        )
        invalid(
            self,
            validate_report,
            *(replace(report, candidate=value) for value in candidate_changes),
        )

        invalid(
            self,
            validate_report,
            replace(report, plan=replace(plan, candidate=candidate_changes[0])),
            replace(
                report,
                evidence=(
                    manifests[0],
                    replace(manifests[1], candidate=candidate_changes[1]),
                    *manifests[2:],
                ),
            ),
            replace(
                report,
                evidence=(
                    manifests[0],
                    replace(
                        manifests[1],
                        fingerprint=replace(
                            manifests[1].fingerprint,
                            toolchains=(("python", "3.12"),),
                        ),
                    ),
                    *manifests[2:],
                ),
            ),
        )

    def test_canonical_evidence_order_and_family_uniqueness(self):
        plan, manifests, report = fixture()
        reordered = (*manifests[1:], manifests[0])
        invalid(self, validate_report, replace(report, evidence=reordered))
        invalid(
            self,
            validate_report,
            replace(report, evidence=(*manifests[:-1], manifests[-1], manifests[-1])),
            replace(
                report,
                evidence=(*manifests[:-1], replace(manifests[-1], family="unknown")),
            ),
            replace(report, evidence=manifests[:-1]),
        )

        payload = report_to_dict(report)
        payload["evidence"][1]["disposition"] = "not-required"
        invalid(self, report_from_dict, payload)

    def test_projection_shapes_and_values_cannot_be_supplied_in_parallel(self):
        report = fixture()[2]
        payload = report_to_dict(report)
        invalid(
            self,
            report_from_dict,
            {**payload, "outcomes": {**payload["outcomes"], "extra": "passed"}},
            {**payload, "fingerprints": [report.plan.fingerprint.digest]},
            {**payload, "durations": {"repository-policy": 8}},
            {
                **payload,
                "artifacts": [
                    {
                        "name": "report.json",
                        "digest": DIGEST,
                        "family": "repository-policy",
                    },
                    {
                        "family": "repository-policy",
                        "name": "report.json",
                        "digest": DIGEST,
                    },
                ],
            },
            {**payload, "errors": ["same", "same"]},
        )

        payload["outcomes"]["repository-policy"] = "product-failure"
        invalid(self, report_from_dict, payload)
        payload = report_to_dict(report)
        payload["durations"]["repository-policy"]["criticalPathSeconds"] = 9
        invalid(self, report_from_dict, payload)
        payload = report_to_dict(report)
        payload["fingerprints"]["repository-policy"] = "a" * 64
        invalid(self, report_from_dict, payload)

    def test_outcomes_preserve_each_distinct_failure_class(self):
        selected = ("repository-policy", "repository-hygiene", "rust-fast")
        for family, outcome in (
            (selected[0], "stale"),
            (selected[1], "product-failure"),
            (selected[2], "infrastructure-failure"),
        ):
            report = fixture(
                selected_families=selected,
                outcomes={family: outcome},
            )[2]
            self.assertEqual(outcome, dict(report.outcomes)[family])

    def test_duration_artifact_and_text_bounds(self):
        report = fixture()[2]
        invalid(
            self,
            validate_report,
            replace(
                report,
                durations=(("repository-policy", (-1, 0)), *report.durations[1:]),
            ),
            replace(
                report,
                durations=(
                    ("repository-policy", (float("inf"), 0)),
                    *report.durations[1:],
                ),
            ),
            replace(
                report, durations=(("repository-policy", (5, 6)), *report.durations[1:])
            ),
            replace(report, artifacts=(("repository-policy", "/absolute", DIGEST),)),
            replace(report, artifacts=(("repository-policy", "..\\secret", DIGEST),)),
            replace(report, artifacts=(("repository-policy", "C:secret", DIGEST),)),
            replace(report, artifacts=(("repository-policy", "report.json", "bad"),)),
            replace(report, artifacts=(("unknown", "report.json", DIGEST),)),
        )

        long_name = "x" * 4_096
        long_manifest = fixture(artifacts=((long_name, DIGEST),))[2]
        validate_report(long_manifest)
        with self.assertRaises(ContractError):
            fixture(artifacts=(("x" * 4_097, DIGEST),))

    def test_report_artifact_and_projection_caps(self):
        selected = (
            "repository-policy",
            "repository-hygiene",
            "rust-fast",
            "linux-x64-bazel",
        )
        artifacts = tuple((f"artifact-{index}", DIGEST) for index in range(64))
        report = fixture(selected_families=selected, artifacts=artifacts)[2]
        self.assertEqual(MAX_ARTIFACTS_PER_REPORT, len(report.artifacts))
        validate_report(report)
        invalid(
            self,
            validate_report,
            replace(report, artifacts=(*report.artifacts, report.artifacts[0])),
            replace(report, evidence=(*report.evidence, *report.evidence[:50])),
            replace(report, outcomes=(*report.outcomes, report.outcomes[0])),
        )

        errors_report = fixture(
            errors=tuple(f"error-{index}" for index in range(MAX_ERRORS))
        )[2]
        validate_report(errors_report)
        with self.assertRaises(ContractError):
            report_for_evidence(
                errors_report.plan,
                errors_report.evidence,
                errors=tuple(f"error-{index}" for index in range(MAX_ERRORS + 1)),
            )

    def test_frozen_wire_parser_is_strict_and_bounded(self):
        report = fixture()[2]
        text = serialize_report(report)
        payload = report_to_dict(report)
        invalid(
            self,
            report_from_dict,
            [],
            {**payload, "unexpected": True},
            {key: value for key, value in payload.items() if key != "errors"},
            {**payload, "schemaVersion": 2},
            {**payload, "outcome": "unknown"},
            {**payload, "errors": ["error", "error"]},
        )
        invalid(
            self,
            parse_report,
            text.replace('"schemaVersion": 1,', '"schemaVersion": NaN,', 1),
            text.replace(
                '"schemaVersion": 1,', '"schemaVersion": 1,\n  "schemaVersion": 1,', 1
            ),
            text.replace("Electivus/electivus-codex", r"\ud800", 1),
            text.replace("{\n", "{ \n", 1),
            b"\xff",
        )
        with self.assertRaises(ContractError):
            parse_report("[" * 2_000 + "]" * 2_000)

        invalid(
            self,
            report_from_dict,
            {**payload, "errors": ["bad\nerror"]},
            {
                **payload,
                "errors": ["x" * 4_097],
            },
        )

    def test_json_and_serialized_limits_count_utf8_bytes(self):
        report = fixture()[2]
        payload = report_to_dict(report)
        payload["errors"] = ["é" * 2_048]
        payload["outcome"] = "indeterminate"
        payload["errors"] = ["é" * 2_048]
        bounded = report_from_dict(payload)
        self.assertEqual(payload, report_to_dict(bounded))
        oversized = serialize_report(bounded) + "x" * (
            MAX_SERIALIZED_BYTES - len(serialize_report(bounded).encode("utf-8")) + 1
        )
        self.assertGreater(len(oversized.encode("utf-8")), MAX_SERIALIZED_BYTES)
        invalid(self, parse_report, oversized)


if __name__ == "__main__":
    unittest.main()
