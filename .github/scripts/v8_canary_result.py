#!/usr/bin/env python3
"""Validate the bounded V8 canary workflow conclusion."""

from dataclasses import dataclass
import os


@dataclass(frozen=True)
class Decision:
    passed: bool
    message: str


def evaluate(
    metadata_result: str,
    canary_required: str,
    canary_reason: str,
    build_result: str,
    windows_build_result: str,
) -> Decision:
    if metadata_result != "success":
        return Decision(False, f"metadata job ended with {metadata_result or 'missing'}")
    if canary_required not in {"true", "false"}:
        return Decision(False, "canary_required output is malformed")
    if (
        not canary_reason
        or canary_reason != canary_reason.strip()
        or not canary_reason.isprintable()
        or len(canary_reason) > 240
    ):
        return Decision(False, "canary reason is malformed")
    if windows_build_result != "success":
        return Decision(
            False,
            "Windows source matrix expected success, found "
            f"{windows_build_result or 'missing'}",
        )
    expected = "success" if canary_required == "true" else "skipped"
    if build_result != expected:
        return Decision(
            False,
            f"canary_required={canary_required} requires build {expected}, "
            f"found {build_result or 'missing'}",
        )
    if canary_required == "true":
        return Decision(True, "Windows V8 and required Linux matrix legs succeeded")
    return Decision(True, "Windows V8 succeeded and Linux matrix correctly skipped")


def main() -> int:
    metadata_result = os.environ.get("METADATA_RESULT", "")
    canary_required = os.environ.get("CANARY_REQUIRED", "")
    canary_reason = os.environ.get("CANARY_REASON", "")
    build_result = os.environ.get("BUILD_RESULT", "")
    windows_build_result = os.environ.get("WINDOWS_BUILD_RESULT", "")
    decision = evaluate(
        metadata_result,
        canary_required,
        canary_reason,
        build_result,
        windows_build_result,
    )
    print("## V8 canary result")
    print()
    print(f"- Metadata job: `{metadata_result or 'missing'}`")
    print(f"- Required: `{canary_required or 'missing'}`")
    print(f"- Build matrix: `{build_result or 'missing'}`")
    print(f"- Windows source matrix: `{windows_build_result or 'missing'}`")
    print(f"- Reason: {canary_reason or 'missing'}")
    print(f"- Decision: {decision.message}")
    return 0 if decision.passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
