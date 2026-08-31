from dataclasses import replace
import json
import unittest

from test_validation_plan_contract import (
    assert_invalid,
    candidate,
    certified_plan,
    repository_only_plan,
    with_fingerprint,
)
from validation_contracts import (
    ContractError,
    MAX_JSON_INTEGER,
    candidate_from_dict,
    candidate_to_dict,
    canonical_json,
)
from validation_plan_codec import (
    MAX_PLAN_INPUT_BYTES,
    MAX_PLAN_ITEMS,
    _serialize_payload,
    parse_plan,
    serialize_plan,
    validate_plan_budgets,
)
from validation_plan_contract import (
    validate_plan,
    parse_plan as legacy_parse_plan,
    plan_to_dict,
    serialize_plan as legacy_serialize_plan,
)


def candidate_kind_plan(
    kind: str,
    *,
    profile: str = "ordinary",
    selected_families: tuple[str, ...] = ("repository-policy",),
):
    plan = repository_only_plan(
        plan_profile=profile, selected_families=selected_families
    )
    candidate_value = replace(
        plan.candidate,
        event_name="push",
        base_sha=None,
        head_sha=None,
        kind=kind,
        pull_request_number=None,
    )
    source = tuple(
        (name, "" if value is None else str(value))
        for name, value in candidate_to_dict(candidate_value).items()
    )
    return replace(
        plan,
        candidate=candidate_value,
        fingerprint=replace(plan.fingerprint, profile=profile, source=source),
    )


