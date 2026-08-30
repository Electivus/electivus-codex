import unittest
from dataclasses import replace

from validation_admission import evaluate_admission
from validation_contracts import CandidateIdentity
from validation_contracts import EvidenceRequirement
from validation_plan import build_plan
from validation_result import manifest_for_requirement
from validation_state import CLEAN
from validation_state import RECOVERY


class ValidationAdmissionTests(unittest.TestCase):
    def test_only_exact_passed_integrated_evidence_allows_admission(self):
        current_main_sha = "a" * 40
        candidate = CandidateIdentity(
            event_name="push",
            repository="Electivus/electivus-codex",
            default_branch="main",
            candidate_sha=current_main_sha,
            base_sha=None,
            head_sha=None,
            kind="integrated",
            branch="main",
        )
        plan = build_plan(candidate, ["codex-rs/core/src/lib.rs"])
        requirement = EvidenceRequirement(
            family="integrated-certification",
            stage="integrated",
            selected=True,
            disposition="required",
            reason="exact main commit was certified",
            retention_class="integrated-certification",
        )
        manifest = manifest_for_requirement(plan, requirement)

        decision = evaluate_admission(current_main_sha, manifest)
        self.assertTrue(decision.allowed)
        self.assertEqual(CLEAN, decision.state)
        self.assertFalse(decision.certification_lock_active)

        failed = evaluate_admission(
            current_main_sha, replace(manifest, outcome="product-failure")
        )
        self.assertFalse(failed.allowed)
        self.assertEqual(RECOVERY, failed.state)
        self.assertTrue(failed.certification_lock_active)

    def test_stale_integrated_evidence_keeps_the_lock_active(self):
        current_main_sha = "b" * 40
        candidate = CandidateIdentity(
            event_name="push",
            repository="Electivus/electivus-codex",
            default_branch="main",
            candidate_sha="a" * 40,
            base_sha=None,
            head_sha=None,
            kind="integrated",
            branch="main",
        )
        plan = build_plan(candidate, ["codex-rs/core/src/lib.rs"])
        manifest = manifest_for_requirement(
            plan,
            EvidenceRequirement(
                family="integrated-certification",
                stage="integrated",
                selected=True,
                disposition="required",
                reason="exact main commit was certified",
                retention_class="integrated-certification",
            ),
        )

        decision = evaluate_admission(current_main_sha, manifest)
        self.assertFalse(decision.allowed)
        self.assertTrue(decision.certification_lock_active)


if __name__ == "__main__":
    unittest.main()
