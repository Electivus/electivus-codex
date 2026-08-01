#!/usr/bin/env python3
"""Validate the bounded Deep Linux Cargo workflow conclusion."""

from dataclasses import dataclass
import os


@dataclass(frozen=True)
class Decision:
    passed: bool
    message: str


def evaluate(eligibility_result: str, eligible: str, cargo_result: str) -> Decision:
    if eligibility_result != "success":
        return Decision(False, f"eligibility job ended with {eligibility_result or 'missing'}")
    if eligible not in {"true", "false"}:
        return Decision(False, "eligibility output is malformed")
    expected = "success" if eligible == "true" else "skipped"
    if cargo_result != expected:
        return Decision(
            False,
            f"eligible={eligible} requires Cargo {expected}, "
            f"found {cargo_result or 'missing'}",
        )
    state = "succeeded" if eligible == "true" else "correctly skipped"
    return Decision(True, f"eligible={eligible}; Deep Linux Cargo {state}")


def main() -> int:
    eligibility_result = os.environ.get("ELIGIBILITY_RESULT", "")
    eligible = os.environ.get("ELIGIBLE", "")
    cargo_result = os.environ.get("CARGO_RESULT", "")
    decision = evaluate(eligibility_result, eligible, cargo_result)
    print("## Deep Linux Cargo result")
    print()
    print(f"- Eligibility job: `{eligibility_result or 'missing'}`")
    print(f"- Eligible output: `{eligible or 'missing'}`")
    print(f"- Cargo workflow: `{cargo_result or 'missing'}`")
    print(f"- Decision: {decision.message}")
    return 0 if decision.passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
