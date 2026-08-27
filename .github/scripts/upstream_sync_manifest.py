"""Canonical domain model for immutable upstream Synchronization attempts."""

import json
import re
from dataclasses import dataclass
from pathlib import PurePosixPath


MANIFEST_DIRECTORY = ".github/upstream-sync-manifests"
MAX_CONFLICTS_SHOWN = 20
MAX_PULL_REQUEST_BODY_CHARACTERS = 8_000
RELEASE_URL_PREFIX = "https://github.com/openai/codex/releases/tag/"
_MAX_REPOSITORY_PATH_LENGTH = 4096
_SHA_PATTERN = re.compile(r"[0-9a-f]{40}")
_TAG_PATTERN = re.compile(
    r"^rust-v(?:0|[1-9]\d*)\."
    r"(?:0|[1-9]\d*)\."
    r"(?:0|[1-9]\d*)"
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
    if not _valid_release_tag(tag):
        raise ValueError("release tag must be an exact rust-v<SemVer> tag")
    return f"{RELEASE_URL_PREFIX}{tag}"


def manifest_filename(release_commit: str) -> str:
    _validate_sha(release_commit, "release commit")
    return f"{release_commit}.json"


def manifest_path(release_commit: str) -> str:
    return f"{MANIFEST_DIRECTORY}/{manifest_filename(release_commit)}"


def serialize_manifest(manifest: SynchronizationManifest) -> str:
    _validate_manifest(manifest)
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
        payload = json.loads(text)
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


def validate_chain(
    manifests: tuple[SynchronizationManifest, ...],
    expected_seed: SynchronizationManifest | None = None,
) -> SynchronizationManifest | None:
    if not manifests:
        if expected_seed is not None:
            raise ValueError("Synchronization manifest seed is missing")
        return None
    for manifest in manifests:
        _validate_manifest(manifest)
    if expected_seed is not None:
        _validate_manifest(expected_seed)

    by_commit = {manifest.release.commit: manifest for manifest in manifests}
    if len(by_commit) != len(manifests) or len(
        {manifest.release.tag for manifest in manifests}
    ) != len(manifests):
        raise ValueError("Synchronization manifest chain contains a duplicate release")

    roots = [
        manifest for manifest in manifests if manifest.previous_release_commit is None
    ]
    if len(roots) != 1:
        raise ValueError("Synchronization manifest chain must have exactly one root")

    children: dict[str, list[SynchronizationManifest]] = {}
    for manifest in manifests:
        previous = manifest.previous_release_commit
        if previous is None:
            continue
        if previous not in by_commit:
            raise ValueError(f"Synchronization manifest predecessor {previous} is missing")
        children.setdefault(previous, []).append(manifest)
    if any(len(items) > 1 for items in children.values()):
        raise ValueError("Synchronization manifest chain forks")

    tips = [manifest for manifest in manifests if manifest.release.commit not in children]
    if len(tips) != 1:
        raise ValueError("Synchronization manifest chain must have exactly one tip")

    ordered: list[SynchronizationManifest] = []
    visited: set[str] = set()
    current: SynchronizationManifest | None = roots[0]
    while current is not None:
        if current.release.commit in visited:
            raise ValueError("Synchronization manifest chain contains a cycle")
        ordered.append(current)
        visited.add(current.release.commit)
        next_items = children.get(current.release.commit, [])
        current = next_items[0] if next_items else None
    if len(ordered) != len(manifests):
        raise ValueError("Synchronization manifest chain contains a cycle or disconnected link")
    if expected_seed is not None and ordered[0] != expected_seed:
        raise ValueError("Synchronization manifest seed does not match PR #153")
    return ordered[-1]


def render_pull_request_body(manifest: SynchronizationManifest) -> str:
    _validate_manifest(manifest)
    conflicts = manifest.conflict_paths
    if conflicts:
        next_action = (
            "Perform explicit Semantic reconciliation, then mark this PR ready "
            "for review."
        )
    else:
        next_action = "Review the Baseline reconciliation and approve its workflow runs."
    predecessor = manifest.previous_release_commit or "none (seed)"
    body = f"""\
Synchronizes the published Codex CLI release `{manifest.release.tag}`.

- Upstream release: {manifest.release.url}
- Release baseline: `{manifest.release.commit}`
- Fork baseline: `{manifest.fork_base_sha}`
- Previous release baseline: `{predecessor}`
- Selection: {manifest.selection_mode}
- Preparation: {manifest.preparation_mode}
- Manifest: `{manifest_path(manifest.release.commit)}`
- Rust workspace version: normalized to `0.0.0`

CI triggered by this `GITHUB_TOKEN`-created PR requires maintainer approval.

Next action: {next_action}
"""
    if not conflicts:
        if len(body) > MAX_PULL_REQUEST_BODY_CHARACTERS:
            raise ValueError("pull-request body metadata exceeds its character budget")
        return body

    encoded_paths = [
        f"    {json.dumps(path, ensure_ascii=True)}"
        for path in conflicts[:MAX_CONFLICTS_SHOWN]
    ]
    for shown_count in range(len(encoded_paths), -1, -1):
        omitted = len(conflicts) - shown_count
        context = f"\nConflicts ({len(conflicts)} total; showing {shown_count}):"
        if shown_count:
            shown_paths = "\n".join(encoded_paths[:shown_count])
            context += f"\n\n{shown_paths}"
        context += (
            f"\n\nOmitted conflicts: {omitted}. The full list remains in "
            f"`{manifest_path(manifest.release.commit)}`.\n"
        )
        candidate = body + context
        if len(candidate) <= MAX_PULL_REQUEST_BODY_CHARACTERS:
            return candidate
    raise ValueError("pull-request body metadata exceeds its character budget")


def _exact_object(value: object, keys: set[str], name: str) -> None:
    if not isinstance(value, dict) or set(value) != keys:
        expected = ", ".join(sorted(keys))
        raise ValueError(f"{name} must contain exactly: {expected}")


def _validate_manifest(manifest: SynchronizationManifest) -> None:
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
    if not _valid_release_tag(manifest.release.tag):
        raise ValueError("release tag must be an exact rust-v<SemVer> tag")
    if manifest.release.url != canonical_release_url(manifest.release.tag):
        raise ValueError("release URL is not canonical")
    _validate_sha(manifest.release.commit, "release.commit")
    _validate_sha(manifest.fork_base_sha, "forkBaseSha")
    previous = manifest.previous_release_commit
    if previous is not None:
        if not isinstance(previous, str):
            raise ValueError(
                "previousReleaseCommit must be null or a lowercase 40-character SHA"
            )
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


def _validate_sha(value: str, name: str) -> None:
    if not isinstance(value, str) or _SHA_PATTERN.fullmatch(value) is None:
        raise ValueError(f"{name} must be a lowercase 40-character SHA")


def _valid_release_tag(tag: object) -> bool:
    if not isinstance(tag, str):
        return False
    match = _TAG_PATTERN.fullmatch(tag)
    if match is None:
        return False
    prerelease = match.group("prerelease")
    return prerelease is None or not any(
        part.isdigit() and len(part) > 1 and part.startswith("0")
        for part in prerelease.split(".")
    )


def _valid_path(path: str) -> bool:
    if "\0" in path or len(path) > _MAX_REPOSITORY_PATH_LENGTH:
        return False
    parsed = PurePosixPath(path)
    return (
        bool(path)
        and path != "."
        and not parsed.is_absolute()
        and parsed.as_posix() == path
        and not {".", ".."}.intersection(parsed.parts)
    )
