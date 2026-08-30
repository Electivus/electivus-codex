#!/usr/bin/env python3
"""Latency, reliability, retention, quarantine, and Surveillance contracts."""

from dataclasses import dataclass
import math
from typing import Iterable, Mapping

from validation_contracts import ContractError
from validation_contracts import SHA1_PATTERN


ORDINARY_ELIGIBLE_OUTCOMES = frozenset({"passed", "product-failure"})
EXCLUDED_FROM_LATENCY = frozenset(
    {"stale", "cancelled", "infrastructure-failure", "indeterminate"}
)
CACHE_MODES = frozenset(
    {"not-used", "cold", "cache-hit-verified", "disabled-reconstruction"}
)
SLO_OBJECTIVES = {
    "firstActionableFailure": 300.0,
    "mergeGate": 1_200.0,
    "automatedMergeReadiness": 3_600.0,
}


@dataclass(frozen=True)
class LatencySample:
    candidate_sha: str
    profile: str
    outcome: str
    first_actionable_failure: float
    merge_gate: float
    automated_merge_readiness: float
    certification_required: float = 0
    integrated_certification: float = 0
    cache_mode: str = "not-used"


def _nonnegative(value: float, name: str) -> None:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value < 0
    ):
        raise ContractError(f"{name} must be a non-negative number")


def validate_sample(sample: LatencySample) -> None:
    if SHA1_PATTERN.fullmatch(sample.candidate_sha) is None:
        raise ContractError("latency sample candidate must be a lowercase 40-character SHA")
    if sample.profile not in {"ordinary", "certification-required", "integrated", "release"}:
        raise ContractError("latency sample profile is unsupported")
    if sample.outcome not in {
        "passed",
        "product-failure",
        "infrastructure-failure",
        "indeterminate",
        "stale",
        "cancelled",
    }:
        raise ContractError("latency sample outcome is unsupported")
    if sample.cache_mode not in CACHE_MODES:
        raise ContractError("latency sample cache mode is unsupported")
    for name, value in (
        ("first_actionable_failure", sample.first_actionable_failure),
        ("merge_gate", sample.merge_gate),
        ("automated_merge_readiness", sample.automated_merge_readiness),
        ("certification_required", sample.certification_required),
        ("integrated_certification", sample.integrated_certification),
    ):
        _nonnegative(value, name)


def eligible_ordinary_sample(sample: LatencySample) -> bool:
    validate_sample(sample)
    return sample.profile == "ordinary" and sample.outcome in ORDINARY_ELIGIBLE_OUTCOMES


def _metric(sample: LatencySample, name: str) -> float:
    try:
        return {
            "firstActionableFailure": sample.first_actionable_failure,
            "mergeGate": sample.merge_gate,
            "automatedMergeReadiness": sample.automated_merge_readiness,
            "certificationRequired": sample.certification_required,
            "integratedCertification": sample.integrated_certification,
        }[name]
    except KeyError as error:
        raise ContractError(f"unknown latency metric: {name}") from error


def _percentile(values: tuple[float, ...], percentile: float) -> float:
    if not values or not 0 < percentile <= 100:
        raise ContractError("percentile requires a non-empty sample")
    index = max(0, math.ceil(len(values) * percentile / 100) - 1)
    return sorted(values)[index]


@dataclass(frozen=True)
class SloEvaluation:
    metric: str
    objective_seconds: float
    sample_count: int
    p50_seconds: float | None
    p95_seconds: float | None
    current_breach: bool
    consecutive_breaches: int
    breached: bool


def evaluate_slo(
    samples: Iterable[LatencySample],
    *,
    metric: str,
    objective_seconds: float,
    previous_evaluation_breached: bool = False,
    window_size: int = 50,
    minimum_sample: int = 20,
) -> SloEvaluation:
    if objective_seconds <= 0 or window_size <= 0 or minimum_sample <= 0:
        raise ContractError("SLO objective and sample bounds must be positive")
    eligible = tuple(sample for sample in samples if eligible_ordinary_sample(sample))
    window = eligible[-window_size:]
    if len(window) < minimum_sample:
        return SloEvaluation(
            metric,
            float(objective_seconds),
            len(window),
            None,
            None,
            False,
            0,
            False,
        )
    values = tuple(_metric(sample, metric) for sample in window)
    p50 = _percentile(values, 50)
    p95 = _percentile(values, 95)
    current_breach = p95 > objective_seconds
    consecutive = (
        2
        if current_breach and previous_evaluation_breached
        else (1 if current_breach else 0)
    )
    return SloEvaluation(
        metric,
        float(objective_seconds),
        len(window),
        p50,
        p95,
        current_breach,
        consecutive,
        consecutive >= 2,
    )


