#!/usr/bin/env python3
"""Validate one explicit Recovery authorization without mutating Git history."""

import argparse
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_state import RECOVERY
from validation_state import RecoveryAuthorization
from validation_state import consume_recovery_authorization
from validation_state import recovery_admission_allowed
from validation_state import validate_recovery_authorization


def authorization_from_dict(payload: object) -> RecoveryAuthorization:
    if not isinstance(payload, dict) or set(payload) != {
        "authorizationId",
        "failedIntegratedSha",
        "candidateHeadSha",
        "currentBaseSha",
        "actionType",
        "grantor",
        "auditIdentity",
        "pullRequestNumber",
        "validationFingerprint",
        "consumed",
    }:
        raise ContractError("Recovery authorization has invalid fields")
    if not isinstance(payload["consumed"], bool):
        raise ContractError("Recovery authorization consumed must be boolean")
    return RecoveryAuthorization(
        authorization_id=payload["authorizationId"],
        failed_integrated_sha=payload["failedIntegratedSha"],
        candidate_head_sha=payload["candidateHeadSha"],
        current_base_sha=payload["currentBaseSha"],
        action_type=payload["actionType"],
        grantor=payload["grantor"],
        audit_identity=payload["auditIdentity"],
        pull_request_number=payload["pullRequestNumber"],
        validation_fingerprint=payload["validationFingerprint"],
        consumed=payload["consumed"],
    )


def authorize_recovery(
    authorization: RecoveryAuthorization,
    *,
    failed_integrated_sha: str,
    candidate_head_sha: str,
    current_base_sha: str,
    action_type: str,
    pull_request_number: int,
    validation_fingerprint: str,
    merge_gate_passed: bool,
    review_passed: bool,
    consume: bool = False,
) -> RecoveryAuthorization:
    validate_recovery_authorization(
        authorization,
        state=RECOVERY,
        failed_integrated_sha=failed_integrated_sha,
        candidate_head_sha=candidate_head_sha,
        current_base_sha=current_base_sha,
        action_type=action_type,
        pull_request_number=pull_request_number,
        validation_fingerprint=validation_fingerprint,
    )
    if not recovery_admission_allowed(
        RECOVERY,
        authorization,
        merge_gate_passed=merge_gate_passed,
        review_passed=review_passed,
    ):
        raise ContractError(
            "Recovery correction must pass the ordinary reviewed Merge gate"
        )
    if consume:
        return consume_recovery_authorization(
            authorization,
            state=RECOVERY,
            failed_integrated_sha=failed_integrated_sha,
            candidate_head_sha=candidate_head_sha,
            current_base_sha=current_base_sha,
            action_type=action_type,
            pull_request_number=pull_request_number,
            validation_fingerprint=validation_fingerprint,
        )
    return authorization


def run(args: argparse.Namespace) -> int:
    try:
        authorization = authorization_from_dict(
            json.loads(args.authorization.read_text(encoding="utf-8"))
        )
        result = authorize_recovery(
            authorization,
            failed_integrated_sha=args.failed_integrated_sha,
            candidate_head_sha=args.candidate_head_sha,
            current_base_sha=args.current_base_sha,
            action_type=args.action_type,
            pull_request_number=args.pull_request_number,
            validation_fingerprint=args.validation_fingerprint,
            merge_gate_passed=args.merge_gate_passed,
            review_passed=args.review_passed,
            consume=args.consume,
        )
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(
                {
                    "authorizationId": result.authorization_id,
                    "failedIntegratedSha": result.failed_integrated_sha,
                    "candidateHeadSha": result.candidate_head_sha,
                    "currentBaseSha": result.current_base_sha,
                    "actionType": result.action_type,
                    "grantor": result.grantor,
                    "auditIdentity": result.audit_identity,
                    "pullRequestNumber": result.pull_request_number,
                    "validationFingerprint": result.validation_fingerprint,
                    "consumed": result.consumed,
                    "automaticMutation": False,
                    "nextAction": "review and merge the correction through normal gates",
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        return 0
    except (ContractError, OSError, json.JSONDecodeError, ValueError) as error:
        print(f"Recovery authorization rejected: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--authorization", type=Path, required=True)
    command.add_argument("--failed-integrated-sha", required=True)
    command.add_argument("--candidate-head-sha", required=True)
    command.add_argument("--current-base-sha", required=True)
    command.add_argument("--action-type", choices=("correction", "revert"), required=True)
    command.add_argument("--pull-request-number", type=int, required=True)
    command.add_argument("--validation-fingerprint", required=True)
    command.add_argument("--merge-gate-passed", action="store_true")
    command.add_argument("--review-passed", action="store_true")
    command.add_argument("--consume", action="store_true")
    command.add_argument("--output", type=Path, required=True)
    return command

def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
