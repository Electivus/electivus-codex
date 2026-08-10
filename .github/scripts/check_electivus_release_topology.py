#!/usr/bin/env python3
"""Fail closed when the Electivus Linux/Windows release boundary drifts."""

import argparse
from pathlib import Path
import re
import sys


SOURCES = (
    ".github/workflows/electivus-release.yml",
    ".github/workflows/rust-release-windows-unsigned.yml",
    ".github/workflows/README.md",
)


def validate_topology(release: str, windows: str, documentation: str) -> list[str]:
    literal_targets = set(re.findall(r"^\s+target: ([a-z0-9_-]+)$", release, re.MULTILINE))
    literal_runners = set(
        re.findall(r"^\s+(?:- )?runner: ([a-z0-9_.-]+)$", release, re.MULTILINE)
    )
    checks = (
        (
            "dedicated tag namespace",
            '- "electivus-v*.*.*"' in release
            and "^electivus-v[0-9]+" in release
            and "github.repository == 'Electivus/electivus-codex'" in release,
        ),
        (
            "hosted Linux matrix",
            literal_targets
            == {"aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl"}
            and literal_runners == {"ubuntu-24.04", "ubuntu-24.04-arm"},
        ),
        (
            "Linux keyless signing",
            "id-token: write" in release
            and "uses: ./.github/actions/linux-code-sign" in release
            and "*.sigstore" in release,
        ),
        (
            "unsigned Windows reuse",
            "uses: ./.github/workflows/rust-release-windows-unsigned.yml" in release
            and "publish_release: false" in release
            and "workflow_call:" in windows
            and "^(windows|electivus)-v" in windows
            and "group: rust-release-windows-unsigned-${{ inputs.release_tag }}"
            in windows,
        ),
        (
            "release-only source invariant",
            'source_version}" != "0.0.0"' in release
            and 'changed_files[0]}" != "codex-rs/Cargo.toml"' in release
            and 'additions}" != "1"' in release
            and 'deletions}" != "1"' in release,
        ),
        (
            "stable and prerelease classification",
            'if [[ "${version}" == *-* ]]' in release
            and "prerelease: ${{ needs.metadata.outputs.prerelease }}" in release
            and "make_latest: ${{ needs.metadata.outputs.make_latest }}" in release,
        ),
        (
            "GitHub-only publication boundary",
            "softprops/action-gh-release" in release
            and "npm publish" not in release
            and "r2-release.yml" not in release
            and "winget-releaser" not in release
            and "apple-darwin" not in literal_targets,
        ),
        (
            "upstream-compatible public filenames",
            '"${dest}/${binary}-${TARGET}"' in release
            and '"${dest}/electivus-${binary}-${TARGET}"' not in release
            and "codex-electivus" not in release,
        ),
        (
            "terminal release verification",
            "Verify published Electivus release" in release
            and "codex-package_SHA256SUMS" in release
            and "codex-x86_64-unknown-linux-musl.zst" in release
            and "codex-x86_64-pc-windows-msvc.exe.zip" in release
            and 'contains("apple-darwin")' in release,
        ),
        (
            "documented operator contract",
            "electivus-v0.1.0" in documentation
            and "`codex-rs/Cargo.lock`" in documentation
            and "unchanged and do not merge the release commit" in documentation
            and "Public binary and package filenames remain compatible" in documentation,
        ),
    )
    return [
        f"Electivus release topology drift: {label}"
        for label, valid in checks
        if not valid
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    repo = parser.parse_args(argv).repo.resolve()
    try:
        issues = validate_topology(
            *[(repo / path).read_text(encoding="utf-8") for path in SOURCES]
        )
    except (OSError, UnicodeError) as error:
        issues = [f"cannot read Electivus release sources: {error}"]
    if issues:
        print(
            "Electivus release topology failed:\n"
            + "\n".join(f"- {issue}" for issue in issues),
            file=sys.stderr,
        )
        return 1
    print("Electivus release topology passed: Linux and Windows GitHub assets only")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
