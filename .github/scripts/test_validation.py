import json
from dataclasses import replace
from pathlib import Path
import tempfile
import unittest

from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_contracts import parse_plan
from validation_reports import report_to_dict
from validation_contracts import serialize_plan
from validation_contracts import validate_manifest
from validation_entrypoint import main as entrypoint_main
from validation_codeql import CodeqlLanguageResult
from validation_codeql import aggregate_codeql
from validation_comparison import ValidationObservation
from validation_comparison import compare
from validation_cutover import CutoverAuthorization
from validation_cutover import require_no_gap
from validation_cutover import validate_cutover
from validation_integrated import integrated_manifest
from validation_observability import LatencySample
from validation_observability import QuarantinedCheck
from validation_observability import SurveillanceRun
from validation_observability import detect_drift
from validation_observability import evaluate_slo
from validation_observability import retention_class_for
from validation_observability import surveillance_cancellation_allowed
from validation_observability import validate_quarantine
from validation_observability_cli import main as observability_main
from validation_plan import EVIDENCE_FAMILIES
from validation_plan import build_plan
from validation_plan import classify_changed_files
from validation_result import aggregate
from validation_result import manifest_for_requirement
from validation_state import DEGRADED
from validation_state import RECOVERY
from validation_state import CLEAN
from validation_state import IntegratedAttempt
from validation_state import RecoveryAuthorization
from validation_state import certification_lock
from validation_state import consume_recovery_authorization
from validation_state import derive_state
from validation_state import recovery_admission_allowed
from validation_state import reject_automatic_recovery
from validation_state import validate_integrated_attempts
from validation_release import PublicationRequest
from validation_release import ReleaseArtifact
from validation_release import certify_artifacts
from validation_release import artifact_set_from_dict
from validation_release import artifact_set_to_dict
from validation_release import verify_promotion
from check_validation_topology import main as validation_topology_main


