#!/usr/bin/env python3
"""Evidence-derived Integrated state, lock, and narrow Recovery authorization."""

from dataclasses import dataclass, replace
from typing import Iterable

from validation_contracts import EvidenceManifest
from validation_contracts import ContractError
from validation_contracts import SHA1_PATTERN
from validation_contracts import SHA256_PATTERN


CLEAN = "clean"
CERTIFICATION_LOCK = "certification-lock"
RECOVERY = "recovery"
DEGRADED = "degraded"
STATES = frozenset({CLEAN, CERTIFICATION_LOCK, RECOVERY, DEGRADED})
CONCLUSIVE_INTEGRATED_OUTCOMES = frozenset(
    {"passed", "product-failure", "infrastructure-failure", "indeterminate"}
)


@dataclass(frozen=True)
class StateDecision:
    state: str
    authoritative: bool
    commit_sha: str
    reason: str
    allowed_actions: tuple[str, ...]


def _sha(value: str, name: str) -> None:
    if not isinstance(value, str) or SHA1_PATTERN.fullmatch(value) is None:
        raise ContractError(f"{name} must be a lowercase 40-character SHA")


def derive_state(
    current_main_sha: str,
    manifest: EvidenceManifest | None,
    *,
    certification_lock: bool = False,
) -> StateDecision:
    """Derive state only from the manifest for the exact current main SHA."""
    _sha(current_main_sha, "current_main_sha")
    if manifest is None:
        return StateDecision(
            state=DEGRADED,
            authoritative=False,
            commit_sha=current_main_sha,
            reason="no Integrated Evidence manifest exists for the current main SHA",
            allowed_actions=("run exact Integrated certification",),
        )
    if (
        manifest.candidate.candidate_sha != current_main_sha
        or manifest.stage != "integrated"
        or manifest.disposition != "required"
    ):
        return StateDecision(
            state=DEGRADED,
            authoritative=False,
            commit_sha=current_main_sha,
            reason="Integrated evidence is not bound to the exact current main SHA",
            allowed_actions=("discard stale evidence", "run exact Integrated certification"),
        )
    if manifest.outcome == "passed":
        state = CERTIFICATION_LOCK if certification_lock else CLEAN
        reason = (
            "exact Integrated certification passed while the Certification lock is active"
            if certification_lock
            else "exact Integrated certification passed"
        )
        actions = (
            "complete the bounded Certification lock",
            "allow ordinary admission after lock clearance",
        ) if certification_lock else ("allow ordinary admission", "allow Release certification")
        return StateDecision(state, True, current_main_sha, reason, actions)
    if manifest.outcome == "product-failure":
        return StateDecision(
            RECOVERY,
            True,
            current_main_sha,
            "authoritative Integrated certification found a deterministic product failure",
            ("review an authorized correction or explicit revert",),
        )
    if manifest.outcome in {"infrastructure-failure", "indeterminate", "stale"}:
        return StateDecision(
            DEGRADED,
            manifest.outcome != "stale",
            current_main_sha,
            f"authoritative Integrated outcome is {manifest.outcome}",
            ("repair validation infrastructure and recertify the exact main SHA",),
        )
    return StateDecision(
        DEGRADED,
        False,
        current_main_sha,
        f"unsupported Integrated outcome is {manifest.outcome}",
        ("reject the manifest and run exact Integrated certification",),
    )


@dataclass(frozen=True)
class CertificationLockDecision:
    active: bool
    uncertified_sha: str | None
    ordinary_admission_allowed: bool
    reason: str


def certification_lock(
    integrated_commits: Iterable[str], certified_commits: Iterable[str]
) -> CertificationLockDecision:
    commits = tuple(integrated_commits)
    certified = set(certified_commits)
    if len(set(commits)) != len(commits):
        raise ContractError("Integrated history contains duplicate commits")
    for commit in commits:
        _sha(commit, "integrated commit")
    for commit in certified:
        _sha(commit, "certified commit")
    if not certified <= set(commits):
        raise ContractError("certified commits must be present in the Integrated history")
    uncertified = tuple(commit for commit in commits if commit not in certified)
    if len(uncertified) > 1:
        raise ContractError(
            "at most one uncertified Integrated change may exist on main"
        )
    if uncertified:
        return CertificationLockDecision(
            active=True,
            uncertified_sha=uncertified[0],
            ordinary_admission_allowed=False,
            reason="one exact Integrated change is awaiting conclusive certification",
        )
    return CertificationLockDecision(
        active=False,
        uncertified_sha=None,
        ordinary_admission_allowed=True,
        reason="no uncertified Integrated change exists",
    )


@dataclass(frozen=True)
class IntegratedAttempt:
    commit_sha: str
    outcome: str | None
    attempt: int
    cancelled: bool = False


def validate_integrated_attempt(attempt: IntegratedAttempt) -> None:
    _sha(attempt.commit_sha, "Integrated attempt commit")
    if attempt.outcome is not None and attempt.outcome not in CONCLUSIVE_INTEGRATED_OUTCOMES:
        raise ContractError("Integrated attempt has an unsupported outcome")
    if isinstance(attempt.attempt, bool) or not isinstance(attempt.attempt, int) or attempt.attempt < 1:
        raise ContractError("Integrated attempt number must be positive")
    if attempt.attempt > 2:
        raise ContractError("Integrated certification permits at most one retry")
    if not isinstance(attempt.cancelled, bool):
        raise ContractError("Integrated cancellation must be boolean")


