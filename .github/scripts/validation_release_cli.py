#!/usr/bin/env python3
"""Certify or promote one immutable Product artifact set."""

import argparse
import json
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import parse_manifest
from validation_contracts import serialize_manifest
from validation_release import PublicationRequest
from validation_release import ReleaseArtifact
from validation_release import artifact_from_dict
from validation_release import artifact_set_from_dict
from validation_release import artifact_set_to_dict
from validation_release import certify_artifacts
from validation_release import release_evidence_manifest
from validation_release_files import verify_release_files
from validation_release import verify_promotion


def _read_json(path: Path) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ContractError(f"cannot read release JSON {path}: {error}") from error


def _artifacts(payload: object) -> tuple[ReleaseArtifact, ...]:
    if isinstance(payload, dict) and set(payload) == {"artifacts"}:
        payload = payload["artifacts"]
    if not isinstance(payload, list):
        raise ContractError("release artifacts must be an array")
    return tuple(artifact_from_dict(item) for item in payload)


def certify(args: argparse.Namespace) -> int:
    integrated = parse_manifest(args.integrated_manifest.read_text(encoding="utf-8"))
    if (
        args.source_sha is not None
        and integrated.candidate.candidate_sha != args.source_sha
    ):
        raise ContractError(
            "Integrated manifest is not bound to the requested release SHA"
        )
    artifacts = _artifacts(_read_json(args.artifacts))
    verify_release_files(
        args.artifact_dir,
        artifacts,
        source_sha=integrated.candidate.candidate_sha,
    )
    artifact_set = certify_artifacts(
        integrated,
        artifacts,
        certification_manifest_id=args.certification_manifest_id,
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(artifact_set_to_dict(artifact_set), indent=2) + "\n",
        encoding="utf-8",
    )
    if args.evidence_output is not None:
        evidence = release_evidence_manifest(
            artifact_set,
            candidate=integrated.candidate,
            duration_seconds=args.duration_seconds,
            created_at=args.now,
        )
        args.evidence_output.parent.mkdir(parents=True, exist_ok=True)
        args.evidence_output.write_text(serialize_manifest(evidence), encoding="utf-8")
    return 0


def _request(payload: object) -> PublicationRequest:
    if not isinstance(payload, dict) or set(payload) != {
        "sourceSha",
        "certificationManifestId",
        "artifacts",
        "rebuild",
        "repackage",
        "resign",
        "publicAuthorized",
    }:
        raise ContractError("publication request has invalid fields")
    flags = (
        payload["rebuild"],
        payload["repackage"],
        payload["resign"],
        payload["publicAuthorized"],
    )
    if not all(isinstance(flag, bool) for flag in flags):
        raise ContractError("publication request flags must be boolean")
    return PublicationRequest(
        source_sha=payload["sourceSha"],
        certification_manifest_id=payload["certificationManifestId"],
        artifacts=_artifacts(payload["artifacts"]),
        rebuild=payload["rebuild"],
        repackage=payload["repackage"],
        resign=payload["resign"],
        public_authorized=payload["publicAuthorized"],
    )


def promote(args: argparse.Namespace) -> int:
    artifact_set = artifact_set_from_dict(_read_json(args.certified_set))
    request = _request(_read_json(args.request))
    verify_promotion(artifact_set, request, state=args.state)
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(
                {
                    "sourceSha": artifact_set.source_sha,
                    "certificationManifestId": artifact_set.certification_manifest_id,
                    "promotedArtifactDigests": [
                        [artifact.name, artifact.digest]
                        for artifact in artifact_set.artifacts
                    ],
                    "rebuild": False,
                    "repackage": False,
                    "resign": False,
                    "publicAuthorized": True,
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    return 0


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    subcommands = command.add_subparsers(dest="command", required=True)
    certify_command = subcommands.add_parser("certify")
    certify_command.add_argument("--integrated-manifest", type=Path, required=True)
    certify_command.add_argument("--artifacts", type=Path, required=True)
    certify_command.add_argument("--artifact-dir", type=Path, required=True)
    certify_command.add_argument("--output", type=Path, required=True)
    certify_command.add_argument("--source-sha")
    certify_command.add_argument("--evidence-output", type=Path)
    certify_command.add_argument(
        "--certification-manifest-id", default="release-certification"
    )
    certify_command.add_argument("--duration-seconds", type=float, default=0)
    certify_command.add_argument("--now", type=int, default=0)
    certify_command.set_defaults(handler=certify)
    promote_command = subcommands.add_parser("promote")
    promote_command.add_argument("--certified-set", type=Path, required=True)
    promote_command.add_argument("--request", type=Path, required=True)
    promote_command.add_argument("--state", default="clean")
    promote_command.add_argument("--output", type=Path)
    promote_command.set_defaults(handler=promote)
    return command


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        return args.handler(args)
    except (ContractError, OSError, ValueError) as error:
        print(f"Release validation failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
