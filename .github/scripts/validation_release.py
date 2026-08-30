#!/usr/bin/env python3
"""Immutable Release certification and promotion contracts."""

from dataclasses import dataclass
from typing import Iterable

from validation_contracts import ContractError
from validation_contracts import EvidenceManifest
from validation_contracts import ValidationFingerprint
from validation_contracts import SHA1_PATTERN
from validation_contracts import SHA256_PATTERN
from validation_contracts import SCHEMA_VERSION
from validation_contracts import fingerprint_from_dict
from validation_contracts import fingerprint_to_dict
from validation_contracts import validate_manifest
from validation_contracts import validate_candidate
from validation_contracts import validate_fingerprint


PRODUCT_PLATFORMS = frozenset(
    {"linux-x64", "linux-arm64", "windows-x64", "windows-arm64"}
)
RELEASE_RETENTION = "unpublished-release-candidate"
PUBLISHED_RETENTION = "published-release"


@dataclass(frozen=True)
class ReleaseArtifact:
    name: str
    digest: str
    platform: str
    packaging: str
    producer: str
    provenance_digest: str
    signature_digest: str | None = None


def artifact_to_dict(artifact: ReleaseArtifact) -> dict[str, str]:
    validate_artifact(artifact)
    return {
        "name": artifact.name,
        "digest": artifact.digest,
        "platform": artifact.platform,
        "packaging": artifact.packaging,
        "producer": artifact.producer,
        "provenanceDigest": artifact.provenance_digest,
        "signatureDigest": artifact.signature_digest,
    }


def artifact_from_dict(payload: object) -> ReleaseArtifact:
    if not isinstance(payload, dict) or set(payload) != {
        "name",
        "digest",
        "platform",
        "packaging",
        "producer",
        "provenanceDigest",
        "signatureDigest",
    }:
        raise ContractError("release artifact has invalid fields")
    artifact = ReleaseArtifact(
        name=payload["name"],
        digest=payload["digest"],
        platform=payload["platform"],
        packaging=payload["packaging"],
        producer=payload["producer"],
        provenance_digest=payload["provenanceDigest"],
        signature_digest=payload["signatureDigest"],
    )
    validate_artifact(artifact)
    return artifact


def validate_artifact(artifact: ReleaseArtifact) -> None:
    if not all(
        isinstance(value, str)
        for value in (
            artifact.name,
            artifact.digest,
            artifact.platform,
            artifact.packaging,
            artifact.producer,
            artifact.provenance_digest,
        )
    ) or (artifact.signature_digest is not None and not isinstance(artifact.signature_digest, str)):
        raise ContractError("release artifact fields must be strings")
    if (
        not artifact.name
        or not artifact.producer
        or not artifact.packaging
        or "/" in artifact.name
        or "\\" in artifact.name
    ):
        raise ContractError("release artifact requires name, packaging, and producer")
    if artifact.platform not in PRODUCT_PLATFORMS:
        raise ContractError(f"unsupported Product platform: {artifact.platform}")
    if SHA256_PATTERN.fullmatch(artifact.digest) is None:
        raise ContractError("release artifact digest must be a lowercase SHA-256")
    if SHA256_PATTERN.fullmatch(artifact.provenance_digest) is None:
        raise ContractError("release provenance digest must be a lowercase SHA-256")
    if artifact.signature_digest is not None and SHA256_PATTERN.fullmatch(artifact.signature_digest) is None:
        raise ContractError("release signature digest must be a lowercase SHA-256")
    expected_packaging = "zip" if artifact.platform.startswith("windows-") else "tar.gz"
    if artifact.packaging != expected_packaging:
        raise ContractError("release packaging does not match its Product platform")
    if artifact.platform.startswith("linux-") and artifact.signature_digest is None:
        raise ContractError("Linux release artifacts require a signature digest")


@dataclass(frozen=True)
class CertifiedArtifactSet:
    source_sha: str
    plan_fingerprint: ValidationFingerprint
    artifacts: tuple[ReleaseArtifact, ...]
    certification_manifest_id: str
    retention_class: str = RELEASE_RETENTION
    build_count: int = 1


