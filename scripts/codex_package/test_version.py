#!/usr/bin/env python3

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import version


class VersionTest(unittest.TestCase):
    def test_replace_workspace_version_updates_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            manifest = Path(temp_dir) / "Cargo.toml"
            manifest.write_text(
                textwrap.dedent(
                    """\
                    [workspace]
                    resolver = "2"

                    [workspace.package]
                    version = "0.0.0"
                    edition = "2024"
                    """
                ),
                encoding="utf-8",
            )

            version.replace_workspace_version(manifest, "1.2.3-beta.4")

            self.assertIn('version = "1.2.3-beta.4"', manifest.read_text(encoding="utf-8"))

    def test_resolve_upstream_build_version_uses_current_non_placeholder(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="1.2.3")

            with patch.object(version, "REPO_ROOT", repo):
                self.assertEqual(version.resolve_upstream_build_version(), "1.2.3")

    def test_resolve_upstream_build_version_uses_highest_release_in_ancestry(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="0.0.0")
            cargo_toml = repo / "codex-rs" / "Cargo.toml"

            write_workspace_version(cargo_toml, "0.148.0-alpha.9")
            commit_all(repo, "Release 0.148.0-alpha.9", day=2)
            write_workspace_version(cargo_toml, "0.148.0-alpha.12")
            commit_all(repo, "Release 0.148.0-alpha.12", day=3)
            write_workspace_version(cargo_toml, "0.0.0")
            commit_all(repo, "Resume development", day=4)

            git(repo, "switch", "--create", "unmerged-release")
            write_workspace_version(cargo_toml, "0.148.0-alpha.19")
            commit_all(repo, "Release 0.148.0-alpha.19", day=5)
            git(repo, "switch", "main")

            with patch.object(version, "REPO_ROOT", repo):
                self.assertEqual(
                    version.resolve_upstream_build_version(),
                    "0.148.0-alpha.12",
                )


def create_repo(root: Path, *, initial_version: str) -> Path:
    repo = root / "repo"
    (repo / "codex-rs").mkdir(parents=True)
    write_workspace_version(repo / "codex-rs" / "Cargo.toml", initial_version)
    git(repo, "init", "--initial-branch=main")
    git(repo, "config", "user.name", "Version Test")
    git(repo, "config", "user.email", "version@test.local")
    commit_all(repo, "Initial source", day=1)
    return repo


def write_workspace_version(path: Path, workspace_version: str) -> None:
    path.write_text(
        textwrap.dedent(
            f"""\
            [workspace]
            resolver = "2"

            [workspace.package]
            version = "{workspace_version}"
            edition = "2024"
            """
        ),
        encoding="utf-8",
    )


def commit_all(repo: Path, message: str, *, day: int) -> None:
    git(repo, "add", ".")
    timestamp = f"2026-08-{day:02d}T12:00:00+00:00"
    git(
        repo,
        "commit",
        "--message",
        message,
        extra_env={"GIT_AUTHOR_DATE": timestamp, "GIT_COMMITTER_DATE": timestamp},
    )


def git(
    repo: Path, *arguments: str, extra_env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *arguments],
        env={**os.environ, **(extra_env or {})},
        check=True,
        text=True,
        capture_output=True,
    )


if __name__ == "__main__":
    unittest.main()
