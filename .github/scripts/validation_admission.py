#!/usr/bin/env python3
"""Check ordinary PR admission against the exact current-main certification."""

from dataclasses import dataclass
import argparse
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import EvidenceManifest
from validation_contracts import parse_manifest
from validation_state import CLEAN
from validation_state import certification_lock
from validation_state import derive_state


@dataclass(frozen=True)
class AdmissionDecision:
    allowed: bool
    current_main_sha: str
    state: str
    certification_lock_active: bool
    reason: str


def evaluate_admission(
    current_main_sha: str, manifest: EvidenceManifest | None
) -> AdmissionDecision:
    """Allow a PR only while the exact current main commit is certified.

    Admission is serialized by the caller's workflow concurrency group and
    refreshes the current main SHA immediately before this check. Treating
    the current main commit as the sole admission unit makes a missing or
    non-passing Integrated manifest an active Certification lock.
    """
    certified = (
        manifest is not None
        and manifest.family == "integrated-certification"
        and manifest.candidate.candidate_sha == current_main_sha
        and manifest.stage == "integrated"
        and manifest.disposition == "required"
        and manifest.outcome == "passed"
    )
    lock = certification_lock(
        (current_main_sha,),
        (current_main_sha,) if certified else (),
    )
    state = derive_state(
        current_main_sha,
        manifest,
        certification_lock=lock.active,
    )
    allowed = (
        lock.ordinary_admission_allowed and state.state == CLEAN and state.authoritative
    )
    reason = (
        "exact current main Integrated certification permits ordinary admission"
        if allowed
        else f"ordinary admission is blocked: {state.reason}; {lock.reason}"
    )
    return AdmissionDecision(
        allowed=allowed,
        current_main_sha=current_main_sha,
        state=state.state,
        certification_lock_active=lock.active,
        reason=reason,
    )


def run(args: argparse.Namespace) -> int:
    try:
        manifest = parse_manifest(args.manifest.read_text(encoding="utf-8"))
        decision = evaluate_admission(args.current_main_sha, manifest)
        payload = {
            "allowed": decision.allowed,
            "currentMainSha": decision.current_main_sha,
            "state": decision.state,
            "certificationLockActive": decision.certification_lock_active,
            "reason": decision.reason,
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print("## Validation admission")
        print()
        print(f"- Allowed: {str(decision.allowed).lower()}")
        print(f"- Exact current main SHA: {decision.current_main_sha}")
        print(f"- State: {decision.state}")
        print(
            "- Certification lock active: "
            f"{str(decision.certification_lock_active).lower()}"
        )
        print(f"- Reason: {decision.reason}")
        return 0 if decision.allowed else 1
    except (ContractError, OSError, ValueError) as error:
        print(f"Validation admission check failed: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--manifest", type=Path, required=True)
    command.add_argument("--current-main-sha", required=True)
    command.add_argument("--output", type=Path, required=True)
    return command


def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
