"""Version discovery for Codex packages."""

from dataclasses import dataclass
from functools import total_ordering
import os
from pathlib import Path
import re
import subprocess

from .targets import REPO_ROOT


SEMVER_PATTERN = re.compile(
    r"^(?P<major>0|[1-9]\d*)\.(?P<minor>0|[1-9]\d*)\.(?P<patch>0|[1-9]\d*)"
    r"(?:-(?P<prerelease>(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
RELEASE_SUBJECT_PATTERN = re.compile(r"^Release (?P<version>.+)$")
UPSTREAM_VERSION_ENV_VAR = "CODEX_UPSTREAM_VERSION"
WORKSPACE_VERSION_BLOCK_PATTERN = re.compile(
    r'(?ms)(^\[workspace\.package\]\s+(?:(?!^\[).)*?^\s*version\s*=\s*")([^"]+)(")'
)


@total_ordering
@dataclass(frozen=True)
class SemVer:
    major: int
    minor: int
    patch: int
    prerelease: tuple[str, ...]

    def __lt__(self, other: object) -> bool:
        if not isinstance(other, SemVer):
            return NotImplemented

        if (self.major, self.minor, self.patch) != (
            other.major,
            other.minor,
            other.patch,
        ):
            return (self.major, self.minor, self.patch) < (
                other.major,
                other.minor,
                other.patch,
            )

        if not self.prerelease and not other.prerelease:
            return False
        if not self.prerelease:
            return False
        if not other.prerelease:
            return True

        return _prerelease_is_less_than(self.prerelease, other.prerelease)


def _prerelease_is_less_than(left: tuple[str, ...], right: tuple[str, ...]) -> bool:
    for left_part, right_part in zip(left, right):
        if left_part == right_part:
            continue

        left_is_numeric = left_part.isdigit()
        right_is_numeric = right_part.isdigit()

        if left_is_numeric and right_is_numeric:
            return int(left_part) < int(right_part)
        if left_is_numeric:
            return True
        if right_is_numeric:
            return False
        return left_part < right_part

    return len(left) < len(right)


def _parse_semver(version: str) -> SemVer | None:
    match = SEMVER_PATTERN.match(version)
    if match is None:
        return None

    prerelease_text = match.group("prerelease")
    prerelease = tuple(prerelease_text.split(".")) if prerelease_text else ()
    return SemVer(
        major=int(match.group("major")),
        minor=int(match.group("minor")),
        patch=int(match.group("patch")),
        prerelease=prerelease,
    )


def validate_upstream_build_version(version: str, source: str) -> str:
    if _parse_semver(version) is None or version == "0.0.0":
        raise RuntimeError(
            f"Invalid upstream build version from {source}: {version!r}. "
            "Expected a bare SemVer other than 0.0.0 (for example, "
            "0.148.0-alpha.5); tag prefixes are not accepted."
        )
    return version


def _workspace_version_from_text(manifest_text: str, manifest_path: Path | str) -> str:
    match = WORKSPACE_VERSION_BLOCK_PATTERN.search(manifest_text)
    if match is None:
        raise RuntimeError(
            f"Could not find [workspace.package].version in {manifest_path}."
        )
    return match.group(2)


def read_workspace_version() -> str:
    cargo_toml = REPO_ROOT / "codex-rs" / "Cargo.toml"
    return _workspace_version_from_text(
        cargo_toml.read_text(encoding="utf-8"),
        cargo_toml,
    )


def replace_workspace_version(cargo_manifest_path: Path, version: str) -> None:
    manifest_text = cargo_manifest_path.read_text(encoding="utf-8")
    match = WORKSPACE_VERSION_BLOCK_PATTERN.search(manifest_text)
    if match is None:
        raise RuntimeError(
            f"Could not find [workspace.package].version in {cargo_manifest_path}."
        )

    version_start = match.start(2)
    version_end = match.end(2)
    updated_text = (
        f"{manifest_text[:version_start]}{version}{manifest_text[version_end:]}"
    )
    cargo_manifest_path.write_text(updated_text, encoding="utf-8", newline="")


def _run_git(arguments: list[str], error_message: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(REPO_ROOT), *arguments],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(error_message)
    return result.stdout


def resolve_upstream_build_version(explicit_version: str | None = None) -> str:
    current_workspace_version = read_workspace_version()
    if current_workspace_version != "0.0.0":
        return current_workspace_version

    if explicit_version is not None:
        return validate_upstream_build_version(explicit_version, "--upstream-version")

    environment_version = os.environ.get(UPSTREAM_VERSION_ENV_VAR)
    if environment_version is not None:
        return validate_upstream_build_version(
            environment_version,
            UPSTREAM_VERSION_ENV_VAR,
        )

    log_output = _run_git(
        [
            "log",
            "--full-history",
            "--format=%H%x09%s",
            "HEAD",
        ],
        "Could not inspect repository history for the upstream release version.",
    )

    selected_version_text: str | None = None
    selected_version: SemVer | None = None

    for entry in log_output.splitlines():
        commit_sha, separator, subject = entry.partition("\t")
        if not separator:
            continue

        release_match = RELEASE_SUBJECT_PATTERN.match(subject)
        if release_match is None:
            continue

        version_text = release_match.group("version")
        semantic_version = _parse_semver(version_text)
        if semantic_version is None or version_text == "0.0.0":
            continue

        manifest_result = subprocess.run(
            [
                "git",
                "-C",
                str(REPO_ROOT),
                "show",
                f"{commit_sha}:codex-rs/Cargo.toml",
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        if manifest_result.returncode != 0:
            continue
        try:
            manifest_version = _workspace_version_from_text(
                manifest_result.stdout,
                f"{commit_sha}:codex-rs/Cargo.toml",
            )
        except RuntimeError:
            continue
        if manifest_version != version_text:
            continue

        if selected_version is None or selected_version < semantic_version:
            selected_version = semantic_version
            selected_version_text = version_text

    if selected_version_text is None:
        raise RuntimeError(
            "Could not prove a valid upstream Release baseline in HEAD's ancestry. "
            "This can happen in a shallow or synthetic checkout. History was not "
            "fetched. Retry with --upstream-version <SEMVER> or set "
            f"{UPSTREAM_VERSION_ENV_VAR}."
        )

    return selected_version_text