def validate_artifact_set(artifact_set: CertifiedArtifactSet) -> None:
    if not isinstance(artifact_set.source_sha, str) or SHA1_PATTERN.fullmatch(artifact_set.source_sha) is None:
        raise ContractError("release source must be a lowercase 40-character SHA")
    if not isinstance(artifact_set.certification_manifest_id, str) or not artifact_set.certification_manifest_id:
        raise ContractError("release certification manifest identity is required")
    if artifact_set.retention_class not in {RELEASE_RETENTION, PUBLISHED_RETENTION}:
        raise ContractError("release artifact retention class is unsupported")
    validate_fingerprint(artifact_set.plan_fingerprint)
    if isinstance(artifact_set.build_count, bool) or artifact_set.build_count != 1:
        raise ContractError("Release certification must build one final artifact set")
    if len(artifact_set.artifacts) != len(PRODUCT_PLATFORMS):
        raise ContractError("release artifact set must contain exactly four Product artifacts")
    names = set()
    for artifact in artifact_set.artifacts:
        validate_artifact(artifact)
        if artifact.name in names:
            raise ContractError(f"duplicate release artifact: {artifact.name}")
        names.add(artifact.name)
    platforms = {artifact.platform for artifact in artifact_set.artifacts}
    if platforms != PRODUCT_PLATFORMS:
        missing = sorted(PRODUCT_PLATFORMS - platforms)
        raise ContractError(f"release artifact set is missing Product platforms: {missing}")
    if len(platforms) != len(artifact_set.artifacts):
        raise ContractError("release artifact set contains duplicate Product platforms")
    source = dict(artifact_set.plan_fingerprint.source)
    if source.get("candidateSha") != artifact_set.source_sha:
        raise ContractError("release fingerprint is bound to a different source")


def artifact_set_to_dict(artifact_set: CertifiedArtifactSet) -> dict[str, object]:
    validate_artifact_set(artifact_set)
    return {
        "sourceSha": artifact_set.source_sha,
        "planFingerprint": fingerprint_to_dict(artifact_set.plan_fingerprint),
        "artifacts": [artifact_to_dict(artifact) for artifact in artifact_set.artifacts],
        "certificationManifestId": artifact_set.certification_manifest_id,
        "retentionClass": artifact_set.retention_class,
        "buildCount": artifact_set.build_count,
    }


def artifact_set_from_dict(
    payload: object,
    *,
    expected_fingerprint: ValidationFingerprint | None = None,
) -> CertifiedArtifactSet:
    if not isinstance(payload, dict) or set(payload) != {
        "sourceSha",
        "planFingerprint",
        "artifacts",
        "certificationManifestId",
        "retentionClass",
        "buildCount",
    }:
        raise ContractError("Certified artifact set has invalid fields")
    fingerprint = fingerprint_from_dict(payload["planFingerprint"])
    if expected_fingerprint is not None and fingerprint != expected_fingerprint:
        raise ContractError("Certified artifact set fingerprint changed")
    artifacts = payload["artifacts"]
    if not isinstance(artifacts, list):
        raise ContractError("Certified artifact set artifacts must be an array")
    artifact_set = CertifiedArtifactSet(
        source_sha=payload["sourceSha"],
        plan_fingerprint=fingerprint,
        artifacts=tuple(artifact_from_dict(item) for item in artifacts),
        certification_manifest_id=payload["certificationManifestId"],
        retention_class=payload["retentionClass"],
        build_count=payload["buildCount"],
    )
    validate_artifact_set(artifact_set)
    return artifact_set