def validate_integrated_attempts(attempts: Iterable[IntegratedAttempt]) -> None:
    records = tuple(attempts)
    for record in records:
        validate_integrated_attempt(record)
    by_commit: dict[str, list[IntegratedAttempt]] = {}
    for record in records:
        by_commit.setdefault(record.commit_sha, []).append(record)
    for commit, records in by_commit.items():
        records_for_commit = sorted(records, key=lambda record: record.attempt)
        if len(records_for_commit) > 2:
            raise ContractError(f"Integrated retry bound exceeded for {commit}")
        if any(record.cancelled for record in records_for_commit):
            raise ContractError("Integrated certification cannot be auto-cancelled")
        if {record.attempt for record in records_for_commit} != set(
            range(1, len(records_for_commit) + 1)
        ):
            raise ContractError("Integrated attempts must start at one and be contiguous")
        if len(records_for_commit) == 2 and records_for_commit[0].outcome == "passed":
            raise ContractError("a passed Integrated certification cannot be retried")


@dataclass(frozen=True)
class RecoveryAuthorization:
    authorization_id: str
    failed_integrated_sha: str
    candidate_head_sha: str
    current_base_sha: str
    action_type: str
    grantor: str
    audit_identity: str
    pull_request_number: int
    validation_fingerprint: str
    consumed: bool = False


def validate_recovery_authorization(
    authorization: RecoveryAuthorization,
    *,
    state: str,
    failed_integrated_sha: str,
    candidate_head_sha: str,
    current_base_sha: str,
    action_type: str,
    pull_request_number: int,
    validation_fingerprint: str,
) -> None:
    if state != RECOVERY:
        raise ContractError("Recovery authorization is valid only in Recovery state")
    for value, name in (
        (authorization.failed_integrated_sha, "authorization.failed_integrated_sha"),
        (authorization.candidate_head_sha, "authorization.candidate_head_sha"),
        (authorization.current_base_sha, "authorization.current_base_sha"),
        (failed_integrated_sha, "failed_integrated_sha"),
        (candidate_head_sha, "candidate_head_sha"),
        (current_base_sha, "current_base_sha"),
    ):
        _sha(value, name)
    if (
        isinstance(pull_request_number, bool)
        or not isinstance(pull_request_number, int)
        or pull_request_number <= 0
    ):
        raise ContractError("Recovery authorization pull request must be positive")
    if (
        not isinstance(validation_fingerprint, str)
        or SHA256_PATTERN.fullmatch(validation_fingerprint) is None
    ):
        raise ContractError("Recovery authorization fingerprint must be a SHA-256")
    if not all(
        isinstance(value, str) and value
        for value in (
            authorization.authorization_id,
            authorization.grantor,
            authorization.audit_identity,
        )
    ):
        raise ContractError("Recovery authorization requires auditable identity fields")
    if authorization.action_type not in {"correction", "revert"}:
        raise ContractError("Recovery authorization action type is unsupported")
    if action_type != authorization.action_type:
        raise ContractError("Recovery authorization action type changed")
    if authorization.failed_integrated_sha != failed_integrated_sha:
        raise ContractError("Recovery authorization failed commit changed")
    if authorization.candidate_head_sha != candidate_head_sha:
        raise ContractError("Recovery authorization candidate changed")
    if authorization.current_base_sha != current_base_sha:
        raise ContractError("Recovery authorization base changed")
    if authorization.pull_request_number != pull_request_number:
        raise ContractError("Recovery authorization pull request changed")
    if authorization.validation_fingerprint != validation_fingerprint:
        raise ContractError("Recovery authorization Validation fingerprint changed")
    if authorization.consumed:
        raise ContractError("Recovery authorization was already consumed")


def consume_recovery_authorization(
    authorization: RecoveryAuthorization,
    *,
    state: str,
    failed_integrated_sha: str,
    candidate_head_sha: str,
    current_base_sha: str,
    action_type: str,
    pull_request_number: int,
    validation_fingerprint: str,
) -> RecoveryAuthorization:
    validate_recovery_authorization(
        authorization,
        state=state,
        failed_integrated_sha=failed_integrated_sha,
        candidate_head_sha=candidate_head_sha,
        current_base_sha=current_base_sha,
        action_type=action_type,
        pull_request_number=pull_request_number,
        validation_fingerprint=validation_fingerprint,
    )
    return replace(authorization, consumed=True)


def recovery_admission_allowed(
    state: str,
    authorization: RecoveryAuthorization | None,
    *,
    merge_gate_passed: bool,
    review_passed: bool,
) -> bool:
    """Recovery remains a normal reviewed Merge gate, never a bypass."""
    if state != RECOVERY or authorization is None:
        return False
    return (
        merge_gate_passed is True
        and review_passed is True
        and not authorization.consumed
    )


def reject_automatic_recovery(action: str) -> None:
    if action in {"automatic-revert", "automatic-history-mutation", "standing-bypass"} or action.startswith("automatic-"):
        raise ContractError(f"Recovery action is not permitted: {action}")
