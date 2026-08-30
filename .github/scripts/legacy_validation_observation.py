#!/usr/bin/env python3
"""Adapt the legacy CI aggregate into a non-authoritative comparison snapshot."""

import argparse
import json
from pathlib import Path

from validation_comparison import COMPARISON_SCHEMA_VERSION
from validation_contracts import ContractError
from validation_contracts import parse_plan


LEGACY_FAMILY_JOBS = {
    "repository-hygiene": ("repo-checks",),
    "rust-fast": ("rust-ci",),
    "linux-x64-bazel": ("bazel",),
    "api-protocol-sdk": ("sdk",),
    "postgresql": (),
    "v8": ("v8-canary",),
    "windows-x64": (),
    "codeql-advanced": (),
    "code-quality": ("rust-ci",),
    "linux-x64-cargo": ("deep-linux-cargo",),
    "linux-arm64": ("deep-linux-cargo",),
    "linux-musl": ("deep-linux-cargo",),
    "release-packaging": (),
    "synchronization-topology": ("repo-checks",),
}


def _outcome(needs: dict[str, object], jobs: tuple[str, ...]) -> str:
    if not jobs:
        return "indeterminate"
    results = []
    for job in jobs:
        value = needs.get(job)
        result = value.get("result") if isinstance(value, dict) else None
        if result not in {"success", "failure", "cancelled", "skipped"}:
            return "indeterminate"
        results.append(result)
    if "failure" in results:
        return "product-failure"
    if "cancelled" in results:
        return "stale"
    if "skipped" in results:
        return "indeterminate"
    return "passed"


def observation(plan, needs: dict[str, object]) -> dict[str, object]:
    evidence = {}
    for requirement in plan.requirements:
        outcome = (
            "not-required"
            if not requirement.selected
            else _outcome(needs, LEGACY_FAMILY_JOBS[requirement.family])
        )
        evidence[requirement.family] = {
            "disposition": requirement.disposition,
            "outcome": outcome,
        }
    child_results = tuple(
        value.get("result")
        for value in needs.values()
        if isinstance(value, dict)
    )
    if "failure" in child_results:
        aggregate_outcome = "product-failure"
    elif "cancelled" in child_results:
        aggregate_outcome = "stale"
    elif all(result in {"success", "skipped"} for result in child_results):
        aggregate_outcome = "passed"
    else:
        aggregate_outcome = "indeterminate"
    identity = plan.candidate
    return {
        "schemaVersion": COMPARISON_SCHEMA_VERSION,
        "candidateSha": identity.candidate_sha,
        "baseSha": identity.base_sha or "",
        "headSha": identity.head_sha or "",
        "planFingerprint": plan.fingerprint.digest,
        "evidence": evidence,
        "outcome": aggregate_outcome,
        "checkName": "CI required",
        "durations": {
            "firstActionableFailure": 0,
            "mergeGate": 0,
            "automatedMergeReadiness": 0,
            "certificationRequired": 0,
            "integratedCertification": 0,
        },
    }


def run(args: argparse.Namespace) -> int:
    try:
        plan = parse_plan(args.plan.read_text(encoding="utf-8"))
        needs = json.loads(args.needs.read_text(encoding="utf-8"))
        if not isinstance(needs, dict):
            raise ContractError("legacy needs must be an object")
        payload = observation(plan, needs)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return 0
    except (ContractError, OSError, json.JSONDecodeError, ValueError) as error:
        print(f"Legacy validation observation failed: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--plan", type=Path, required=True)
    command.add_argument("--needs", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    return command


def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
