#!/usr/bin/env python3

"""Decide which V8 canary work is needed for a commit range.

The reusable workflow deliberately has no trigger-level path filters because
blocking-ci calls it for pull requests and it also supports manual dispatch.
Keeping the patterns here gives those entrypoints one source of truth;
unrelated pull requests still run metadata but skip the expensive matrices.
"""

import argparse
import subprocess
import tomllib
from dataclasses import dataclass
from fnmatch import fnmatchcase
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
# These patterns replace the old pull_request/push path filters. Include parent
# workflow changes because they can alter whether the canary is invoked.
CANARY_PATH_PATTERNS = {
    ".bazelrc",
    ".github/actions/setup-bazel-ci/**",
    ".github/actions/setup-ci/**",
    ".github/scripts/run_bazel_with_buildbuddy.py",
    ".github/scripts/check_v8_canary_topology.py",
    ".github/scripts/rusty_v8_bazel.py",
    ".github/scripts/rusty_v8_module_bazel.py",
    ".github/scripts/setup-dev-drive.ps1",
    ".github/scripts/test_check_v8_canary_topology.py",
    ".github/scripts/test_v8_canary_changes.py",
    ".github/scripts/test_v8_canary_result.py",
    ".github/scripts/v8_canary_changes.py",
    ".github/scripts/v8_canary_result.py",
    ".github/workflows/blocking-ci.yml",
    ".github/workflows/repo-checks.yml",
    ".github/workflows/README.md",
    ".github/workflows/rusty-v8-release.yml",
    ".github/workflows/v8-canary.yml",
    "MODULE.bazel",
    "MODULE.bazel.lock",
    "codex-rs/Cargo.toml",
    "codex-rs/v8-poc/**",
    "patches/BUILD.bazel",
    "patches/llvm_*.patch",
    "patches/rules_cc_*.patch",
    "patches/v8_*.patch",
    "third_party/v8/**",
}
KNOWN_IRRELEVANT_PATH_PATTERNS = {
    "*.md",
    "codex-rs/**",
    "docs/**",
}
# Windows source builds are a narrower, more expensive subset of the canary.
# A V8 version change also requires them even when no path below changed.
WINDOWS_SOURCE_BUILD_PATHS = {
    ".github/actions/setup-ci/**",
    ".github/scripts/rusty_v8_bazel.py",
    ".github/scripts/rusty_v8_module_bazel.py",
    ".github/scripts/setup-dev-drive.ps1",
    ".github/scripts/v8_canary_changes.py",
    ".github/workflows/rusty-v8-release.yml",
    ".github/workflows/v8-canary.yml",
}


@dataclass(frozen=True)
class CanaryDecision:
    required: bool
    reason: str


@dataclass(frozen=True)
class CanaryMetadata:
    canary: CanaryDecision
    windows_source_required: bool


def matching_canary_paths(changed_files: set[str]) -> set[str]:
    """Return changed paths that require the general V8 build matrix."""
    return {
        path
        for path in changed_files
        if any(fnmatchcase(path, pattern) for pattern in CANARY_PATH_PATTERNS)
    }


def canary_required(
    changed_files: set[str],
    base_v8_version: str,
    head_v8_version: str,
    *,
    force: bool = False,
) -> bool:
    """Return whether the general V8 build matrix should run."""
    return classify_changed_files(
        changed_files, base_v8_version, head_v8_version, force=force
    ).required


def classify_changed_files(
    changed_files: set[str],
    base_v8_version: str,
    head_v8_version: str,
    *,
    force: bool = False,
) -> CanaryDecision:
    if force:
        return CanaryDecision(True, "manual workflow dispatch")
    if base_v8_version != head_v8_version:
        return CanaryDecision(
            True, f"v8 version changed from {base_v8_version} to {head_v8_version}"
        )
    matched = sorted(matching_canary_paths(changed_files))
    if matched:
        return CanaryDecision(True, _bounded_reason("V8 canary path changed: ", matched))
    if not changed_files:
        return CanaryDecision(True, "comparison returned no changed paths")
    unknown = sorted(
        path
        for path in changed_files
        if not any(
            fnmatchcase(path, pattern)
            for pattern in KNOWN_IRRELEVANT_PATH_PATTERNS
        )
    )
    if unknown:
        return CanaryDecision(True, _bounded_reason("unknown V8 impact: ", unknown))
    count = len(changed_files)
    noun = "path is" if count == 1 else "paths are"
    return CanaryDecision(False, f"all {count} changed {noun} explicitly V8-irrelevant")


