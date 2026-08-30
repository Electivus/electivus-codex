#!/usr/bin/env python3
"""Preflight the one atomic transfer of validation authority."""

from dataclasses import dataclass

from validation_contracts import ContractError
from validation_contracts import SHA1_PATTERN


@dataclass(frozen=True)
class CutoverAuthorization:
    branch: str
    current_candidate_sha: str
    legacy_check: str
    replacement_check: str
    default_codeql_authoritative: bool
    advanced_codeql_ready: bool
    code_quality_authoritative: bool
    stability_passed: bool
    fresh_authorization: bool
    authorization_id: str
    authorized_by: str


@dataclass(frozen=True)
class CutoverDecision:
    allowed: bool
    reason: str
    atomic_operations: tuple[str, ...]


def validate_cutover(authorization: CutoverAuthorization) -> CutoverDecision:
    failures = []
    if authorization.branch != "main":
        failures.append("cutover must target the default branch")
    if SHA1_PATTERN.fullmatch(authorization.current_candidate_sha) is None:
        failures.append("cutover candidate identity is malformed")
    if authorization.legacy_check != "CI required":
        failures.append("the established required check identity must be CI required")
    if authorization.replacement_check != "CI required":
        failures.append("the replacement must receive the exact CI required identity")
    if not authorization.default_codeql_authoritative:
        failures.append("the legacy CodeQL authority is not currently verified")
    if not authorization.advanced_codeql_ready:
        failures.append("advanced CodeQL has not completed no-gap readiness")
    if not authorization.code_quality_authoritative:
        failures.append("independent code-quality authority is not verified")
    if not authorization.stability_passed:
        failures.append("Stability certification has not passed")
    if not authorization.fresh_authorization or not authorization.authorization_id or not authorization.authorized_by:
        failures.append("fresh auditable cutover authorization is required")
    return CutoverDecision(
        allowed=not failures,
        reason="; ".join(failures) if failures else "atomic validation authority cutover is authorized",
        atomic_operations=(
            "disable legacy aggregate",
            "activate replacement aggregate as CI required",
            "activate authoritative advanced CodeQL",
            "preserve independent code-quality gate",
        ),
    )


def require_no_gap(old_authoritative: bool, new_authoritative: bool) -> None:
    if not old_authoritative and not new_authoritative:
        raise ContractError("validation cutover would create a protection gap")
