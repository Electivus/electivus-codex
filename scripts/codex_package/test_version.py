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

            self.assertIn(
                'version = "1.2.3-beta.4"', manifest.read_text(encoding="utf-8")
            )

    def test_resolve_upstream_build_version_uses_current_non_placeholder(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="1.2.3")

            with patch.object(version, "REPO_ROOT", repo):
                self.assertEqual(
                    version.resolve_upstream_build_version(
                        "also-invalid",
                        environment={version.UPSTREAM_VERSION_ENV_VAR: "invalid"},
                    ),
                    "1.2.3",
                )

    def test_resolve_upstream_build_version_uses_highest_release_in_ancestry(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="0.0.0")
            cargo_toml = repo / "codex-rs" / "Cargo.toml"

            write_workspace_version(cargo_toml, "0.148.0-alpha.12")
            commit_all(repo, "Release 0.148.0-alpha.12", day=2)
            write_workspace_version(cargo_toml, "0.148.0-alpha.9")
            commit_all(repo, "Release 0.148.0-alpha.9", day=3)
            write_workspace_version(cargo_toml, "0.0.0")
            commit_all(repo, "Resume development", day=4)

            git(repo, "switch", "--create", "unmerged-release")
            write_workspace_version(cargo_toml, "0.148.0-alpha.19")
            commit_all(repo, "Release 0.148.0-alpha.19", day=5)
            git(repo, "switch", "main")

            with patch.object(version, "REPO_ROOT", repo):
                self.assertEqual(
                    version.resolve_upstream_build_version(environment={}),
                    "0.148.0-alpha.12",
                )

    def test_release_commit_need_not_change_workspace_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="0.0.0")
            cargo_toml = repo / "codex-rs" / "Cargo.toml"

            write_workspace_version(cargo_toml, "2.3.4")
            commit_all(repo, "Prepare release", day=2)
            (repo / "release-notes.txt").write_text("ready\n", encoding="utf-8")
            commit_all(repo, "Release 2.3.4", day=3)
            write_workspace_version(cargo_toml, "0.0.0")
            commit_all(repo, "Resume development", day=4)

            with patch.object(version, "REPO_ROOT", repo):
                self.assertEqual(
                    version.resolve_upstream_build_version(environment={}),
                    "2.3.4",
                )

    def test_semver_precedence_includes_stable_and_numeric_prereleases(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="0.0.0")
            cargo_toml = repo / "codex-rs" / "Cargo.toml"
            releases = (
                "1.0.0-alpha.9",
                "1.0.0-alpha.12",
                "1.0.0",
                "1.1.0-alpha.1",
            )
            for day, release in enumerate(releases, start=2):
                write_workspace_version(cargo_toml, release)
                commit_all(repo, f"Release {release}", day=day)
            write_workspace_version(cargo_toml, "0.0.0")
            commit_all(repo, "Resume development", day=7)

            with patch.object(version, "REPO_ROOT", repo):
                self.assertEqual(
                    version.resolve_upstream_build_version(environment={}),
                    "1.1.0-alpha.1",
                )

    def test_malformed_release_candidates_are_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="0.0.0")
            cargo_toml = repo / "codex-rs" / "Cargo.toml"

            write_workspace_version(cargo_toml, "1.2.3")
            commit_all(repo, "Release v1.2.3", day=2)
            commit_all(repo, "Release 9.9.9", day=3)
            commit_all(repo, "release 1.2.3", day=4)
            write_workspace_version(cargo_toml, "0.0.0")
            commit_all(repo, "Resume development", day=5)

            with patch.object(version, "REPO_ROOT", repo):
                with self.assertRaisesRegex(RuntimeError, "shallow or synthetic"):
                    version.resolve_upstream_build_version(environment={})

    def test_explicit_override_precedes_environment_override(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="0.0.0")

            with patch.object(version, "REPO_ROOT", repo):
                self.assertEqual(
                    version.resolve_upstream_build_version(
                        "2.0.0-beta.1",
                        environment={version.UPSTREAM_VERSION_ENV_VAR: "1.2.3"},
                    ),
                    "2.0.0-beta.1",
                )

    def test_environment_override_precedes_ancestral_discovery(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="0.0.0")

            with patch.object(version, "REPO_ROOT", repo):
                self.assertEqual(
                    version.resolve_upstream_build_version(
                        environment={version.UPSTREAM_VERSION_ENV_VAR: "3.4.5+ci.7"}
                    ),
                    "3.4.5+ci.7",
                )

    def test_invalid_overrides_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="0.0.0")

            with patch.object(version, "REPO_ROOT", repo):
                for invalid_version in (
                    "",
                    "0.0.0",
                    "v1.2.3",
                    "rust-v1.2.3",
                    "1.2",
                    "1.2.3-01",
                ):
                    with self.subTest(version=invalid_version):
                        with self.assertRaisesRegex(RuntimeError, "bare SemVer"):
                            version.resolve_upstream_build_version(
                                invalid_version,
                                environment={},
                            )

    def test_line_breaks_in_overrides_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = create_repo(Path(temp_dir), initial_version="0.0.0")

            with patch.object(version, "REPO_ROOT", repo):
                for invalid_version in (
                    "1.2.3\n",
                    "1.2.3\r",
                    "1.2.3\r\n",
                    "1.2.3\nignored",
                ):
                    with self.subTest(source="explicit", version=invalid_version):
                        with self.assertRaisesRegex(RuntimeError, "bare SemVer"):
                            version.resolve_upstream_build_version(
                                invalid_version,
                                environment={},
                            )
                    with self.subTest(source="environment", version=invalid_version):
                        with self.assertRaisesRegex(RuntimeError, "bare SemVer"):
                            version.resolve_upstream_build_version(
                                environment={
                                    version.UPSTREAM_VERSION_ENV_VAR: invalid_version
                                }
                            )

    def test_no_provable_baseline_explains_explicit_overrides(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source_repo = create_repo(root, initial_version="0.0.0")
            cargo_toml = source_repo / "codex-rs" / "Cargo.toml"
            write_workspace_version(cargo_toml, "1.2.3")
            commit_all(source_repo, "Release 1.2.3", day=2)
            write_workspace_version(cargo_toml, "0.0.0")
            commit_all(source_repo, "Resume development", day=3)
            repo = root / "shallow"
            git(root, "clone", "--depth", "1", f"file://{source_repo}", str(repo))

            with patch.object(version, "REPO_ROOT", repo):
                with self.assertRaisesRegex(
                    RuntimeError,
                    r"History was not fetched.*--upstream-version <SEMVER>.*"
                    + version.UPSTREAM_VERSION_ENV_VAR,
                ):
                    version.resolve_upstream_build_version(environment={})


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
        "--allow-empty",
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
