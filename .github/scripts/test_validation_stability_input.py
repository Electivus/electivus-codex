import unittest
from dataclasses import replace

from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_integrated import integrated_manifest
from validation_reports import report_to_dict
from validation_plan import build_plan
from validation_result import aggregate
from validation_result import manifest_for_requirement
from validation_stability import validate_stability
from validation_stability_input import build_stability_inputs


def _candidate(index: int, *, kind: str = "pull-request") -> CandidateIdentity:
    return CandidateIdentity(
        event_name="push" if kind == "integrated" else "pull_request",
        repository="Electivus/electivus-codex",
        default_branch="main",
        candidate_sha=f"{index:040x}",
        base_sha=None if kind == "integrated" else "b" * 40,
        head_sha=None if kind == "integrated" else f"{index + 1000:040x}",
        kind=kind,
        branch="main" if kind == "integrated" else "feature/stability",
    )


def _report(candidate: CandidateIdentity, changed_file: str):
    plan = build_plan(candidate, [changed_file])
    manifests = tuple(
        manifest_for_requirement(plan, requirement) for requirement in plan.requirements
    )
    return aggregate(plan, manifests, current_candidate=candidate).report


class ValidationStabilityInputTests(unittest.TestCase):
    def test_inputs_are_derived_from_exact_reports_and_pass_the_stability_contract(
        self,
    ):
        ordinary = tuple(
            _report(_candidate(index), "codex-rs/core/src/lib.rs")
            for index in range(1, 21)
        )
        certification = _report(
            _candidate(101), ".github/workflows/validation-shadow.yml"
        )
        cache_disabled = replace(ordinary[0], cache_fallback="disabled-reconstruction")
        integrated = _report(
            _candidate(201, kind="integrated"), "codex-rs/core/src/lib.rs"
        )
        authority = integrated_manifest(report_to_dict(integrated))

        inputs = build_stability_inputs(
            ordinary,
            certification,
            cache_disabled,
            integrated,
            authority,
        )

        self.assertEqual(4, len(inputs.records))
        self.assertEqual(21, len(inputs.samples))
        self.assertEqual(f"{201:040x}", inputs.resulting_main_sha)
        self.assertTrue(
            validate_stability(
                inputs.records,
                resulting_main_sha=inputs.resulting_main_sha,
                ordinary_samples=inputs.samples,
            ).passed
        )

    def test_cache_disabled_and_integrated_identities_are_required(self):
        ordinary = tuple(
            _report(_candidate(index), "codex-rs/core/src/lib.rs")
            for index in range(1, 21)
        )
        certification = _report(
            _candidate(101), ".github/workflows/validation-shadow.yml"
        )
        integrated = _report(
            _candidate(201, kind="integrated"), "codex-rs/core/src/lib.rs"
        )
        authority = integrated_manifest(report_to_dict(integrated))

        with self.assertRaises(ContractError):
            build_stability_inputs(
                ordinary,
                certification,
                ordinary[0],
                integrated,
                authority,
            )
        with self.assertRaises(ContractError):
            build_stability_inputs(
                ordinary,
                certification,
                replace(ordinary[0], cache_fallback="disabled-reconstruction"),
                replace(
                    integrated,
                    candidate=replace(integrated.candidate, candidate_sha="f" * 40),
                ),
                authority,
            )

        with self.assertRaises(ContractError):
            build_stability_inputs(
                ordinary,
                certification,
                replace(
                    ordinary[1], cache_fallback="disabled-reconstruction"
                ),
                integrated,
                authority,
            )

    def test_stability_rejects_retry_and_mixed_validation_generations(self):
        ordinary = tuple(
            _report(_candidate(index), "codex-rs/core/src/lib.rs")
            for index in range(1, 21)
        )
        certification = _report(
            _candidate(101), ".github/workflows/validation-shadow.yml"
        )
        cache_disabled = replace(ordinary[0], cache_fallback="disabled-reconstruction")
        integrated = _report(
            _candidate(201, kind="integrated"), "codex-rs/core/src/lib.rs"
        )
        authority = integrated_manifest(report_to_dict(integrated))

        retried = replace(
            ordinary[0],
            evidence=tuple(replace(manifest, attempt=2) for manifest in ordinary[0].evidence),
        )
        with self.assertRaises(ContractError):
            build_stability_inputs(
                (retried, *ordinary[1:]),
                certification,
                replace(retried, cache_fallback="disabled-reconstruction"),
                integrated,
                authority,
            )

        changed_toolchain = replace(
            ordinary[1],
            plan=replace(
                ordinary[1].plan,
                fingerprint=replace(
                    ordinary[1].plan.fingerprint,
                    toolchains=(("rust", "different"),),
                ),
            ),
        )
        with self.assertRaises(ContractError):
            build_stability_inputs(
                (ordinary[0], changed_toolchain, *ordinary[2:]),
                certification,
                cache_disabled,
                integrated,
                authority,
            )

        changed_shape = replace(
            ordinary[1],
            plan=replace(
                ordinary[1].plan,
                fingerprint=replace(
                    ordinary[1].plan.fingerprint,
                    commands=("validation:changed",),
                ),
            ),
        )
        with self.assertRaises(ContractError):
            build_stability_inputs(
                (ordinary[0], changed_shape, *ordinary[2:]),
                certification,
                cache_disabled,
                integrated,
                authority,
            )


if __name__ == "__main__":
    unittest.main()