def matching_windows_source_paths(changed_files: set[str]) -> set[str]:
    """Return changed paths that require Windows rusty_v8 source builds."""
    return {
        path
        for path in changed_files
        if any(fnmatchcase(path, pattern) for pattern in WINDOWS_SOURCE_BUILD_PATHS)
    }


def resolved_v8_version(cargo_lock: bytes) -> str:
    versions = sorted(
        {
            package["version"]
            for package in tomllib.loads(cargo_lock.decode())["package"]
            if package["name"] == "v8"
        }
    )
    if len(versions) != 1:
        raise ValueError(f"expected exactly one resolved v8 version, found: {versions}")
    return versions[0]


def windows_source_required(
    changed_files: set[str],
    base_v8_version: str,
    head_v8_version: str,
    *,
    force: bool = False,
) -> bool:
    """Return whether Windows must rebuild rusty_v8 from source."""
    return (
        force
        or base_v8_version != head_v8_version
        or bool(matching_windows_source_paths(changed_files))
    )


def git_output(*args: str, root: Path = ROOT) -> bytes:
    return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.PIPE)


def _bounded_reason(prefix: str, paths: list[str]) -> str:
    reason = prefix + ", ".join(paths)
    return reason if len(reason) <= 240 else reason[:237] + "..."


def v8_version_at_revision(revision: str, *, root: Path = ROOT) -> str:
    return resolved_v8_version(
        git_output("show", f"{revision}:codex-rs/Cargo.lock", root=root)
    )


def merge_base(base: str, head: str, *, root: Path = ROOT) -> str:
    return git_output("merge-base", base, head, root=root).decode().strip()


def changed_files(base: str, head: str, *, root: Path = ROOT) -> set[str]:
    # Three-dot diff gives PRs merge-base semantics while remaining equivalent
    # to before/after for ordinary linear pushes to main.
    output = git_output(
        "diff",
        "--name-only",
        "--no-renames",
        f"{base}...{head}",
        root=root,
    )
    return set(output.decode().splitlines())


def decision_for_revisions(
    base: str | None,
    head: str | None,
    *,
    force: bool = False,
    root: Path = ROOT,
) -> CanaryDecision:
    return metadata_for_revisions(base, head, force=force, root=root).canary


def metadata_for_revisions(
    base: str | None,
    head: str | None,
    *,
    force: bool = False,
    root: Path = ROOT,
) -> CanaryMetadata:
    if force:
        return CanaryMetadata(
            CanaryDecision(True, "manual workflow dispatch"),
            windows_source_required=True,
        )
    if not base or not head:
        return CanaryMetadata(
            CanaryDecision(True, "comparison is missing base or head"),
            windows_source_required=True,
        )
    try:
        files = changed_files(base, head, root=root)
        common = merge_base(base, head, root=root)
        base_version = v8_version_at_revision(common, root=root)
        head_version = v8_version_at_revision(head, root=root)
        return CanaryMetadata(
            classify_changed_files(files, base_version, head_version),
            windows_source_required(
                files,
                base_version,
                head_version,
            ),
        )
    except Exception as error:
        return CanaryMetadata(
            CanaryDecision(True, f"comparison failed ({type(error).__name__})"),
            windows_source_required=True,
        )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base")
    parser.add_argument("--head")
    parser.add_argument("--force", action="store_true")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    metadata = metadata_for_revisions(args.base, args.head, force=args.force)
    print(f"canary_required={str(metadata.canary.required).lower()}")
    print(f"canary_reason={metadata.canary.reason}")
    print(
        "windows_source_required="
        f"{str(metadata.windows_source_required).lower()}"
    )


if __name__ == "__main__":
    main()
