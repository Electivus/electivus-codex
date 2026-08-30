#!/usr/bin/env python3
"""Build Stability inputs only from exact retained Validation artifacts."""

import argparse
from dataclasses import dataclass
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import EvidenceManifest
from validation_contracts import ValidationReport
from validation_contracts import parse_manifest
from validation_observability import CACHE_MODES
from validation_observability import LatencySample
from validation_reports import parse_report
from validation_stability import StabilityRecord


MINIMUM_ORDINARY_REPORTS = 20
MAXIMUM_ORDINARY_REPORTS = 50
REQUIRED_DURATIONS = (
    "firstActionableFailure",
    "mergeGate",
    "automatedMergeReadiness",
    "certificationRequired",
    "integratedCertification",
)


@dataclass(frozen=True)
class StabilityInputs:
    records: tuple[StabilityRecord, ...]
    samples: tuple[LatencySample, ...]
    resulting_main_sha: str


def _report(path: Path) -> ValidationReport:
    try:
        return parse_report(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError, ContractError) as error:
        raise ContractError(f"cannot read Validation report {path}: {error}") from error


def _single_report(directory: Path, name: str) -> ValidationReport:
    if not directory.is_dir():
        raise ContractError(f"{name} artifact directory is missing: {directory}")
    paths = sorted(directory.rglob("validation-report.json"))
    if len(paths) != 1:
        raise ContractError(
            f"{name} artifact must contain exactly one Validation report, found {len(paths)}"
        )
    return _report(paths[0])


def _single_integrated_manifest(directory: Path) -> EvidenceManifest:
    paths = sorted(directory.rglob("integrated.json"))
    if len(paths) != 1:
        raise ContractError(
            "Integrated artifact must contain exactly one authority manifest, "
            f"found {len(paths)}"
        )
    try:
        return parse_manifest(paths[0].read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError, ContractError) as error:
        raise ContractError(
            f"cannot read Integrated authority manifest: {error}"
        ) from error


def _cache_mode(report: ValidationReport, name: str) -> str:
    cache_mode = report.cache_fallback
    if cache_mode == "not-applicable":
        cache_mode = "not-used"
    if cache_mode not in CACHE_MODES:
        raise ContractError(
            f"{name} has an unsupported cache disposition: {cache_mode}"
        )
    return cache_mode


def _retry_count(report: ValidationReport) -> int:
    attempts = tuple(
        manifest.attempt
        for manifest in report.evidence
        if manifest.disposition == "required"
    )
    return max(attempts, default=1) - 1


def _generation_core(report: ValidationReport) -> tuple[object, ...]:
    fingerprint = report.plan.fingerprint
    dependencies = tuple(
        item for item in fingerprint.dependencies if item[0] != "selectedEvidence"
    )
    return (
        fingerprint.validation_implementation,
        dependencies,
        fingerprint.toolchains,
    )


def _generation_shape(report: ValidationReport) -> tuple[object, ...]:
    fingerprint = report.plan.fingerprint
    return fingerprint.commands, fingerprint.platforms


def _validate_generation(reports: tuple[ValidationReport, ...]) -> None:
    if not reports:
        raise ContractError("Stability requires at least one Validation report")
    expected_core = _generation_core(reports[0])
    shapes_by_lane: dict[tuple[str, str], set[tuple[object, ...]]] = {}
    for report in reports:
        if _generation_core(report) != expected_core:
            raise ContractError(
                "Stability reports must use one validation implementation, dependency, "
                "and toolchain generation"
            )
        lane = (report.plan.profile, report.candidate.kind)
        shapes_by_lane.setdefault(lane, set()).add(_generation_shape(report))
    changed_lanes = tuple(
        lane for lane, shapes in shapes_by_lane.items() if len(shapes) != 1
    )
    if changed_lanes:
        rendered = ", ".join(f"{profile}/{kind}" for profile, kind in changed_lanes)
        raise ContractError(
            "Stability reports changed validation commands or platforms within lane: "
            f"{rendered}"
        )


def _require_shadow_profile(
    report: ValidationReport,
    *,
    profile: str,
    name: str,
) -> None:
    if report.candidate.kind != "pull-request":
        raise ContractError(f"{name} is not a Pull request candidate")
    if report.candidate.base_sha is None or report.candidate.head_sha is None:
        raise ContractError(f"{name} is missing Pull request base or head identity")
    if report.plan.profile != profile:
        raise ContractError(
            f"{name} has profile {report.plan.profile}, expected {profile}"
        )


