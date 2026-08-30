#!/usr/bin/env python3
"""Render state authority for the exact current default-branch commit."""

import argparse
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import parse_manifest
from validation_state import derive_state


def run(args: argparse.Namespace) -> int:
    try:
        manifest = None
        if args.manifest is not None and args.manifest.is_file():
            manifest = parse_manifest(args.manifest.read_text(encoding="utf-8"))
        elif not args.allow_missing:
            raise ContractError("Integrated state manifest is missing")
        decision = derive_state(
            args.current_main_sha,
            manifest,
            certification_lock=args.certification_lock,
        )
        payload = {
            "state": decision.state,
            "authoritative": decision.authoritative,
            "currentMainSha": decision.commit_sha,
            "reason": decision.reason,
            "allowedActions": list(decision.allowed_actions),
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print("## Validation state")
        print()
        print(f"- State: `{decision.state}`")
        print(f"- Exact main SHA: `{decision.commit_sha}`")
        print(f"- Authoritative: `{str(decision.authoritative).lower()}`")
        print(f"- Reason: {decision.reason}")
        return 0
    except (ContractError, OSError, ValueError) as error:
        print(f"Validation state derivation failed: {error}")
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--manifest", type=Path)
    command.add_argument("--current-main-sha", required=True)
    command.add_argument("--output", type=Path, required=True)
    command.add_argument("--certification-lock", action="store_true")
    command.add_argument(
        "--allow-missing",
        action="store_true",
        help="derive Degraded state when no exact Integrated manifest exists",
    )
    return command


def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