def certify_artifacts(
    integrated_manifest: EvidenceManifest,
    artifacts: Iterable[ReleaseArtifact],
    *,
    plan_fingerprint: ValidationFingerprint | None = None,
    certification_manifest_id: str = "release-certification",
) -> CertifiedArtifactSet:
    validate_manifest(integrated_manifest)
    if (
        integrated_manifest.family != "integrated-certification"
        or integrated_manifest.stage != "integrated"
        or integrated_manifest.outcome != "passed"
        or integrated_manifest.disposition != "required"
        or integrated_manifest.candidate.kind != "integrated"
    ):
        raise ContractError("Release certification requires passed Integrated evidence")
    if plan_fingerprint is not None and plan_fingerprint != integrated_manifest.fingerprint:
        raise ContractError("Release certification fingerprint differs from Integrated evidence")
    artifact_set = CertifiedArtifactSet(
        source_sha=integrated_manifest.candidate.candidate_sha,
        plan_fingerprint=plan_fingerprint or integrated_manifest.fingerprint,
        artifacts=tuple(artifacts),
        certification_manifest_id=certification_manifest_id,
    )
    validate_artifact_set(artifact_set)
    return artifact_set


@dataclass(frozen=True)
class PublicationRequest:
    source_sha: str
    certification_manifest_id: str
    artifacts: tuple[ReleaseArtifact, ...]
    rebuild: bool = False
    repackage: bool = False
    resign: bool = False
    public_authorized: bool = False


def verify_promotion(
    artifact_set: CertifiedArtifactSet,
    request: PublicationRequest,
    *,
    state: str = "clean",
) -> None:
    validate_artifact_set(artifact_set)
    if not isinstance(request, PublicationRequest):
        raise ContractError("publication request has an invalid structure")
    if not all(
        isinstance(flag, bool)
        for flag in (
            request.rebuild,
            request.repackage,
            request.resign,
            request.public_authorized,
        )
    ):
        raise ContractError("publication request flags must be boolean")
    if state not in {"clean", "recovery", "degraded", "certification-lock"}:
        raise ContractError(f"publication state is unsupported: {state}")
    if state in {"recovery", "degraded", "certification-lock"}:
        raise ContractError(f"publication is forbidden in {state} state")
    if not request.public_authorized:
        raise ContractError("public publication requires separate explicit authorization")
    if request.source_sha != artifact_set.source_sha:
        raise ContractError("publication source changed after Release certification")
    if request.certification_manifest_id != artifact_set.certification_manifest_id:
        raise ContractError("publication certification manifest changed")
    if request.rebuild or request.repackage or request.resign:
        raise ContractError("publication cannot rebuild, repackage, or resign certified bytes")
    if request.artifacts != artifact_set.artifacts:
        raise ContractError("publication artifact digests or metadata changed")


def release_evidence_manifest(
    artifact_set: CertifiedArtifactSet,
    *,
    candidate,
    duration_seconds: float = 0,
    created_at: int = 0,
    published: bool = False,
) -> EvidenceManifest:
    validate_artifact_set(artifact_set)
    validate_candidate(candidate)
    if candidate.candidate_sha != artifact_set.source_sha:
        raise ContractError("release evidence candidate differs from certified source")
    retention = PUBLISHED_RETENTION if published else RELEASE_RETENTION
    manifest = EvidenceManifest(
        schema_version=SCHEMA_VERSION,
        evidence_id=artifact_set.certification_manifest_id,
        family="release-packaging",
        stage="release",
        candidate=candidate,
        producer="release-certification",
        outcome="passed",
        disposition="required",
        fingerprint=artifact_set.plan_fingerprint,
        artifact_digests=tuple(
            digest
            for artifact in artifact_set.artifacts
            for digest in (
                (
                    (artifact.name, artifact.digest),
                    (f"{artifact.name}.provenance", artifact.provenance_digest),
                )
                + (
                    ((f"{artifact.name}.signature", artifact.signature_digest),)
                    if artifact.signature_digest is not None
                    else ()
                )
            )
        ),
        retention_class=retention,
        duration_seconds=duration_seconds,
        critical_path_seconds=duration_seconds,
        reason="one immutable Certified artifact set passed packaging and smoke checks",
        created_at=created_at,
        expires_at=(created_at + 30 * 86_400 if created_at and not published else None),
    )
    validate_manifest(manifest)
    return manifest
