#!/usr/bin/env python3
"""Verify the immutable bytes consumed by Release certification."""

import hashlib
import json
from pathlib import Path
from pathlib import PurePosixPath
import stat
import tarfile
from typing import Iterable
import zipfile

from validation_contracts import ContractError
from validation_release import ReleaseArtifact
from validation_release import validate_artifact


def _file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise ContractError(f"cannot read release file {path}: {error}") from error
    return digest.hexdigest()


def _archive_signature_digest(path: Path, artifact: ReleaseArtifact) -> str | None:
    if artifact.packaging == "tar.gz":
        try:
            with tarfile.open(path, mode="r:gz") as archive:
                members = archive.getmembers()
                if {member.name for member in members} != {"codex", "codex.sigstore"}:
                    raise ContractError(
                        f"Linux release archive has an unexpected file set: {path.name}"
                    )
                signature = next(
                    member for member in members if member.name == "codex.sigstore"
                )
                if not signature.isfile():
                    raise ContractError("Linux release signature is not a regular file")
                stream = archive.extractfile(signature)
                if stream is None:
                    raise ContractError("Linux release signature cannot be read")
                digest = hashlib.sha256()
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(chunk)
                return digest.hexdigest()
        except (OSError, tarfile.TarError) as error:
            raise ContractError(
                f"cannot verify Linux release archive {path}: {error}"
            ) from error

    try:
        with zipfile.ZipFile(path) as archive:
            if archive.testzip() is not None:
                raise ContractError(
                    f"Windows release archive has a corrupt member: {path.name}"
                )
            members = [member for member in archive.infolist() if not member.is_dir()]
            for member in members:
                path_parts = PurePosixPath(member.filename)
                if path_parts.is_absolute() or ".." in path_parts.parts:
                    raise ContractError(
                        "Windows release archive contains an unsafe path"
                    )
                mode = (member.external_attr >> 16) & 0o170000
                if mode == stat.S_IFLNK:
                    raise ContractError(
                        "Windows release archive contains a symbolic link"
                    )
            binaries = [
                member
                for member in members
                if PurePosixPath(member.filename).name == "codex.exe"
            ]
            if len(members) != 1 or len(binaries) != 1:
                raise ContractError(
                    f"Windows release archive must contain exactly one codex.exe: {path.name}"
                )
            return None
    except (OSError, zipfile.BadZipFile) as error:
        raise ContractError(
            f"cannot verify Windows release archive {path}: {error}"
        ) from error


def _verify_provenance(
    path: Path,
    artifact: ReleaseArtifact,
    *,
    source_sha: str | None,
) -> None:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ContractError(
            f"cannot read release provenance {path}: {error}"
        ) from error
    expected_fields = {
        "sourceSha",
        "archive",
        "archiveSha256",
        "signatureSha256",
        "builder",
        "command",
        "provenance",
        "signatureBoundary",
    }
    if not isinstance(payload, dict) or set(payload) != expected_fields:
        raise ContractError(f"release provenance has invalid fields: {path.name}")
    if source_sha is not None and payload["sourceSha"] != source_sha:
        raise ContractError(f"release provenance source differs: {path.name}")
    expected_signature = artifact.signature_digest or "not-applicable"
    if payload["archive"] != artifact.name:
        raise ContractError(f"release provenance archive differs: {path.name}")
    if payload["archiveSha256"] != artifact.digest:
        raise ContractError(f"release provenance archive digest differs: {path.name}")
    if payload["signatureSha256"] != expected_signature:
        raise ContractError(f"release provenance signature differs: {path.name}")
    for name in (
        "sourceSha",
        "archive",
        "archiveSha256",
        "signatureSha256",
        "builder",
        "command",
        "provenance",
        "signatureBoundary",
    ):
        if not isinstance(payload[name], str) or not payload[name]:
            raise ContractError(f"release provenance field is empty: {name}")


def verify_release_files(
    directory: Path,
    artifacts: Iterable[ReleaseArtifact],
    *,
    source_sha: str | None = None,
) -> tuple[ReleaseArtifact, ...]:
    """Verify downloaded release bytes, provenance, and embedded signatures."""
    artifacts = tuple(artifacts)
    if not directory.is_dir():
        raise ContractError(f"release artifact directory is missing: {directory}")
    names: set[str] = set()
    platforms: set[str] = set()
    for artifact in artifacts:
        validate_artifact(artifact)
        if artifact.name in names:
            raise ContractError(
                f"duplicate downloaded release artifact: {artifact.name}"
            )
        if artifact.platform in platforms:
            raise ContractError(
                f"duplicate downloaded release platform: {artifact.platform}"
            )
        names.add(artifact.name)
        platforms.add(artifact.platform)
        archive = directory / artifact.name
        if not archive.is_file():
            raise ContractError(
                f"downloaded release archive is missing: {artifact.name}"
            )
        if _file_digest(archive) != artifact.digest:
            raise ContractError(
                f"downloaded release archive digest differs: {artifact.name}"
            )
        provenance = directory / f"provenance-{artifact.platform}.json"
        if not provenance.is_file():
            raise ContractError(
                f"downloaded release provenance is missing: {provenance.name}"
            )
        if _file_digest(provenance) != artifact.provenance_digest:
            raise ContractError(
                f"downloaded release provenance digest differs: {provenance.name}"
            )
        _verify_provenance(provenance, artifact, source_sha=source_sha)
        signature_digest = _archive_signature_digest(archive, artifact)
        if signature_digest != artifact.signature_digest:
            raise ContractError(
                f"downloaded release signature digest differs: {artifact.name}"
            )
    return artifacts