def _record(
    report: ValidationReport,
    *,
    profile: str,
    name: str,
    integrated_sha: str | None = None,
) -> StabilityRecord:
    cache_mode = _cache_mode(report, name)
    return StabilityRecord(
        candidate_sha=report.candidate.candidate_sha,
        profile=profile,
        outcome=report.outcome,
        retry_count=_retry_count(report),
        cache_mode=cache_mode,
        integrated_sha=integrated_sha,
    )


def _sample(report: ValidationReport, name: str) -> LatencySample:
    durations = dict(report.durations)
    missing = tuple(metric for metric in REQUIRED_DURATIONS if metric not in durations)
    if missing:
        raise ContractError(f"{name} is missing duration metrics: {', '.join(missing)}")
    return LatencySample(
        candidate_sha=report.candidate.candidate_sha,
        profile=report.plan.profile,
        outcome=report.outcome,
        first_actionable_failure=durations["firstActionableFailure"],
        merge_gate=durations["mergeGate"],
        automated_merge_readiness=durations["automatedMergeReadiness"],
        certification_required=durations["certificationRequired"],
        integrated_certification=durations["integratedCertification"],
        cache_mode=_cache_mode(report, name),
    )


def build_stability_inputs(
    ordinary_reports: tuple[ValidationReport, ...],
    certification_report: ValidationReport,
    cache_disabled_report: ValidationReport,
    integrated_report: ValidationReport,
    integrated_manifest: EvidenceManifest,
) -> StabilityInputs:
    """Validate report identities and derive the finite Stability contract."""
    if (
        not MINIMUM_ORDINARY_REPORTS
        <= len(ordinary_reports)
        <= MAXIMUM_ORDINARY_REPORTS
    ):
        raise ContractError(
            f"Stability requires {MINIMUM_ORDINARY_REPORTS}-{MAXIMUM_ORDINARY_REPORTS} "
            f"ordinary reports, found {len(ordinary_reports)}"
        )
    candidate_shas = [report.candidate.candidate_sha for report in ordinary_reports]
    if len(set(candidate_shas)) != len(candidate_shas):
        raise ContractError(
            "ordinary Stability reports must identify distinct candidates"
        )
    for index, report in enumerate(ordinary_reports, start=1):
        name = f"ordinary report {index}"
        _require_shadow_profile(report, profile="ordinary", name=name)
        if _cache_mode(report, name) == "disabled-reconstruction":
            raise ContractError(
                "ordinary reports must keep the cache-disabled run separate"
            )
    _require_shadow_profile(
        certification_report,
        profile="certification-required",
        name="Certification-required report",
    )
    if (
        certification_report.outcome != "passed"
        or not certification_report.admission_allowed
    ):
        raise ContractError(
            "Certification-required Stability report must be admitted and passed"
        )
    if (
        cache_disabled_report.candidate.kind != "pull-request"
        or cache_disabled_report.candidate.base_sha is None
        or cache_disabled_report.candidate.head_sha is None
    ):
        raise ContractError(
            "cache-disabled Stability report has an invalid Pull request identity"
        )
    if cache_disabled_report.plan.profile != "ordinary":
        raise ContractError(
            "cache-disabled Stability report must use the ordinary profile"
        )
    if cache_disabled_report.candidate != ordinary_reports[0].candidate:
        raise ContractError(
            "cache-disabled Stability report must reconstruct the first ordinary candidate"
        )
    if cache_disabled_report.plan.fingerprint != ordinary_reports[0].plan.fingerprint:
        raise ContractError(
            "cache-disabled Stability report must use the first ordinary plan"
        )
    if cache_disabled_report.cache_fallback != "disabled-reconstruction":
        raise ContractError(
            "cache-disabled Stability report must record disabled-reconstruction"
        )
    if (
        cache_disabled_report.outcome != "passed"
        or not cache_disabled_report.admission_allowed
    ):
        raise ContractError(
            "cache-disabled Stability report must be admitted and passed"
        )
    if (
        integrated_report.candidate.kind != "integrated"
        or integrated_report.candidate.base_sha is not None
        or integrated_report.candidate.head_sha is not None
        or integrated_report.plan.profile != "certification-required"
    ):
        raise ContractError(
            "Integrated Stability report has an invalid identity or profile"
        )
    if integrated_report.outcome != "passed" or not integrated_report.admission_allowed:
        raise ContractError("Integrated Stability report must be admitted and passed")
    if (
        integrated_manifest.family != "integrated-certification"
        or integrated_manifest.stage != "integrated"
        or integrated_manifest.disposition != "required"
        or integrated_manifest.outcome != integrated_report.outcome
        or integrated_manifest.candidate != integrated_report.candidate
        or integrated_manifest.fingerprint != integrated_report.plan.fingerprint
        or integrated_manifest.attempt != _retry_count(integrated_report) + 1
    ):
        raise ContractError("Integrated authority manifest does not match its report")

    _validate_generation(
        (*ordinary_reports, certification_report, cache_disabled_report, integrated_report)
    )

    ordinary_record = _record(
        ordinary_reports[0], profile="ordinary", name="ordinary report 1"
    )
    if (
        ordinary_record.outcome != "passed"
        or ordinary_record.retry_count != 0
        or ordinary_record.cache_mode == "disabled-reconstruction"
    ):
        raise ContractError(
            "the first ordinary Stability report must be a retry-free passed candidate"
        )
    certification_record = _record(
        certification_report,
        profile="certification-required",
        name="Certification-required report",
    )
    cache_disabled_record = _record(
        cache_disabled_report,
        profile="ordinary",
        name="cache-disabled report",
    )
    integrated_sha = integrated_report.candidate.candidate_sha
    integrated_record = _record(
        integrated_report,
        profile="integrated",
        name="Integrated report",
        integrated_sha=integrated_sha,
    )
    samples = tuple(
        [
            _sample(report, f"ordinary report {index}")
            for index, report in enumerate(ordinary_reports, start=1)
        ]
        + [_sample(cache_disabled_report, "cache-disabled report")]
    )
    return StabilityInputs(
        records=(
            ordinary_record,
            certification_record,
            cache_disabled_record,
            integrated_record,
        ),
        samples=samples,
        resulting_main_sha=integrated_sha,
    )


