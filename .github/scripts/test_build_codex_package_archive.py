import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
ARCHIVE_SCRIPT = REPO_ROOT / ".github" / "scripts" / "build-codex-package-archive.sh"


class BuildCodexPackageArchiveTests(unittest.TestCase):
    def test_exports_repo_root_for_package_builder(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_root = Path(temp_dir)
            bin_dir = temp_root / "bin"
            bin_dir.mkdir()
            python_stub = bin_dir / "python3"
            python_stub.write_text(
                textwrap.dedent(
                    """\
                    #!/usr/bin/env bash
                    set -euo pipefail
                    if [[ "${CODEX_REPO_ROOT-}" != "${CODEX_TEST_EXPECTED_REPO_ROOT}" ]]; then
                      printf 'unexpected CODEX_REPO_ROOT: %s\\n' "${CODEX_REPO_ROOT-}" >&2
                      exit 23
                    fi
                    """
                ),
                encoding="utf-8",
            )
            python_stub.chmod(0o755)

            env = os.environ.copy()
            env.pop("CODEX_REPO_ROOT", None)
            env.update(
                {
                    "CODEX_TEST_EXPECTED_REPO_ROOT": str(REPO_ROOT),
                    "GITHUB_WORKSPACE": str(REPO_ROOT),
                    "PATH": f"{bin_dir}{os.pathsep}{env['PATH']}",
                    "RUNNER_TEMP": str(temp_root / "runner-temp"),
                }
            )
            result = subprocess.run(
                [
                    "bash",
                    str(ARCHIVE_SCRIPT),
                    "--target",
                    "x86_64-unknown-linux-musl",
                    "--bundle",
                    "primary",
                    "--entrypoint-dir",
                    str(temp_root / "entrypoints"),
                    "--archive-dir",
                    str(temp_root / "archives"),
                ],
                cwd=REPO_ROOT,
                env=env,
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(0, result.returncode, result.stderr)


if __name__ == "__main__":
    unittest.main()