class ValidationPlanCodecTests(unittest.TestCase):
    def test_candidate_kinds_require_certification_profile(self) -> None:
        for kind in ("integrated", "release", "synchronization"):
            with self.subTest(kind=kind):
                assert_invalid(self, validate_plan_budgets, candidate_kind_plan(kind))
                validate_plan_budgets(
                    candidate_kind_plan(kind, profile="certification-required")
                )

    def test_candidate_identity_mutations_keep_original_fingerprint_and_fail(
        self,
    ) -> None:
        plan = repository_only_plan()
        original_fingerprint = plan.fingerprint
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
            mutated = replace(plan, candidate=replace(plan.candidate, **{field: value}))
            self.assertIs(mutated.fingerprint, original_fingerprint)
            assert_invalid(self, validate_plan_budgets, mutated)

    def test_multiple_surfaces_and_modifiers_round_trip_with_fingerprint(self) -> None:
        plan = certified_plan(risk_modifiers=("security", "publication"))
        surfaces = ("repository", "documentation", "rust")
        fingerprint = replace(
            plan.fingerprint,
            parameters=(
                ("changeSurfaces", canonical_json(list(surfaces))),
                *plan.fingerprint.parameters[1:],
            ),
        )
        multi_surface_plan = replace(plan, surfaces=surfaces, fingerprint=fingerprint)

        validate_plan_budgets(multi_surface_plan)
        self.assertEqual(
            multi_surface_plan,
            parse_plan(serialize_plan(multi_surface_plan)),
        )

        for index, item in enumerate(plan.requirements):
            requirements = list(plan.requirements)
            requirements[index] = replace(
                item, stage="merge" if item.stage == "preflight" else "preflight"
            )
            assert_invalid(
                self, validate_plan_budgets, replace(plan, requirements=requirements)
            )

    def test_published_retention_requires_a_release_candidate(self) -> None:
        def published(plan):
            return replace(
                plan,
                requirements=tuple(
                    replace(item, retention_class="published-release")
                    if item.family == "release-packaging"
                    else item
                    for item in plan.requirements
                ),
            )

        validate_plan_budgets(
            published(
                candidate_kind_plan(
                    "release",
                    profile="certification-required",
                    selected_families=("release-packaging",),
                )
            )
        )
        assert_invalid(
            self,
            validate_plan_budgets,
            published(
                repository_only_plan(
                    plan_profile="certification-required",
                    selected_families=("release-packaging",),
                )
            ),
        )

    def test_canonical_json_round_trips_as_exact_object(self) -> None:
        plan = repository_only_plan()
        self.assertEqual(plan, parse_plan(serialize_plan(plan)))
        self.assertEqual(plan, legacy_parse_plan(legacy_serialize_plan(plan)))

    def test_pull_request_number_uses_bounded_json_integer_range(self) -> None:
        candidate_value = replace(candidate(), pull_request_number=MAX_JSON_INTEGER)
        self.assertEqual(
            candidate_value, candidate_from_dict(candidate_to_dict(candidate_value))
        )
        assert_invalid(
            self,
            candidate_from_dict,
            {
                **candidate_to_dict(candidate()),
                "pullRequestNumber": MAX_JSON_INTEGER + 1,
            },
        )
        serialized = serialize_plan(repository_only_plan())
        oversized = serialized.replace(
            '"pullRequestNumber": 181',
            '"pullRequestNumber": ' + "9" * 5_000,
            1,
        )
        with self.assertRaisesRegex(
            ContractError, "JSON integer exceeds its bounded range"
        ):
            parse_plan(oversized)

    def test_json_errors_and_recursion_become_contract_errors(self) -> None:
        serialized = serialize_plan(repository_only_plan())
        invalid = (
            serialized.replace(
                '"schemaVersion": 1,',
                '"schemaVersion": 1,\n"schemaVersion": 1,',
            ),
            serialized.replace('"schemaVersion": 1', '"schemaVersion": NaN', 1),
            serialized.replace("Electivus/electivus-codex", r"\ud800", 1),
            serialized + " ",
            "9" * 5_000,
        )
        for value in invalid:
            assert_invalid(self, parse_plan, value)

        deeply_nested = "[" * 2_000 + "]" * 2_000
        assert_invalid(self, parse_plan, deeply_nested)
        nested: object = []
        for _ in range(2_000):
            nested = [nested]
        assert_invalid(
            self,
            lambda value: _serialize_payload(value, "Validation plan"),
            {"nested": nested},
        )

    def test_input_byte_budget_is_checked_before_decoding(self) -> None:
        oversized_invalid_utf8 = b"\xff" * (MAX_PLAN_INPUT_BYTES + 1)
        with self.assertRaisesRegex(ContractError, "input byte budget"):
            parse_plan(oversized_invalid_utf8)

    def test_controls_reject_del_and_c1_but_preserve_unicode_and_tabs(self) -> None:
        plan = repository_only_plan()
        for code_point in range(0x7F, 0xA0):
            with self.subTest(code_point=code_point), self.assertRaises(ContractError):
                bad = replace(plan.requirements[1], reason=f"bad{chr(code_point)}text")
                validate_plan_budgets(
                    replace(
                        plan,
                        requirements=(
                            plan.requirements[0],
                            bad,
                            *plan.requirements[2:],
                        ),
                    )
                )

        valid = replace(plan.requirements[1], reason="política ✓ — 日本語\ttexto")
        valid_plan = replace(
            plan,
            requirements=(plan.requirements[0], valid, *plan.requirements[2:]),
        )
        self.assertEqual(valid_plan, parse_plan(serialize_plan(valid_plan)))

    def test_aggregate_and_input_budgets_are_bounded(self) -> None:
        plan = repository_only_plan()
        with self.assertRaisesRegex(ContractError, "input byte budget"):
            parse_plan(serialize_plan(plan) + " " * MAX_PLAN_INPUT_BYTES)

        too_many_items = with_fingerprint(
            plan,
            inputs=tuple((f"input-{index}", "") for index in range(MAX_PLAN_ITEMS)),
        )
        with self.assertRaisesRegex(ContractError, "aggregate item budget"):
            validate_plan(too_many_items)

        long_inputs = tuple((f"input-{index}", "x" * 1_000) for index in range(100))
        large = with_fingerprint(plan, inputs=long_inputs)
        with self.assertRaisesRegex(ContractError, "aggregate text budget"):
            serialize_plan(large)

    def test_json_array_parameters_remain_canonical(self) -> None:
        plan = repository_only_plan()
        malformed = with_fingerprint(
            plan,
            parameters=(
                ("changeSurfaces", "repository,documentation"),
                *plan.fingerprint.parameters[1:],
            ),
        )
        assert_invalid(self, validate_plan_budgets, malformed)

        deep_parameter = "[" * 2_000 + "]" * 2_000
        assert_invalid(
            self,
            validate_plan_budgets,
            with_fingerprint(
                plan,
                parameters=(
                    ("changeSurfaces", deep_parameter),
                    *plan.fingerprint.parameters[1:],
                ),
            ),
        )

        payload = plan_to_dict(plan)
        payload["fingerprint"]["parameters"][0][1] = "[repository]"
        assert_invalid(self, parse_plan, json.dumps(payload))


if __name__ == "__main__":
    unittest.main()
