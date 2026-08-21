#!/usr/bin/env python3
"""Validate a bounded Deep Linux workflow conclusion."""

from dataclasses import dataclass
import os
import re


LABEL = re.compile(r"[A-Za-z0-9][A-Za-z0-9 ._-]{0,79}")


@dataclass(frozen=True)
class Decision:
    passed: bool
    message: str


def evaluate(
    eligibility_result: str,
    eligible: str,
    validation_result: str,
    validation_label: str,
) -> Decision:
    if LABEL.fullmatch(validation_label) is None:
        return Decision(False, "validation label is malformed")
    if eligibility_result != "success":
        return Decision(False, f"eligibility job ended with {eligibility_result or 'missing'}")
    if eligible not in {"true", "false"}:
        return Decision(False, "eligibility output is malformed")
    expected = "success" if eligible == "true" else "skipped"
    if validation_result != expected:
        return Decision(
            False,
            f"eligible={eligible} requires {validation_label} {expected}, "
            f"found {validation_result or 'missing'}",
        )
    state = "succeeded" if eligible == "true" else "correctly skipped"
    return Decision(True, f"eligible={eligible}; {validation_label} {state}")


def main() -> int:
    eligibility_result = os.environ.get("ELIGIBILITY_RESULT", "")
    eligible = os.environ.get("ELIGIBLE", "")
    validation_label = os.environ.get("VALIDATION_LABEL", "")
    validation_result = os.environ.get("VALIDATION_RESULT", "")
    decision = evaluate(
        eligibility_result, eligible, validation_result, validation_label
    )
    heading = validation_label if LABEL.fullmatch(validation_label) else "Deep Linux validation"
    print(f"## {heading} result")
    print()
    print(f"- Eligibility job: `{eligibility_result or 'missing'}`")
    print(f"- Eligible output: `{eligible or 'missing'}`")
    print(f"- Validation workflow: `{validation_result or 'missing'}`")
    print(f"- Decision: {decision.message}")
    return 0 if decision.passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
