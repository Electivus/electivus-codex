#!/usr/bin/env python3

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest


INSTALL_SCRIPT = Path(__file__).with_name("install-local.ps1")
POWERSHELL = shutil.which("pwsh")
WINDOWS_POWERSHELL = shutil.which("powershell.exe")


@unittest.skipUnless(
    os.name == "nt" and WINDOWS_POWERSHELL,
    "requires Windows PowerShell",
)
class WindowsPowerShellCompatibilityTest(unittest.TestCase):
    def test_windows_powershell_5_1_requires_pwsh(self) -> None:
        result = subprocess.run(
            [
                str(WINDOWS_POWERSHELL),
                "-NoLogo",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(INSTALL_SCRIPT),
                "--help",
            ],
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "install-local.ps1 requires PowerShell 7 or newer",
            result.stdout + result.stderr,
        )
        self.assertIn("pwsh", result.stdout + result.stderr)


@unittest.skipUnless(os.name == "nt" and POWERSHELL, "requires PowerShell 7")
class InstallLocalPowerShellTest(unittest.TestCase):
    def test_upstream_version_ignores_non_ancestor_and_restores_cargo_files(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            cargo_toml = repo / "codex-rs" / "Cargo.toml"
            cargo_lock = repo / "codex-rs" / "Cargo.lock"
            original_cargo_toml = cargo_toml.read_bytes()
            original_cargo_lock = cargo_lock.read_bytes()
            build_log = root / "build-version.txt"

            result = run_installer(root, repo, build_log)

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(
                build_log.read_text(encoding="utf-8"),
                "0.148.0-alpha.12\ndev\nfalse",
            )
            self.assertEqual(cargo_toml.read_bytes(), original_cargo_toml)
            self.assertEqual(cargo_lock.read_bytes(), original_cargo_lock)

    def test_upstream_version_uses_semver_precedence_for_ancestors(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            cargo_toml = repo / "codex-rs" / "Cargo.toml"

            write_workspace_version(cargo_toml, "0.148.0-alpha.9")
            commit_all(repo, "Release 0.148.0-alpha.9", day=6)
            write_workspace_version(cargo_toml, "0.0.0")
            commit_all(repo, "Resume development again", day=7)
            original_cargo_toml = cargo_toml.read_bytes()
            original_cargo_lock = (repo / "codex-rs" / "Cargo.lock").read_bytes()
            build_log = root / "build-version.txt"

            result = run_installer(root, repo, build_log)

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(
                build_log.read_text(encoding="utf-8"),
                "0.148.0-alpha.12\ndev\nfalse",
            )
            self.assertEqual(cargo_toml.read_bytes(), original_cargo_toml)
            self.assertEqual(
                (repo / "codex-rs" / "Cargo.lock").read_bytes(),
                original_cargo_lock,
            )

    def test_existing_workspace_version_is_authoritative(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            cargo_toml = repo / "codex-rs" / "Cargo.toml"
            write_workspace_version(cargo_toml, "1.2.3-beta.4")
            original_cargo_toml = cargo_toml.read_bytes()
            original_cargo_lock = (repo / "codex-rs" / "Cargo.lock").read_bytes()
            build_log = root / "build-version.txt"

            result = run_installer(root, repo, build_log)

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(
                build_log.read_text(encoding="utf-8"),
                "1.2.3-beta.4\ndev\nfalse",
            )
            self.assertEqual(cargo_toml.read_bytes(), original_cargo_toml)
            self.assertEqual(
                (repo / "codex-rs" / "Cargo.lock").read_bytes(),
                original_cargo_lock,
            )


def create_repo(root: Path) -> Path:
    repo = root / "repo"
    (repo / "scripts" / "install").mkdir(parents=True)
    (repo / "codex-rs").mkdir()
    shutil.copy2(INSTALL_SCRIPT, repo / "scripts" / "install" / INSTALL_SCRIPT.name)
    (repo / "scripts" / "build_codex_package.py").write_text(
        textwrap.dedent(
            """\
            import os
            from pathlib import Path
            import re

            cargo_toml = Path(__file__).parents[1] / "codex-rs" / "Cargo.toml"
            match = re.search(
                r'(?ms)^\\[workspace\\.package\\].*?^version\\s*=\\s*"([^"]+)"',
                cargo_toml.read_text(encoding="utf-8"),
            )
            if match is None:
                raise RuntimeError("workspace version not found")
            arguments = os.sys.argv[1:]
            profile_index = arguments.index("--cargo-profile") + 1
            Path(os.environ["CODEX_TEST_BUILD_LOG"]).write_text(
                f"{match.group(1)}\\n{arguments[profile_index]}\\n"
                f"{os.environ.get('CARGO_PROFILE_DEV_DEBUG_ASSERTIONS', 'unset')}",
                encoding="utf-8",
            )
            raise SystemExit(23)
            """
        ),
        encoding="utf-8",
    )
    cargo_toml = repo / "codex-rs" / "Cargo.toml"
    write_workspace_version(cargo_toml, "0.0.0")
    (repo / "codex-rs" / "Cargo.lock").write_bytes(
        b"# exact lockfile bytes must survive\r\nversion = 4\r\n"
    )

    git(repo, "init", "--initial-branch=main")
    git(repo, "config", "user.name", "Codex Installer Test")
    git(repo, "config", "user.email", "installer@example.test")
    commit_all(repo, "Initial source", day=1)

    write_workspace_version(cargo_toml, "0.148.0-alpha.12")
    commit_all(repo, "Release 0.148.0-alpha.12", day=2)
    write_workspace_version(cargo_toml, "0.0.0")
    commit_all(repo, "Resume development", day=3)
    (repo / "source.txt").write_text("current fork source\n", encoding="utf-8")
    commit_all(repo, "Fork source change", day=4)

    git(repo, "switch", "--create", "unmerged-release")
    write_workspace_version(cargo_toml, "0.148.0-alpha.19")
    commit_all(repo, "Release 0.148.0-alpha.19", day=5)
    git(repo, "switch", "main")
    return repo


def write_workspace_version(path: Path, version: str) -> None:
    path.write_text(
        textwrap.dedent(
            f"""\
            [workspace]
            resolver = "2"

            [workspace.package]
            version = "{version}"
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


def run_installer(
    root: Path, repo: Path, build_log: Path
) -> subprocess.CompletedProcess[str]:
    temp = root / "temp"
    temp.mkdir()
    env = {
        **os.environ,
        "CODEX_HOME": str(root / "codex-home"),
        "CODEX_INSTALL_DIR": str(root / "install-bin"),
        "CODEX_TEST_BUILD_LOG": str(build_log),
        "TEMP": str(temp),
        "TMP": str(temp),
    }
    return subprocess.run(
        [
            str(POWERSHELL),
            "-NoLogo",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            str(repo / "scripts" / "install" / INSTALL_SCRIPT.name),
            "-UseUpstreamVersion",
        ],
        cwd=repo,
        env=env,
        text=True,
        encoding="utf-8",
        errors="replace",
        capture_output=True,
        check=False,
    )


if __name__ == "__main__":
    unittest.main()