class ValidationTests(unittest.TestCase):
    candidate = CandidateIdentity(
        "pull_request",
        "Electivus/electivus-codex",
        "main",
        "a" * 40,
        "b" * 40,
        "c" * 40,
        "pull-request",
        181,
        "feature",
    )

    def all_manifests(self, plan, **kwargs):
        return tuple(
            manifest_for_requirement(plan, requirement, **kwargs)
            for requirement in plan.requirements
        )

    def test_repository_only_plan_is_exact_and_has_all_dispositions(self):
        plan = build_plan(self.candidate, ["README.md", "docs/guide.md"])
        self.assertEqual(
            ("repository-documentation",),
            plan.surfaces,
        )
        self.assertEqual("ordinary", plan.profile)
        self.assertEqual((), plan.risk_modifiers)
        self.assertEqual(
            ("repository-hygiene",),
            tuple(item.family for item in plan.requirements if item.selected),
        )
        self.assertEqual(len(EVIDENCE_FAMILIES), len(plan.requirements))
        self.assertTrue(all(item.disposition in {"required", "not-required"} for item in plan.requirements))
        self.assertEqual(plan, parse_plan(serialize_plan(plan)))

    def test_surface_and_modifier_classification_is_additive(self):
        classification = classify_changed_files(
            [
                "codex-rs/app-server-protocol/src/v2.rs",
                "codex-rs/state/src/lib.rs",
                ".github/workflows/validation-shadow.yml",
                ".github/workflows/upstream-release-sync.yml",
            ],
            branch="automation/upstream-sync/" + "d" * 40,
        )
        self.assertEqual(
            (
                "rust",
                "api-protocol-sdk",
                "runtime-state-postgresql",
                "platform-build",
                "validation-architecture",
                "upstream-synchronization",
            ),
            classification.surfaces,
        )
        self.assertEqual(
            ("security", "breaking", "migration", "validation-authority", "synchronization"),
            classification.risk_modifiers,
        )

    def test_each_surface_and_risk_modifier_has_an_additive_fixture(self):
        surface_paths = {
            "repository-documentation": "README.md",
            "rust": "codex-rs/core/src/lib.rs",
            "api-protocol-sdk": "codex-rs/app-server-protocol/src/lib.rs",
            "runtime-state-postgresql": "codex-rs/state/src/runtime/threads.rs",
            "execution-sandbox-v8": "codex-rs/v8-poc/src/lib.rs",
            "platform-build": ".github/workflows/validation-shadow.yml",
            "package-release": "scripts/codex_package/archive.py",
            "validation-architecture": ".github/scripts/validation_plan.py",
            "upstream-synchronization": ".github/upstream-sync-manifests/b3a6d7f67cf056e18472c2b9ec26d3999ed40b7b.json",
        }
        for surface, path in surface_paths.items():
            with self.subTest(surface=surface):
                self.assertIn(surface, classify_changed_files([path]).surfaces)
        modifier_paths = {
            "security": ".github/workflows/validation-shadow.yml",
            "breaking": "sdk/typescript/src/index.ts",
            "migration": "codex-rs/state/goals_migrations/0001_thread_goals.sql",
            "publication": "scripts/codex_package/archive.py",
            "validation-authority": ".github/scripts/validation_state.py",
            "synchronization": ".github/scripts/upstream_sync_manifest.py",
        }
        for modifier, path in modifier_paths.items():
            with self.subTest(modifier=modifier):
                self.assertIn(modifier, classify_changed_files([path]).risk_modifiers)

    def test_unknown_and_uncertain_inputs_select_broad_certification(self):
        plan = build_plan(
            self.candidate,
            ["new/unsupported/file.weird"],
            metadata={"comparison_failed": True},
        )
        self.assertEqual("certification-required", plan.profile)
        self.assertIn("unknown", plan.risk_modifiers)
        selected = {item.family for item in plan.requirements if item.selected}
        self.assertTrue({"linux-x64-cargo", "linux-arm64", "linux-musl", "codeql-advanced"} <= selected)
        self.assertEqual(("rust", "python", "javascript-typescript"), plan.codeql_languages)
        self.assertTrue(plan.policy_errors)

    def test_every_codeql_language_is_attributable_and_missing_parallel_work_fails(self):
        plan = build_plan(self.candidate, ["new/unsupported/file.weird"])
        selected = tuple(
            CodeqlLanguageResult(language, "a" * 40, "passed", 10)
            for language in plan.codeql_languages
        )
        manifest = aggregate_codeql(plan, selected)
        self.assertEqual("passed", manifest.outcome)
        missing = aggregate_codeql(plan, selected[:-1])
        self.assertEqual("indeterminate", missing.outcome)
        stale = aggregate_codeql(
            plan,
            (*selected[:-1], CodeqlLanguageResult(plan.codeql_languages[-1], "e" * 40, "passed", 10)),
        )
        self.assertEqual("indeterminate", stale.outcome)
        docs_plan = build_plan(self.candidate, ["README.md"])
        self.assertEqual("not-required", aggregate_codeql(docs_plan, ()).outcome)

    def test_fingerprint_changes_when_any_identity_or_parameter_changes(self):
        plan = build_plan(self.candidate, ["codex-rs/core/src/lib.rs"])
        changed_candidate = replace(self.candidate, candidate_sha="e" * 40)
        changed = build_plan(changed_candidate, ["codex-rs/core/src/lib.rs"])
        changed_surface = build_plan(self.candidate, ["sdk/python/foo.py"])
        self.assertNotEqual(plan.fingerprint.digest, changed.fingerprint.digest)
        self.assertNotEqual(plan.fingerprint.digest, changed_surface.fingerprint.digest)
        self.assertEqual(64, len(plan.fingerprint.digest))

    def test_plan_rejects_a_mutated_fingerprint_identity(self):
        plan = build_plan(self.candidate, ["README.md"])
        payload = json.loads(serialize_plan(plan))
        payload["fingerprint"]["source"][0][1] = "e" * 40
        with self.assertRaises(ContractError):
            parse_plan(json.dumps(payload))

    def test_ordinary_rust_aggregation_is_complete_and_passes(self):
        plan = build_plan(self.candidate, ["codex-rs/core/src/lib.rs"])
        result = aggregate(plan, self.all_manifests(plan), current_candidate=self.candidate)
        self.assertEqual("passed", result.report.outcome)
        self.assertTrue(result.report.admission_allowed)
        self.assertEqual(len(EVIDENCE_FAMILIES), len(result.report.evidence))
        self.assertEqual((), result.report.errors)

    def test_missing_mismatched_expired_and_cache_only_evidence_fail_closed(self):
        plan = build_plan(self.candidate, ["codex-rs/core/src/lib.rs"])
        required = next(item for item in plan.requirements if item.selected and item.family != "repository-hygiene")
        good = manifest_for_requirement(plan, required)
        missing = aggregate(plan, [manifest_for_requirement(plan, plan.requirements[0])], current_candidate=self.candidate)
        self.assertFalse(missing.report.admission_allowed)
        self.assertIn("missing required evidence", " ".join(missing.report.errors))
        foreign = replace(good, candidate=replace(self.candidate, candidate_sha="e" * 40))
        mismatched = aggregate(
            plan,
            [manifest_for_requirement(plan, plan.requirements[0]), foreign],
            current_candidate=self.candidate,
        )
        self.assertFalse(mismatched.report.admission_allowed)
        self.assertIn("stale", dict(mismatched.report.outcomes)[required.family])
        cache_only = replace(good, cache_mode="cache-only")
        cached = aggregate(
            plan,
            [manifest_for_requirement(plan, plan.requirements[0]), cache_only],
            current_candidate=self.candidate,
        )
        self.assertFalse(cached.report.admission_allowed)
        self.assertIn("cache-only", " ".join(cached.report.errors))
        expired = replace(good, created_at=1, expires_at=2)
        with self.assertRaises(ContractError):
            validate_manifest(expired)

    def test_slo_uses_latest_fifty_eligible_and_excludes_unreliable_outcomes(self):
        samples = tuple(
            LatencySample(
                candidate_sha=f"{index + 1:040x}",
                profile="ordinary",
                outcome="passed" if index % 2 == 0 else "product-failure",
                first_actionable_failure=100,
                merge_gate=100 + index,
                automated_merge_readiness=200,
                cache_mode="cold" if index == 0 else "not-used",
            )
            for index in range(55)
        ) + (
            LatencySample("f" * 40, "ordinary", "stale", 999, 999, 999),
            LatencySample("e" * 40, "ordinary", "infrastructure-failure", 999, 999, 999),
        )
        evaluation = evaluate_slo(
            samples,
            metric="mergeGate",
            objective_seconds=1_000,
        )
        self.assertEqual(50, evaluation.sample_count)
        self.assertEqual(129, evaluation.p50_seconds)
        self.assertEqual(152, evaluation.p95_seconds)
        self.assertFalse(evaluation.breached)
        breach = evaluate_slo(
            samples,
            metric="mergeGate",
            objective_seconds=100,
            previous_evaluation_breached=True,
        )
        self.assertTrue(breach.breached)
        first_breach = evaluate_slo(
            samples,
            metric="mergeGate",
            objective_seconds=100,
        )
        self.assertTrue(first_breach.current_breach)
        self.assertFalse(first_breach.breached)
        second_breach = evaluate_slo(
            samples,
            metric="mergeGate",
            objective_seconds=100,
            previous_evaluation_breached=first_breach.current_breach,
        )
        self.assertTrue(second_breach.breached)

    def test_retention_quarantine_surveillance_and_drift_contracts(self):
        self.assertEqual("ordinary-pull-request", retention_class_for("ordinary", "merge-gate"))
        self.assertEqual("integrated-certification", retention_class_for("integrated", "integrated"))
        quarantine = QuarantinedCheck(
            "check-id",
            "a" * 40,
            "linux-x64",
            ("evidence-id",),
            "two independent intermittent failures",
            "#200",
            100,
            100 + 7 * 86_400,
            "surveillance",
        )
        validate_quarantine(quarantine)
        with self.assertRaises(ContractError):
            validate_quarantine(replace(quarantine, expires_at=100 + 7 * 86_400 + 1))
        previous = SurveillanceRun("old", "dependencies", "a" * 40, 1, "passed")
        newer = SurveillanceRun("new", "dependencies", "b" * 40, 2, "passed")
        self.assertTrue(surveillance_cancellation_allowed(previous, newer))
        self.assertFalse(
            surveillance_cancellation_allowed(previous, replace(newer, profile="toolchains"))
        )
        self.assertEqual(
            ("dependencies", "external-assumptions"),
            detect_drift(
                {"dependencies": "1", "toolchains": "1", "external_assumptions": "1"},
                {"dependencies": "2", "toolchains": "1", "external_assumptions": "2"},
            ),
        )

    def test_integrated_state_lock_retry_and_recovery_are_exact(self):
        integrated_candidate = replace(
            self.candidate,
            kind="integrated",
            event_name="push",
            base_sha=None,
            head_sha=None,
            pull_request_number=None,
            branch="main",
        )
        integrated_plan = build_plan(integrated_candidate, ["codex-rs/core/src/lib.rs"])
        integrated_requirement = replace(
            next(item for item in integrated_plan.requirements if item.family == "linux-x64-cargo"),
            stage="integrated",
            retention_class="integrated-certification",
            selected=True,
            disposition="required",
        )
        integrated = manifest_for_requirement(integrated_plan, integrated_requirement)
        self.assertEqual(CLEAN, derive_state("a" * 40, integrated).state)
        self.assertEqual(DEGRADED, derive_state("b" * 40, integrated).state)
        failed = replace(integrated, outcome="product-failure")
        decision = derive_state("a" * 40, failed)
        self.assertEqual(RECOVERY, decision.state)
        self.assertFalse(recovery_admission_allowed(RECOVERY, None, merge_gate_passed=True, review_passed=True))
        lock = certification_lock(("a" * 40, "b" * 40), ("a" * 40,))
        self.assertTrue(lock.active)
        self.assertFalse(lock.ordinary_admission_allowed)
        with self.assertRaises(ContractError):
            certification_lock(("a" * 40, "b" * 40, "c" * 40), ("a" * 40,))
        validate_integrated_attempts((IntegratedAttempt("a" * 40, "product-failure", 1), IntegratedAttempt("a" * 40, "passed", 2)))
        with self.assertRaises(ContractError):
            validate_integrated_attempts((IntegratedAttempt("a" * 40, "passed", 1, True),))
        authorization = RecoveryAuthorization(
            "auth-1",
            "a" * 40,
            "c" * 40,
            "b" * 40,
            "correction",
            "grantor",
            "audit",
            181,
            "d" * 64,
        )
        consumed = consume_recovery_authorization(
            authorization,
            state=RECOVERY,
            failed_integrated_sha="a" * 40,
            candidate_head_sha="c" * 40,
            current_base_sha="b" * 40,
            action_type="correction",
            pull_request_number=181,
            validation_fingerprint="d" * 64,
        )
        self.assertTrue(consumed.consumed)
        with self.assertRaises(ContractError):
            consume_recovery_authorization(
                consumed,
                state=RECOVERY,
                failed_integrated_sha="a" * 40,
                candidate_head_sha="c" * 40,
                current_base_sha="b" * 40,
                action_type="correction",
                pull_request_number=181,
                validation_fingerprint="d" * 64,
            )
        with self.assertRaises(ContractError):
            reject_automatic_recovery("automatic-revert")

    def test_integrated_report_becomes_exact_state_authority_manifest(self):
        integrated_candidate = replace(
            self.candidate,
            event_name="push",
            base_sha=None,
            head_sha=None,
            kind="integrated",
            pull_request_number=None,
            branch="main",
        )
        plan = build_plan(integrated_candidate, ["codex-rs/core/src/lib.rs"])
        result = aggregate(plan, self.all_manifests(plan), current_candidate=integrated_candidate)
        manifest = integrated_manifest(report_to_dict(result.report))
        self.assertEqual("integrated", manifest.stage)
        self.assertEqual("a" * 40, manifest.candidate.candidate_sha)
        self.assertEqual("passed", manifest.outcome)
        self.assertEqual(CLEAN, derive_state("a" * 40, manifest).state)

    def test_integrated_plan_keeps_full_depth_even_for_documentation_changes(self):
        candidate = replace(
            self.candidate,
            event_name="push",
            base_sha=None,
            head_sha=None,
            kind="integrated",
            pull_request_number=None,
            branch="main",
        )
        plan = build_plan(candidate, ["README.md"])
        selected = {item.family for item in plan.requirements if item.selected}
        self.assertTrue(
            {
                "linux-x64-bazel",
                "linux-x64-cargo",
                "postgresql",
                "v8",
                "windows-x64",
                "linux-arm64",
                "linux-musl",
            }
            <= selected
        )
        self.assertEqual("integrated-certification", next(
            item.retention_class
            for item in plan.requirements
            if item.family == "linux-arm64"
        ))

    def release_artifacts(self):
        return tuple(
            ReleaseArtifact(
                name=f"codex-{platform}",
                digest=f"{index + 1:064x}",
                platform=platform,
                packaging="tar.gz" if platform.startswith("linux") else "zip",
                producer="release-build",
                provenance_digest=f"{index + 5:064x}",
                signature_digest=(
                    f"{index + 9:064x}" if platform.startswith("linux") else None
                ),
            )
            for index, platform in enumerate(
                ("linux-x64", "linux-arm64", "windows-x64", "windows-arm64")
            )
        )

    def test_release_certification_and_promotion_use_one_exact_artifact_set(self):
        integrated_candidate = replace(
            self.candidate,
            kind="integrated",
            event_name="push",
            base_sha=None,
            head_sha=None,
            pull_request_number=None,
            branch="main",
        )
        integrated_plan = build_plan(integrated_candidate, ["codex-rs/core/src/lib.rs"])
        integrated_report = aggregate(
            integrated_plan,
            self.all_manifests(integrated_plan),
            current_candidate=integrated_candidate,
        )
        integrated = integrated_manifest(report_to_dict(integrated_report.report))
        artifact_set = certify_artifacts(integrated, self.release_artifacts())
        request = PublicationRequest(
            artifact_set.source_sha,
            artifact_set.certification_manifest_id,
            artifact_set.artifacts,
            public_authorized=True,
        )
        verify_promotion(artifact_set, request)
        for field in ("rebuild", "repackage", "resign"):
            with self.subTest(field=field):
                with self.assertRaises(ContractError):
                    verify_promotion(artifact_set, replace(request, **{field: True}))
        with self.assertRaises(ContractError):
            verify_promotion(artifact_set, request, state=DEGRADED)
        with self.assertRaises(ContractError):
            verify_promotion(artifact_set, replace(request, public_authorized=False))
        self.assertEqual(
            artifact_set,
            artifact_set_from_dict(artifact_set_to_dict(artifact_set)),
        )

    def test_cutover_is_atomic_and_requires_no_gap(self):
        decision = validate_cutover(
            CutoverAuthorization(
                "main",
                "a" * 40,
                "CI required",
                "CI required",
                True,
                True,
                True,
                True,
                True,
                "cutover-1",
                "operator-1",
            )
        )
        self.assertTrue(decision.allowed)
        self.assertEqual(4, len(decision.atomic_operations))
        with self.assertRaises(ContractError):
            require_no_gap(False, False)

    def test_legacy_replacement_comparison_is_exact_and_name_safe(self):
        evidence = (
            ("repository-hygiene", ("required", "passed")),
            ("linux-x64-bazel", ("required", "passed")),
        )
        legacy = ValidationObservation(
            "a" * 40,
            "b" * 40,
            "c" * 40,
            "d" * 64,
            evidence,
            "passed",
            "CI required",
            (("mergeGate", 10.0),),
        )
        replacement = replace(legacy, check_name="Validation Shadow")
        self.assertTrue(compare(legacy, replacement).equivalent)
        slower = replace(replacement, durations=(("mergeGate", 11.0),))
        slower_decision = compare(legacy, slower)
        self.assertTrue(slower_decision.equivalent)
        self.assertEqual((("mergeGate", 10.0, 11.0),), slower_decision.latency_deltas)
        collision = replace(replacement, check_name="CI required")
        self.assertFalse(compare(legacy, collision).equivalent)
        self.assertIn("collides", " ".join(compare(legacy, collision).differences))

    def test_entrypoint_emits_bounded_plan_fingerprint_report_and_preflight_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            status = entrypoint_main(
                [
                    "--output-dir",
                    str(output),
                    "--root",
                    str(Path.cwd()),
                    "--event-name",
                    "pull_request",
                    "--repository",
                    "Electivus/electivus-codex",
                    "--default-branch",
                    "main",
                    "--candidate",
                    "a" * 40,
                    "--base",
                    "b" * 40,
                    "--head",
                    "c" * 40,
                    "--pull-request",
                    "181",
                    "--branch",
                    "feature",
                    "--changed-file",
                    "README.md",
                ]
            )
            self.assertEqual(0, status)
            self.assertTrue((output / "validation-plan.json").is_file())
            self.assertTrue((output / "validation-fingerprint.json").is_file())
            self.assertTrue((output / "validation-report.json").is_file())
            self.assertTrue((output / "evidence/preflight.json").is_file())
            self.assertEqual("passed", json.loads((output / "validation-report.json").read_text())["outcome"])

    def test_entrypoint_rejects_unpinned_workflow_without_compiling_product(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workflow = root / ".github/workflows/bad.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text("name: bad\non:\n  pull_request:\njobs:\n  test:\n    uses: actions/checkout@v1\n")
            input_path = root / "input.json"
            candidate = self.candidate
            input_path.write_text(
                json.dumps(
                    {
                        "candidate": {
                            "eventName": candidate.event_name,
                            "repository": candidate.repository,
                            "defaultBranch": candidate.default_branch,
                            "candidateSha": candidate.candidate_sha,
                            "baseSha": candidate.base_sha,
                            "headSha": candidate.head_sha,
                            "kind": candidate.kind,
                            "pullRequestNumber": candidate.pull_request_number,
                            "branch": candidate.branch,
                        },
                        "changedFiles": [".github/workflows/bad.yml"],
                        "metadata": {"comparison_status": "ok"},
                    }
                )
            )
            output = root / "output"
            status = entrypoint_main(
                ["--input", str(input_path), "--output-dir", str(output), "--root", str(root), "--preflight-only"]
            )
            self.assertEqual(1, status)
            self.assertTrue((output / "validation-report.md").is_file())

    def test_observability_cli_reads_reports_and_preserves_latency_contract(self):
        plan = build_plan(self.candidate, ["README.md"])
        result = aggregate(plan, self.all_manifests(plan), current_candidate=self.candidate)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reports = root / "reports" / "one"
            reports.mkdir(parents=True)
            (reports / "validation-report.json").write_text(
                json.dumps(report_to_dict(result.report)), encoding="utf-8"
            )
            status = observability_main(
                [
                    "--reports-dir",
                    str(root / "reports"),
                    "--output",
                    str(root / "slo.json"),
                    "--markdown",
                    str(root / "slo.md"),
                ]
            )
            self.assertEqual(0, status)
            payload = json.loads((root / "slo.json").read_text())
            self.assertEqual(1, payload["sampleCount"])
            self.assertEqual(1, payload["slo"]["mergeGate"]["sampleCount"])

    def test_validation_topology_is_present_and_pinned(self):
        self.assertEqual(0, validation_topology_main(["--repo", str(Path.cwd())]))


if __name__ == "__main__":
    unittest.main()
