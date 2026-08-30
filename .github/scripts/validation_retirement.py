#!/usr/bin/env python3
"""Fail closed until the legacy validation graph's observation contract holds."""

from dataclasses import dataclass

from validation_contracts import ContractError


OBSERVATION_SECONDS = 30 * 86_400
SUPERSEDED_ISSUES = (82, 91, 92, *range(135, 146))


@dataclass(frozen=True)
class RetirementObservation:
    cutover_at: int
    now: int
    eligible_merge_runs: int
    release_certification_passed: bool
    protection_gap: bool
    state_authority_ambiguous: bool
    rollback_required: bool
    legacy_manually_runnable: bool


@dataclass(frozen=True)
class RetirementDecision:
    allowed: bool
    reason: str
    required_backlink_issues: tuple[int, ...]


def validate_retirement(observation: RetirementObservation) -> RetirementDecision:
    if any(
        isinstance(value, bool) or not isinstance(value, int)
        for value in (
            observation.cutover_at,
            observation.now,
            observation.eligible_merge_runs,
        )
    ):
        raise ContractError("retirement numeric inputs are malformed")
    if not all(
        isinstance(value, bool)
        for value in (
            observation.release_certification_passed,
            observation.protection_gap,
            observation.state_authority_ambiguous,
            observation.rollback_required,
            observation.legacy_manually_runnable,
        )
    ):
        raise ContractError("retirement boolean inputs are malformed")
    if observation.cutover_at <= 0 or observation.now < observation.cutover_at:
        raise ContractError("retirement timestamps are malformed")
    failures = []
    if observation.now - observation.cutover_at < OBSERVATION_SECONDS:
        failures.append("the 30-day observation window has not elapsed")
    if observation.eligible_merge_runs < 20:
        failures.append("fewer than 20 eligible replacement Merge gate runs were observed")
    if not observation.release_certification_passed:
        failures.append("no replacement Release certification has passed")
    if observation.protection_gap:
        failures.append("a required-check protection gap remains")
    if observation.state_authority_ambiguous:
        failures.append("Validation state authority remains ambiguous")
    if observation.rollback_required:
        failures.append("rollback is still required")
    if not observation.legacy_manually_runnable:
        failures.append("legacy validation must remain manually runnable before retirement")
    return RetirementDecision(
        allowed=not failures,
        reason="; ".join(failures) if failures else "legacy retirement preconditions passed",
        required_backlink_issues=SUPERSEDED_ISSUES,
    )
