import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class RunBazelCiTests(unittest.TestCase):
    def test_keyless_windows_main_and_diagnostic_calls_share_local_config(self) -> None:
        bash = shutil.which("bash")
        if os.name == "nt" and (git := shutil.which("git")) is not None:
            git_bash = Path(git).parent.parent / "usr/bin/bash.exe"
            if git_bash.is_file():
                bash = str(git_bash)
        if bash is None:
            self.skipTest("bash is required to exercise run-bazel-ci.sh")

        repo = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temp_dir:
            temp = Path(temp_dir)
            script_dir = temp / "scripts"
            script_dir.mkdir()
            script = script_dir / "run-bazel-ci.sh"
            wrapper = script_dir / "run_bazel_with_buildbuddy.py"
            for source, destination in (
                (repo / ".github/scripts/run-bazel-ci.sh", script),
                (repo / ".github/scripts/run_bazel_with_buildbuddy.py", wrapper),
            ):
                destination.write_text(source.read_text(encoding="utf-8"), encoding="utf-8", newline="\n")
                destination.chmod(0o755)
            capture = temp / "bazel-args.txt"
            if os.name == "nt":
                fake_bazel = temp / "fake-bazel.cmd"
                fake_bazel.write_text(
                    '@echo off\r\n>>"%FAKE_BAZEL_CAPTURE%" echo %*\r\nexit /b 7\r\n',
                    encoding="utf-8",
                )
            else:
                fake_bazel = temp / "fake-bazel"
                fake_bazel.write_text(
                    '#!/usr/bin/env bash\nprintf \'%s\\n\' "$*" >> "$FAKE_BAZEL_CAPTURE"\nexit 7\n',
                    encoding="utf-8",
                )
                fake_bazel.chmod(0o755)

            env = os.environ.copy()
            for name in (
                "BAZEL_OUTPUT_USER_ROOT",
                "BAZEL_REPOSITORY_CACHE",
                "BAZEL_REPO_CONTENTS_CACHE",
                "BUILDBUDDY_API_KEY",
                "CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR",
            ):
                env.pop(name, None)
            env.update(
                CODEX_BAZEL_BIN=str(fake_bazel),
                CODEX_BAZEL_WINDOWS_PATH=r"C:\Windows\System32",
                FAKE_BAZEL_CAPTURE=str(capture),
                GITHUB_ACTIONS="false",
                GITHUB_JOB="windows-test",
                RUNNER_OS="Windows",
                RUNNER_TEMP=str(temp),
            )
            result = subprocess.run(
                [
                    bash,
                    script.as_posix(),
                    "--print-failed-test-logs",
                    "--windows-cross-compile",
                    "--",
                    "test",
                    "--",
                    "//fake:test",
                ],
                cwd=repo,
                env=env,
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(7, result.returncode, result.stdout + result.stderr)
            calls = capture.read_text(encoding="utf-8").splitlines()
            self.assertEqual(2, len(calls), calls)
            main_call, info_call = calls
            for expected in (
                "test",
                "--config=ci-windows",
                "--jobs=4",
                "--host_platform=//:local_windows_msvc",
            ):
                self.assertIn(expected, main_call)
            self.assertNotIn("ci-windows-cross", main_call)
            self.assertNotIn("buildbuddy", main_call.casefold())
            for expected in (
                "info",
                "--config=ci-windows",
                "--host_platform=//:local_windows_msvc",
                "bazel-testlogs",
            ):
                self.assertIn(expected, info_call)
            self.assertNotIn("ci-windows-cross", info_call)
            self.assertNotIn("buildbuddy", info_call.casefold())


if __name__ == "__main__":
    unittest.main()
