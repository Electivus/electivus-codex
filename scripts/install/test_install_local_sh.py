#!/usr/bin/env python3

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


INSTALL_SCRIPT = Path(__file__).with_name("install-local.sh")
REPO_ROOT = INSTALL_SCRIPT.parents[2]
BUILD_SCRIPT = REPO_ROOT / "scripts" / "build_codex_package.py"
TARGET = "x86_64-unknown-linux-gnu"
RELEASE_PREFIX = f"local-debug-{TARGET}"


class InstallLocalShTest(unittest.TestCase):
    def test_upstream_version_build_sets_repository_root_for_package_helpers(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)

            result = run_installer(root, use_upstream_version=True)

            self.assertEqual(result.returncode, 0, result.stderr)

    def test_dev_build_disables_debug_assertions_without_using_release(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)

            result = run_installer(root)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (root / "build.log").read_text(encoding="utf-8").splitlines(),
                ["cargo_profile=dev-small", "debug_assertions=false"],
            )

    def test_successful_install_keeps_new_release_and_two_previous(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            releases_dir = root / "codex-home" / "packages" / "standalone" / "releases"
            releases_dir.mkdir(parents=True)
            previous_releases = [
                releases_dir / f"{RELEASE_PREFIX}-20260728072517-1",
                releases_dir / f"{RELEASE_PREFIX}-20260728194547-2",
                releases_dir / f"{RELEASE_PREFIX}-20260730171629-3",
                releases_dir / f"{RELEASE_PREFIX}-20260730190235-4",
            ]
            for timestamp, release in enumerate(reversed(previous_releases), start=1):
                release.mkdir()
                os.utime(release, (timestamp, timestamp))

            result = run_installer(root)

            self.assertEqual(result.returncode, 0, result.stderr)
            retained = sorted(path.name for path in releases_dir.iterdir())
            current = root / "codex-home" / "packages" / "standalone" / "current"
            generated = current.resolve().name
            self.assertEqual(
                retained,
                sorted(
                    [generated, previous_releases[-2].name, previous_releases[-1].name]
                ),
            )

    def test_failed_install_does_not_prune_previous_releases(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            releases_dir = root / "codex-home" / "packages" / "standalone" / "releases"
            releases_dir.mkdir(parents=True)
            previous_releases = [
                releases_dir / "previous-1",
                releases_dir / "previous-2",
                releases_dir / "previous-3",
            ]
            for release in previous_releases:
                release.mkdir()

            result = run_installer(root, codex_exit=1)

            self.assertNotEqual(result.returncode, 0)
            retained = [path for path in releases_dir.iterdir() if path.is_dir()]
            self.assertEqual(len(retained), 4)
            for previous_release in previous_releases:
                self.assertTrue(previous_release.is_dir())


def run_installer(
    root: Path, *, codex_exit: int = 0, use_upstream_version: bool = False
) -> subprocess.CompletedProcess[str]:
    fake_bin = root / "fake-bin"
    fake_bin.mkdir()
    home = root / "home"
    home.mkdir()
    install_bin = root / "install-bin"

    write_executable(fake_bin / "cargo", "#!/bin/sh\nexit 0\n")
    write_executable(
        fake_bin / "uname",
        """\
        #!/bin/sh
        case "$1" in
          -s) printf 'Linux\n' ;;
          -m) printf 'x86_64\n' ;;
          *) printf 'Linux\n' ;;
        esac
        """,
    )
    write_executable(
        fake_bin / "date",
        """\
        #!/bin/sh
        case "$1" in
          +%Y%m%d%H%M%S) printf '20260731120000\n' ;;
          +%s) printf '1785500000\n' ;;
          *) exec /bin/date "$@" ;;
        esac
        """,
    )
    write_executable(
        fake_bin / "python3",
        f"""\
        #!/bin/sh
        if [ "$1" = "-c" ]; then
          case "$2" in
            *read_workspace_version*)
              printf '0.0.0\n'
              exit 0
              ;;
            *resolve_upstream_build_version*)
              if [ "${{CODEX_REPO_ROOT-}}" != "$CODEX_TEST_EXPECTED_REPO_ROOT" ]; then
                printf 'unexpected CODEX_REPO_ROOT: %s\n' "${{CODEX_REPO_ROOT-}}" >&2
                exit 1
              fi
              printf '0.0.0\n'
              exit 0
              ;;
          esac
        fi

        if [ "$1" != "{BUILD_SCRIPT}" ]; then
          exec "{sys.executable}" "$@"
        fi

        shift
        package_dir=""
        target=""
        cargo_profile=""
        while [ "$#" -gt 0 ]; do
          case "$1" in
            --package-dir)
              shift
              package_dir="$1"
              ;;
            --target)
              shift
              target="$1"
              ;;
            --cargo-profile)
              shift
              cargo_profile="$1"
              ;;
          esac
          shift
        done

        printf 'cargo_profile=%s\ndebug_assertions=%s\n' \
          "$cargo_profile" "${{CARGO_PROFILE_DEV_DEBUG_ASSERTIONS-unset}}" \
          >"$CODEX_TEST_BUILD_LOG"
        mkdir -p "$package_dir/bin" "$package_dir/codex-path" \
          "$package_dir/codex-resources"
        printf '#!/bin/sh\nexit %s\n' "$CODEX_TEST_CODEX_EXIT" \
          >"$package_dir/bin/codex"
        printf '#!/bin/sh\nexit 0\n' >"$package_dir/codex-path/rg"
        printf '#!/bin/sh\nexit 0\n' >"$package_dir/codex-resources/bwrap"
        chmod +x "$package_dir/bin/codex" "$package_dir/codex-path/rg" \
          "$package_dir/codex-resources/bwrap"
        printf '{{"target": "%s"}}\n' "$target" \
          >"$package_dir/codex-package.json"
        """,
    )

    env = {
        **os.environ,
        "PATH": f"{fake_bin}:/usr/bin:/bin",
        "HOME": str(home),
        "SHELL": "/bin/sh",
        "CODEX_HOME": str(root / "codex-home"),
        "CODEX_INSTALL_DIR": str(install_bin),
        "CODEX_TEST_BUILD_LOG": str(root / "build.log"),
        "CODEX_TEST_CODEX_EXIT": str(codex_exit),
        "CODEX_TEST_EXPECTED_REPO_ROOT": str(REPO_ROOT),
        "TMPDIR": str(root),
    }
    env.pop("CODEX_REPO_ROOT", None)
    arguments = ["sh", str(INSTALL_SCRIPT)]
    if use_upstream_version:
        arguments.append("--use-upstream-version")
    return subprocess.run(
        arguments,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )


def write_executable(path: Path, content: str) -> None:
    path.write_text(textwrap.dedent(content), encoding="utf-8")
    path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
