"""Strict domain model for immutable upstream Synchronization manifests."""

import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import PurePosixPath


MANIFEST_DIRECTORY = ".github/upstream-sync-manifests"
RELEASE_URL_PREFIX = "https://github.com/openai/codex/releases/tag/"
_MAX_REPOSITORY_PATH_LENGTH = 4096
_PR153_RELEASE_COMMIT = "b3a6d7f67cf056e18472c2b9ec26d3999ed40b7b"
# SHA-256 of the canonical PR #153 seed, binding every field and conflict path.
_PR153_SEED_MANIFEST_SHA256 = (
    "f7a3f94ef75e8f911ae6e9b4e123a65cfb1223c06b1c6ea07352123d6388620e"
)
_SHA_PATTERN = re.compile(r"[0-9a-f]{40}")
_TAG_PATTERN = re.compile(
    r"^rust-v(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)"
    r"(?:-(?P<prerelease>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)


@dataclass(frozen=True)
class ReleaseIdentity:
    tag: str
    commit: str
    url: str


@dataclass(frozen=True)
class SynchronizationManifest:
    schema_version: int
    release: ReleaseIdentity
    fork_base_sha: str
    previous_release_commit: str | None
    selection_mode: str
    preparation_mode: str
    conflict_paths: tuple[str, ...]


def canonical_release_url(tag: str) -> str:
    if _release_tag_match(tag) is None:
        raise ValueError("release tag must be an exact rust-v<SemVer> tag")
    return f"{RELEASE_URL_PREFIX}{tag}"


def manifest_filename(release_commit: str) -> str:
    _validate_sha(release_commit, "release commit")
    return f"{release_commit}.json"


def manifest_path(release_commit: str) -> str:
    return f"{MANIFEST_DIRECTORY}/{manifest_filename(release_commit)}"


def serialize_manifest(manifest: SynchronizationManifest) -> str:
    _validate_manifest(manifest)
    return _canonical_manifest_text(manifest)


def _canonical_manifest_text(manifest: SynchronizationManifest) -> str:
    payload = {
        "schemaVersion": manifest.schema_version,
        "release": {
            "tag": manifest.release.tag,
            "commit": manifest.release.commit,
            "url": manifest.release.url,
        },
        "forkBaseSha": manifest.fork_base_sha,
        "previousReleaseCommit": manifest.previous_release_commit,
        "selectionMode": manifest.selection_mode,
        "preparationMode": manifest.preparation_mode,
        "conflictPaths": list(manifest.conflict_paths),
    }
    return json.dumps(payload, indent=2, ensure_ascii=False) + "\n"


def parse_manifest(text: str) -> SynchronizationManifest:
    try:
        payload = json.loads(
            text,
            object_pairs_hook=_unique_object,
            parse_constant=_reject_json_constant,
        )
    except json.JSONDecodeError as error:
        raise ValueError(f"invalid Synchronization manifest JSON: {error}") from error
    _exact_object(
        payload,
        {
            "schemaVersion",
            "release",
            "forkBaseSha",
            "previousReleaseCommit",
            "selectionMode",
            "preparationMode",
            "conflictPaths",
        },
        "manifest",
    )
    release = payload["release"]
    _exact_object(release, {"tag", "commit", "url"}, "release")
    conflicts = payload["conflictPaths"]
    if not isinstance(conflicts, list):
        raise ValueError("conflictPaths must be an array")
    manifest = SynchronizationManifest(
        schema_version=payload["schemaVersion"],
        release=ReleaseIdentity(
            tag=release["tag"],
            commit=release["commit"],
            url=release["url"],
        ),
        fork_base_sha=payload["forkBaseSha"],
        previous_release_commit=payload["previousReleaseCommit"],
        selection_mode=payload["selectionMode"],
        preparation_mode=payload["preparationMode"],
        conflict_paths=tuple(conflicts),
    )
    _validate_manifest(manifest)
    if serialize_manifest(manifest) != text:
        raise ValueError("Synchronization manifest is not canonically serialized")
    return manifest


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(
                f"Synchronization manifest contains duplicate field: {key}"
            )
        result[key] = value
    return result


def _reject_json_constant(value: str) -> object:
    raise ValueError(f"invalid Synchronization manifest JSON constant: {value}")


def _exact_object(value: object, keys: set[str], name: str) -> None:
    if not isinstance(value, dict) or set(value) != keys:
        expected = ", ".join(sorted(keys))
        raise ValueError(f"{name} must contain exactly: {expected}")


def _validate_manifest(manifest: SynchronizationManifest) -> None:
    if not isinstance(manifest, SynchronizationManifest) or not isinstance(
        manifest.release, ReleaseIdentity
    ):
        raise ValueError("Synchronization manifest has an invalid structure")
    if type(manifest.schema_version) is not int or manifest.schema_version != 1:
        raise ValueError("unsupported Synchronization manifest schemaVersion")
    string_values = (
        manifest.release.tag,
        manifest.release.commit,
        manifest.release.url,
        manifest.fork_base_sha,
        manifest.selection_mode,
        manifest.preparation_mode,
    )
    if any(not isinstance(value, str) for value in string_values):
        raise ValueError("Synchronization manifest scalar fields must be strings")
    tag_match = _release_tag_match(manifest.release.tag)
    if tag_match is None:
        raise ValueError("release tag must be an exact rust-v<SemVer> tag")
    if manifest.release.url != canonical_release_url(manifest.release.tag):
        raise ValueError("release URL is not canonical")
    _validate_sha(manifest.release.commit, "release.commit")
    _validate_sha(manifest.fork_base_sha, "forkBaseSha")
    previous = manifest.previous_release_commit
    if previous is not None:
        _validate_sha(previous, "previousReleaseCommit")
    if manifest.selection_mode not in ("automatic", "manual"):
        raise ValueError("invalid selectionMode")
    if manifest.preparation_mode not in ("clean", "conflicting"):
        raise ValueError("invalid preparationMode")
    if type(manifest.conflict_paths) is not tuple:
        raise ValueError("conflictPaths must be an immutable sequence")
    if any(
        not isinstance(path, str) or not _valid_path(path)
        for path in manifest.conflict_paths
    ):
        raise ValueError("conflictPaths contains an invalid repository path")
    if tuple(sorted(set(manifest.conflict_paths))) != manifest.conflict_paths:
        raise ValueError("conflictPaths must be sorted and unique")
    if (manifest.preparation_mode == "clean") != (not manifest.conflict_paths):
        raise ValueError("preparationMode and conflictPaths disagree")
    if manifest.release.commit == _PR153_RELEASE_COMMIT:
        manifest_digest = hashlib.sha256(
            _canonical_manifest_text(manifest).encode("utf-8")
        ).hexdigest()
        if manifest_digest != _PR153_SEED_MANIFEST_SHA256:
            raise ValueError("PR #153 seed manifest does not match its canonical form")
    elif (
        manifest.selection_mode == "automatic"
        and tag_match.group("prerelease") is not None
    ):
        raise ValueError("automatic selection requires a stable release")


def _validate_sha(value: object, name: str) -> None:
    if not isinstance(value, str) or _SHA_PATTERN.fullmatch(value) is None:
        raise ValueError(f"{name} must be a lowercase 40-character SHA")


def _release_tag_match(tag: object) -> re.Match[str] | None:
    if not isinstance(tag, str):
        return None
    match = _TAG_PATTERN.fullmatch(tag)
    if match is None:
        return None
    prerelease = match.group("prerelease")
    if prerelease is not None and any(
        part.isdigit() and len(part) > 1 and part.startswith("0")
        for part in prerelease.split(".")
    ):
        return None
    return match


def _valid_path(path: str) -> bool:
    try:
        encoded_path = path.encode("utf-8")
    except UnicodeEncodeError:
        return False
    if b"\0" in encoded_path or len(encoded_path) > _MAX_REPOSITORY_PATH_LENGTH:
        return False
    parsed = PurePosixPath(path)
    return (
        bool(path)
        and path != "."
        and not parsed.is_absolute()
        and parsed.as_posix() == path
        and not {".", ".."}.intersection(parsed.parts)
    )