def reliability_counts(samples: Iterable[LatencySample]) -> tuple[tuple[str, int], ...]:
    counts: dict[str, int] = {}
    for sample in samples:
        validate_sample(sample)
        counts[sample.outcome] = counts.get(sample.outcome, 0) + 1
    return tuple(sorted(counts.items()))


def render_slo(evaluation: SloEvaluation) -> dict[str, object]:
    return {
        "metric": evaluation.metric,
        "objectiveSeconds": evaluation.objective_seconds,
        "sampleCount": evaluation.sample_count,
        "p50Seconds": evaluation.p50_seconds,
        "p95Seconds": evaluation.p95_seconds,
        "currentBreach": evaluation.current_breach,
        "consecutiveBreaches": evaluation.consecutive_breaches,
        "breached": evaluation.breached,
        "excludedOutcomes": sorted(EXCLUDED_FROM_LATENCY),
        "eligibleOutcomes": sorted(ORDINARY_ELIGIBLE_OUTCOMES),
    }


def retention_class_for(profile: str, stage: str) -> str:
    if stage == "preflight":
        return "intra-run"
    if stage == "integrated":
        return "integrated-certification"
    if stage == "release":
        return "unpublished-release-candidate"
    if stage == "surveillance":
        return "surveillance"
    if stage == "intra-run":
        return "intra-run"
    if profile == "ordinary":
        return "ordinary-pull-request"
    if profile == "certification-required":
        return "certification-required-pull-request"
    raise ContractError(f"cannot resolve retention class for {profile}/{stage}")


@dataclass(frozen=True)
class QuarantinedCheck:
    check_id: str
    source_sha: str
    scope: str
    evidence_ids: tuple[str, ...]
    justification: str
    tracking_reference: str
    started_at: int
    expires_at: int
    continued_stage: str


def validate_quarantine(check: QuarantinedCheck) -> None:
    if not all(
        isinstance(value, str) and value
        for value in (
            check.check_id,
            check.scope,
            check.justification,
            check.tracking_reference,
        )
    ):
        raise ContractError("quarantine requires exact identity, scope, justification, and tracking")
    if not isinstance(check.source_sha, str) or SHA1_PATTERN.fullmatch(check.source_sha) is None:
        raise ContractError("quarantine source must be a lowercase 40-character SHA")
    if not check.evidence_ids or len(set(check.evidence_ids)) != len(check.evidence_ids) or not all(
        isinstance(value, str) and value for value in check.evidence_ids
    ):
        raise ContractError("quarantine requires evidence of intermittent behavior")
    if (
        isinstance(check.started_at, bool)
        or not isinstance(check.started_at, int)
        or isinstance(check.expires_at, bool)
        or not isinstance(check.expires_at, int)
        or check.expires_at <= check.started_at
    ):
        raise ContractError("quarantine expiry must be after its start")
    if check.expires_at - check.started_at > 7 * 86_400:
        raise ContractError("quarantine may last no more than seven days")
    if check.continued_stage not in {"integrated", "surveillance"}:
        raise ContractError("quarantined checks must continue in Integrated or Surveillance")


@dataclass(frozen=True)
class SurveillanceRun:
    run_id: str
    profile: str
    source_sha: str
    started_at: int
    outcome: str
    candidate_evidence: bool = False


def validate_surveillance_run(run: SurveillanceRun) -> None:
    if not isinstance(run.run_id, str) or not run.run_id or not isinstance(run.profile, str) or not run.profile:
        raise ContractError("Surveillance run requires an identity and profile")
    if not isinstance(run.source_sha, str) or SHA1_PATTERN.fullmatch(run.source_sha) is None:
        raise ContractError("Surveillance source must be a lowercase 40-character SHA")
    if isinstance(run.started_at, bool) or not isinstance(run.started_at, int) or run.started_at < 0:
        raise ContractError("Surveillance start time is invalid")
    if run.outcome not in {"passed", "product-failure", "infrastructure-failure", "indeterminate", "stale", "cancelled"}:
        raise ContractError("Surveillance outcome is unsupported")
    if run.candidate_evidence:
        raise ContractError("Surveillance cannot be candidate evidence")


def surveillance_cancellation_allowed(
    previous: SurveillanceRun, newer: SurveillanceRun
) -> bool:
    validate_surveillance_run(previous)
    validate_surveillance_run(newer)
    return newer.profile == previous.profile and newer.started_at > previous.started_at


def detect_drift(
    baseline: Mapping[str, str], current: Mapping[str, str]
) -> tuple[str, ...]:
    """Return bounded drift categories without creating external incident state."""
    categories = {
        "dependencies": "dependencies",
        "toolchains": "toolchains",
        "quarantine": "quarantine",
        "cache": "cache",
        "external_assumptions": "external-assumptions",
    }
    return tuple(
        category
        for key, category in categories.items()
        if baseline.get(key) != current.get(key)
    )