def _record_to_dict(record: StabilityRecord) -> dict[str, object]:
    return {
        "candidateSha": record.candidate_sha,
        "profile": record.profile,
        "outcome": record.outcome,
        "retryCount": record.retry_count,
        "cacheMode": record.cache_mode,
        "integratedSha": record.integrated_sha,
    }


def _sample_to_dict(sample: LatencySample) -> dict[str, object]:
    return {
        "candidateSha": sample.candidate_sha,
        "profile": sample.profile,
        "outcome": sample.outcome,
        "firstActionableFailure": sample.first_actionable_failure,
        "mergeGate": sample.merge_gate,
        "automatedMergeReadiness": sample.automated_merge_readiness,
        "certificationRequired": sample.certification_required,
        "integratedCertification": sample.integrated_certification,
        "cacheMode": sample.cache_mode,
    }


def run(args: argparse.Namespace) -> int:
    ordinary_reports = tuple(_report(path) for path in args.ordinary_report)
    inputs = build_stability_inputs(
        ordinary_reports,
        _single_report(args.certification_dir, "Certification-required"),
        _single_report(args.cache_disabled_dir, "cache-disabled"),
        _single_report(args.integrated_dir, "Integrated"),
        _single_integrated_manifest(args.integrated_dir),
    )
    args.records_output.parent.mkdir(parents=True, exist_ok=True)
    args.records_output.write_text(
        json.dumps([_record_to_dict(record) for record in inputs.records], indent=2)
        + "\n",
        encoding="utf-8",
    )
    args.samples_output.parent.mkdir(parents=True, exist_ok=True)
    args.samples_output.write_text(
        json.dumps([_sample_to_dict(sample) for sample in inputs.samples], indent=2)
        + "\n",
        encoding="utf-8",
    )
    args.resulting_main_sha_output.parent.mkdir(parents=True, exist_ok=True)
    args.resulting_main_sha_output.write_text(
        inputs.resulting_main_sha + "\n", encoding="utf-8"
    )
    print(f"Built Stability inputs from {len(ordinary_reports)} exact ordinary reports")
    print(f"Resulting main SHA: {inputs.resulting_main_sha}")
    return 0


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--ordinary-report", type=Path, action="append", required=True)
    command.add_argument("--certification-dir", type=Path, required=True)
    command.add_argument("--cache-disabled-dir", type=Path, required=True)
    command.add_argument("--integrated-dir", type=Path, required=True)
    command.add_argument("--records-output", type=Path, required=True)
    command.add_argument("--samples-output", type=Path, required=True)
    command.add_argument("--resulting-main-sha-output", type=Path, required=True)
    return command


def main(argv: list[str] | None = None) -> int:
    try:
        return run(parser().parse_args(argv))
    except (ContractError, OSError, ValueError) as error:
        print(f"Stability input construction failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
