#!/usr/bin/env python3
"""Validate the finite Stability certification contract before cutover."""

from dataclasses import dataclass
import re
from typing import Iterable

from validation_contracts import ContractError
from validation_observability import CACHE_MODES
from validation_observability import LatencySample
from validation_observability import evaluate_slo


SHA1 = re.compile(r"^[0-9a-f]{40}$")


@dataclass(frozen=True)
class StabilityRecord:
    candidate_sha: str
    profile: str
    outcome: str
    retry_count: int
    cache_mode: str
    integrated_sha: str | None = None


@dataclass(frozen=True)
class StabilityDecision:
    passed: bool
    reason: str
    ordinary_slo_passed: bool
    automated_readiness_slo_passed: bool


def _record(record: StabilityRecord) -> None:
    if not isinstance(record.candidate_sha, str) or SHA1.fullmatch(record.candidate_sha) is None:
        raise ContractError("Stability candidate identity is malformed")
    if record.profile not in {"ordinary", "certification-required", "integrated"}:
        raise ContractError("Stability profile is unsupported")
    if record.outcome not in {"passed", "product-failure", "infrastructure-failure", "indeterminate"}:
        raise ContractError("Stability outcome is unsupported")
    if isinstance(record.retry_count, bool) or not isinstance(record.retry_count, int) or record.retry_count < 0:
        raise ContractError("Stability retry count cannot be negative")
    if record.retry_count > 1:
        raise ContractError("Stability permits at most one retry")
    if record.cache_mode not in CACHE_MODES:
        raise ContractError("Stability cache mode is unsupported")
    if record.integrated_sha is not None and (
        not isinstance(record.integrated_sha, str)
        or SHA1.fullmatch(record.integrated_sha) is None
    ):
        raise ContractError("Stability Integrated identity is malformed")
    if record.profile == "integrated" and record.integrated_sha is None:
        raise ContractError("Integrated Stability record requires its resulting main SHA")


def validate_stability(
    records: Iterable[StabilityRecord],
    *,
    resulting_main_sha: str,
    ordinary_samples: Iterable[LatencySample],
    objective_merge_gate: float = 1_200,
    objective_automated_readiness: float = 3_600,
) -> StabilityDecision:
    if not isinstance(resulting_main_sha, str) or SHA1.fullmatch(resulting_main_sha) is None:
        raise ContractError("Stability resulting main identity is malformed")
    records = tuple(records)
    for record in records:
        _record(record)
    ordinary = tuple(
        record
        for record in records
        if record.profile == "ordinary" and record.cache_mode != "disabled-reconstruction"
    )
    certification = tuple(
        record for record in records if record.profile == "certification-required"
    )
    cache_disabled = tuple(
        record
        for record in records
        if record.profile == "ordinary"
        and record.cache_mode == "disabled-reconstruction"
    )
    integrated = tuple(record for record in records if record.profile == "integrated")
    failures = []
    if len(ordinary) != 1 or ordinary[0].outcome != "passed" or ordinary[0].retry_count != 0:
        failures.append("one retry-free ordinary candidate is required")
    if len(certification) != 1 or certification[0].outcome != "passed":
        failures.append("one passed Certification-required candidate is required")
    if len(cache_disabled) != 1 or cache_disabled[0].outcome != "passed":
        failures.append("one passed cache-disabled candidate is required")
    if (
        len(integrated) != 1
        or integrated[0].outcome != "passed"
        or integrated[0].candidate_sha != resulting_main_sha
        or integrated[0].integrated_sha != resulting_main_sha
    ):
        failures.append("Integrated certification must pass for the exact resulting main SHA")
    merge_slo = evaluate_slo(
        ordinary_samples,
        metric="mergeGate",
        objective_seconds=objective_merge_gate,
    )
    readiness_slo = evaluate_slo(
        ordinary_samples,
        metric="automatedMergeReadiness",
        objective_seconds=objective_automated_readiness,
    )
    ordinary_slo_passed = (
        merge_slo.sample_count >= 20
        and not merge_slo.breached
    )
    automated_readiness_slo_passed = (
        readiness_slo.sample_count >= 20 and not readiness_slo.breached
    )
    if not ordinary_slo_passed or not automated_readiness_slo_passed:
        failures.append("ordinary candidate samples do not satisfy both latency objectives")
    return StabilityDecision(
        passed=not failures,
        reason="; ".join(failures) if failures else "Stability certification contract passed",
        ordinary_slo_passed=ordinary_slo_passed,
        automated_readiness_slo_passed=automated_readiness_slo_passed,
    )
