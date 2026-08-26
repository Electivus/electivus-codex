#!/usr/bin/env python3

import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import textwrap
import time
import unittest


INSTALL_SCRIPT = Path(__file__).with_name("install-local.sh")
SOURCE_REPO = INSTALL_SCRIPT.parents[2]
TARGET = "x86_64-unknown-linux-gnu"
RELEASE_PREFIX = f"local-debug-{TARGET}"


class InstallLocalShTest(unittest.TestCase):
    def test_default_build_preserves_dirty_workspace_and_uses_development_version(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            cargo_toml = repo / "codex-rs" / "Cargo.toml"
            cargo_lock = repo / "codex-rs" / "Cargo.lock"
            cargo_toml.write_bytes(cargo_toml.read_bytes() + b"# dirty manifest\r\n")
            cargo_lock.write_bytes(cargo_lock.read_bytes() + b"# dirty lock\r\n")
            (repo / "untracked-probe.txt").write_text("untracked\n", encoding="utf-8")
            original_files = cargo_toml.read_bytes(), cargo_lock.read_bytes()

            result = run_installer(root, repo)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                read_build_log(root),
                {
                    "version": "0.0.0",
                    "cargo_profile": "dev",
                    "debug_assertions": "false",
                    "rg_bin": None,
                    "probe": "untracked\n",
                },
            )
            self.assertEqual(
                (cargo_toml.read_bytes(), cargo_lock.read_bytes()),
                original_files,
            )

    def test_automatic_version_uses_greatest_ancestral_release_and_restores_bytes(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            cargo_toml = repo / "codex-rs" / "Cargo.toml"
            cargo_lock = repo / "codex-rs" / "Cargo.lock"
            original_files = cargo_toml.read_bytes(), cargo_lock.read_bytes()

            result = run_installer(
                root,
                repo,
                arguments=["--use-upstream-version"],
                mutate_lock=True,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(read_build_log(root)["version"], "0.148.0-alpha.12")
            self.assertEqual(
                (cargo_toml.read_bytes(), cargo_lock.read_bytes()),
                original_files,
            )

    def test_explicit_override_precedes_environment_and_already_versioned_wins(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)

            result = run_installer(
                root,
                repo,
                arguments=["--upstream-version", "2.0.0-beta.1"],
                extra_env={"CODEX_UPSTREAM_VERSION": "3.0.0"},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(read_build_log(root)["version"], "2.0.0-beta.1")

            write_workspace_version(repo / "codex-rs" / "Cargo.toml", "4.5.6")
            second_root = root / "second"
            second_root.mkdir()
            result = run_installer(
                second_root,
                repo,
                arguments=["--upstream-version", "not-semver"],
                extra_env={"CODEX_UPSTREAM_VERSION": "also-invalid"},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(read_build_log(second_root)["version"], "4.5.6")

    def test_environment_override_enables_versioning_and_invalid_value_fails(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)

            result = run_installer(
                root,
                repo,
                extra_env={"CODEX_UPSTREAM_VERSION": "5.6.7+local.1"},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(read_build_log(root)["version"], "5.6.7+local.1")

            invalid_root = root / "invalid"
            invalid_root.mkdir()
            result = run_installer(
                invalid_root,
                repo,
                extra_env={"CODEX_UPSTREAM_VERSION": "rust-v5.6.7"},
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("tag prefixes are not accepted", result.stderr)
            self.assertFalse((invalid_root / "build.json").exists())

    def test_windows_delegation_rejects_unix_only_version_sources(self) -> None:
        cases = (
            ("argument", ["--upstream-version", "1.2.3"], {}),
            ("equals-argument", ["--upstream-version=1.2.3"], {}),
            ("environment", [], {"CODEX_UPSTREAM_VERSION": "1.2.3"}),
        )
        for name, arguments, extra_env in cases:
            with (
                self.subTest(name=name),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                repo = create_repo(root)
                env = installer_env(root, repo, extra_env=extra_env)
                fake_bin = Path(env["PATH"].split(os.pathsep, 1)[0])
                delegated = root / "powershell-delegated"
                write_executable(
                    fake_bin / "uname",
                    '#!/bin/sh\ncase "$1" in -s) echo MINGW64_NT-10.0;; '
                    "-m) echo x86_64;; *) echo MINGW64_NT-10.0;; esac\n",
                )
                write_executable(
                    fake_bin / "pwsh",
                    '#!/bin/sh\ntouch "$CODEX_TEST_DELEGATED"\nexit 97\n',
                )
                env["CODEX_TEST_DELEGATED"] = str(delegated)

                result = subprocess.run(
                    [
                        "sh",
                        str(repo / "scripts/install/install-local.sh"),
                        *arguments,
                    ],
                    cwd=repo,
                    env=env,
                    text=True,
                    capture_output=True,
                    check=False,
                )

                self.assertNotEqual(result.returncode, 0)
                source = (
                    "CODEX_UPSTREAM_VERSION"
                    if name == "environment"
                    else "--upstream-version"
                )
                self.assertIn(f"{source} is Unix-only", result.stderr)
                self.assertIn("issue #167", result.stderr)
                self.assertFalse(delegated.exists())
                self.assertFalse((root / "build.json").exists())

    def test_no_provable_baseline_fails_without_fetching(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root, include_releases=False)
            refs_before = git(repo, "show-ref").stdout

            result = run_installer(
                root,
                repo,
                arguments=["--use-upstream-version"],
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("shallow or synthetic checkout", result.stderr)
            self.assertIn("--upstream-version <SEMVER>", result.stderr)
            self.assertEqual(git(repo, "show-ref").stdout, refs_before)

    def test_build_failure_and_signal_restore_files_without_pruning(self) -> None:
        for build_mode in ("fail", "signal"):
            with self.subTest(build_mode=build_mode):
                with tempfile.TemporaryDirectory() as temp_dir:
                    root = Path(temp_dir)
                    repo = create_repo(root)
                    releases_dir = prepare_previous_releases(root)
                    cargo_toml = repo / "codex-rs" / "Cargo.toml"
                    cargo_lock = repo / "codex-rs" / "Cargo.lock"
                    original_files = cargo_toml.read_bytes(), cargo_lock.read_bytes()

                    result = run_installer(
                        root,
                        repo,
                        arguments=["--use-upstream-version"],
                        build_mode=build_mode,
                        mutate_lock=True,
                    )

                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(
                        (cargo_toml.read_bytes(), cargo_lock.read_bytes()),
                        original_files,
                    )
                    self.assertEqual(
                        sorted(path.name for path in releases_dir.iterdir()),
                        ["previous-1", "previous-2", "previous-3"],
                    )

    def test_activation_failure_restores_prior_links_command_and_workspace(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            releases_dir = prepare_previous_releases(root)
            previous_release = releases_dir / "previous-1"
            (previous_release / "bin").mkdir()
            write_executable(
                previous_release / "bin/codex",
                "#!/bin/sh\nprintf 'previous installation\\n'\n",
            )
            current = releases_dir.parent / "current"
            current.symlink_to(previous_release)
            visible_command = root / "install-bin/codex"
            visible_command.parent.mkdir()
            visible_command.symlink_to(current / "bin/codex")
            previous_current_target = os.readlink(current)
            previous_command_target = os.readlink(visible_command)
            cargo_toml = repo / "codex-rs" / "Cargo.toml"
            cargo_lock = repo / "codex-rs" / "Cargo.lock"
            original_files = cargo_toml.read_bytes(), cargo_lock.read_bytes()

            result = run_installer(
                root,
                repo,
                arguments=["--use-upstream-version"],
                codex_exit=19,
                mutate_lock=True,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(
                (cargo_toml.read_bytes(), cargo_lock.read_bytes()),
                original_files,
            )
            self.assertEqual(os.readlink(current), previous_current_target)
            self.assertEqual(os.readlink(visible_command), previous_command_target)
            previous_version = subprocess.run(
                [str(visible_command), "--version"],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(previous_version.returncode, 0, previous_version.stderr)
            self.assertEqual(previous_version.stdout, "previous installation\n")
            self.assertGreaterEqual(len(list(releases_dir.iterdir())), 4)

    def test_missing_lockfile_is_restored_as_missing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            cargo_lock = repo / "codex-rs" / "Cargo.lock"
            cargo_lock.unlink()

            result = run_installer(
                root,
                repo,
                arguments=["--use-upstream-version"],
                mutate_lock=True,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(cargo_lock.exists())

    def test_forced_termination_marker_blocks_other_install_root_without_restoring(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)

            killed = run_installer(
                root,
                repo,
                arguments=["--use-upstream-version"],
                build_mode="kill",
                mutate_lock=True,
            )
            self.assertNotEqual(killed.returncode, 0)
            transaction_dir = version_transaction_dir(repo)
            self.assertTrue((transaction_dir / "Cargo.toml.original").is_file())
            self.assertTrue((transaction_dir / "Cargo.lock.original").is_file())

            cargo_toml = repo / "codex-rs" / "Cargo.toml"
            cargo_toml.write_bytes(cargo_toml.read_bytes() + b"# edit after crash\n")
            post_crash_bytes = cargo_toml.read_bytes()
            retry_root = root / "retry"
            retry_root.mkdir()
            retry = run_installer(
                retry_root,
                repo,
                arguments=["--upstream-version", "9.9.9"],
            )

            self.assertNotEqual(retry.returncode, 0)
            self.assertEqual(cargo_toml.read_bytes(), post_crash_bytes)
            self.assertIn(str(transaction_dir), retry.stderr)
            self.assertIn("Refusing to restore or mutate", retry.stderr)
            self.assertIn("Recovery steps", retry.stderr)

    def test_restore_verification_failure_retains_backups_before_activation(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)

            result = run_installer(
                root,
                repo,
                arguments=["--use-upstream-version"],
                build_mode="break-restore",
            )

            self.assertNotEqual(result.returncode, 0)
            transaction_dir = version_transaction_dir(repo)
            self.assertTrue((transaction_dir / "Cargo.toml.original").is_file())
            self.assertIn("Failed to restore and verify", result.stderr)
            self.assertFalse(
                (root / "codex-home" / "packages" / "standalone" / "current").exists()
            )

    def test_fallback_locks_serialize_without_publishing_pidless_directories(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            first_root = root / "first"
            second_root = root / "second"
            first_root.mkdir()
            second_root.mkdir()
            continue_path = root / "continue"
            first_env = installer_env(
                first_root,
                repo,
                build_mode="hold",
                extra_env={"CODEX_TEST_CONTINUE": str(continue_path)},
                force_fallback_locks=True,
            )
            second_env = installer_env(second_root, repo, force_fallback_locks=True)
            command = [
                "sh",
                str(repo / "scripts/install/install-local.sh"),
                "--use-upstream-version",
            ]

            first = subprocess.Popen(
                command,
                cwd=repo,
                env=first_env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            second = None
            try:
                wait_for_path(first_root / "build.ready")
                first_install_lock = (
                    first_root / "codex-home/packages/standalone/install.lock.d"
                )
                shared_version_lock = version_lock_path(repo)
                self.assertTrue(first_install_lock.is_file())
                self.assertTrue(
                    first_install_lock.read_text().splitlines()[0].isdigit()
                )
                self.assertTrue(shared_version_lock.is_file())
                self.assertTrue(
                    shared_version_lock.read_text().splitlines()[0].isdigit()
                )
                second = subprocess.Popen(
                    command,
                    cwd=repo,
                    env=second_env,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                wait_for_path(
                    second_root / "codex-home/packages/standalone/install.lock.d"
                )
                second_install_lock = (
                    second_root / "codex-home/packages/standalone/install.lock.d"
                )
                self.assertTrue(second_install_lock.is_file())
                self.assertTrue(
                    second_install_lock.read_text().splitlines()[0].isdigit()
                )
                self.assertFalse((second_root / "build.json").exists())
                continue_path.touch()
                first_stdout, first_stderr = first.communicate(timeout=10)
                second_stdout, second_stderr = second.communicate(timeout=10)
                self.assertEqual(first.returncode, 0, first_stderr + first_stdout)
                self.assertEqual(second.returncode, 0, second_stderr + second_stdout)
            finally:
                for process in (first, second):
                    if process is not None and process.poll() is None:
                        process.kill()
                        process.communicate()

    def test_fallback_reclaims_a_stale_legacy_mkdir_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            legacy_lock = version_lock_path(repo)
            legacy_lock.mkdir(parents=True)
            (legacy_lock / "pid").write_text("2147483647\n", encoding="utf-8")
            (legacy_lock / "started_at").write_text("1\n", encoding="utf-8")

            result = run_installer(
                root,
                repo,
                arguments=["--use-upstream-version"],
                force_fallback_locks=True,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(legacy_lock.exists())

    def test_recent_dead_fallback_lock_reports_when_to_retry(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            env = installer_env(root, repo, force_fallback_locks=True)
            standalone_root = root / "codex-home/packages/standalone"
            standalone_root.mkdir(parents=True)
            lock_path = standalone_root / "install.lock.d"
            lock_contents = "2147483647\n1787659200\nmissing-owner\n"
            lock_path.write_text(lock_contents, encoding="utf-8")
            process = subprocess.Popen(
                ["sh", str(repo / "scripts/install/install-local.sh")],
                cwd=repo,
                env=env,
                start_new_session=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                stdout, stderr = communicate_bounded(process)

                self.assertNotEqual(process.returncode, 0, stdout)
                self.assertIn("no longer live", stderr)
                self.assertIn("Retry after 600 seconds", stderr)
                self.assertEqual(lock_path.read_text(encoding="utf-8"), lock_contents)
                self.assertFalse((root / "build.json").exists())
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.communicate(timeout=2)

    def test_fallback_reclaimers_cannot_remove_a_new_version_lock_owner(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            version_lock = version_lock_path(repo)
            version_lock.parent.mkdir(parents=True, exist_ok=True)
            version_lock.write_text(
                "2147483647\n1\n" + str(version_lock.parent / "missing-owner") + "\n",
                encoding="utf-8",
            )
            reclaim_continue = root / "allow-reclaim"
            build_continue = root / "allow-build"
            command = [
                "sh",
                str(repo / "scripts/install/install-local.sh"),
                "--use-upstream-version",
            ]
            processes: list[subprocess.Popen[str]] = []
            process_roots = [root / name for name in ("first", "second", "third")]
            try:
                for process_root in process_roots[:2]:
                    process_root.mkdir()
                    env = installer_env(
                        process_root,
                        repo,
                        build_mode="hold",
                        extra_env={
                            "CODEX_TEST_CONTINUE": str(build_continue),
                            "CODEX_TEST_RECLAIM_CONTINUE": str(reclaim_continue),
                        },
                        force_fallback_locks=True,
                    )
                    install_reclaim_guard_barrier(env)
                    processes.append(
                        subprocess.Popen(
                            command,
                            cwd=repo,
                            env=env,
                            text=True,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                        )
                    )

                wait_for_glob_count(f"{version_lock}.reclaim.*", 2)
                reclaim_continue.touch()
                wait_for_any_path(
                    [process_root / "build.ready" for process_root in process_roots[:2]]
                )
                live_owner = version_lock.read_text(encoding="utf-8")
                live_pid = int(live_owner.splitlines()[0])
                os.kill(live_pid, 0)

                third_root = process_roots[2]
                third_root.mkdir()
                third_env = installer_env(
                    third_root,
                    repo,
                    build_mode="hold",
                    extra_env={
                        "CODEX_TEST_CONTINUE": str(build_continue),
                        "CODEX_TEST_RECLAIM_CONTINUE": str(reclaim_continue),
                    },
                    force_fallback_locks=True,
                )
                install_reclaim_guard_barrier(third_env)
                processes.append(
                    subprocess.Popen(
                        command,
                        cwd=repo,
                        env=third_env,
                        text=True,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                    )
                )
                time.sleep(0.2)
                self.assertFalse((third_root / "build.ready").exists())
                self.assertEqual(version_lock.read_text(encoding="utf-8"), live_owner)

                build_continue.touch()
                for process in processes:
                    stdout, stderr = process.communicate(timeout=10)
                    self.assertEqual(process.returncode, 0, stderr + stdout)
            finally:
                reclaim_continue.touch(exist_ok=True)
                build_continue.touch(exist_ok=True)
                for process in processes:
                    if process.poll() is None:
                        process.kill()
                        process.communicate()

    def test_live_reused_or_unverifiable_install_lock_pid_fails_closed(self) -> None:
        for case in ("fingerprint-mismatch", "unknown-identity"):
            with (
                self.subTest(case=case),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                repo = create_repo(root)
                env = installer_env(root, repo, force_fallback_locks=True)
                standalone_root = root / "codex-home/packages/standalone"
                standalone_root.mkdir(parents=True)
                lock_path = standalone_root / "install.lock.d"
                lock_lines = [str(os.getpid()), "1787659200", "foreign-owner"]
                if case == "fingerprint-mismatch":
                    lock_lines.append("fingerprint=definitely-not-this-process")
                lock_contents = "\n".join(lock_lines) + "\n"
                lock_path.write_text(lock_contents, encoding="utf-8")
                process = subprocess.Popen(
                    ["sh", str(repo / "scripts/install/install-local.sh")],
                    cwd=repo,
                    env=env,
                    start_new_session=True,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                try:
                    stdout, stderr = communicate_bounded(process)

                    self.assertNotEqual(process.returncode, 0, stdout)
                    self.assertIn(str(lock_path), stderr)
                    self.assertIn("manual recovery", stderr)
                    self.assertEqual(
                        lock_path.read_text(encoding="utf-8"), lock_contents
                    )
                    self.assertFalse((root / "build.json").exists())
                    self.assertFalse((standalone_root / "current").exists())
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=2)

    def test_malformed_fallback_lock_metadata_fails_closed_promptly(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            env = installer_env(root, repo, force_fallback_locks=True)
            standalone_root = root / "codex-home/packages/standalone"
            standalone_root.mkdir(parents=True)
            lock_path = standalone_root / "install.lock.d"
            lock_contents = "not-a-pid\nnot-a-timestamp\nforeign-owner\n"
            lock_path.write_text(lock_contents, encoding="utf-8")
            process = subprocess.Popen(
                ["sh", str(repo / "scripts/install/install-local.sh")],
                cwd=repo,
                env=env,
                start_new_session=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                stdout, stderr = communicate_bounded(process)

                self.assertNotEqual(process.returncode, 0, stdout)
                self.assertIn("metadata", stderr)
                self.assertIn("manual recovery", stderr)
                self.assertEqual(lock_path.read_text(encoding="utf-8"), lock_contents)
                self.assertFalse((root / "build.json").exists())
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.communicate(timeout=2)

    def test_fallback_hardlink_claim_failure_is_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            env = installer_env(root, repo, force_fallback_locks=True)
            fake_ln = Path(env["PATH"]) / "ln"
            fake_ln.unlink()
            write_executable(
                fake_ln,
                "#!/bin/sh\nprintf '%s\\n' 'simulated hard-link failure' >&2\nexit 95\n",
            )
            process = subprocess.Popen(
                ["sh", str(repo / "scripts/install/install-local.sh")],
                cwd=repo,
                env=env,
                start_new_session=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                stdout, stderr = communicate_bounded(process)

                self.assertNotEqual(process.returncode, 0, stdout)
                self.assertIn("Cannot claim the installer lock", stderr)
                self.assertIn("hard-link", stderr)
                self.assertIn("retry", stderr)
                self.assertFalse((root / "build.json").exists())
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.communicate(timeout=2)

    def test_signal_cleanup_preserves_a_successor_reclaim_guard(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            env = installer_env(root, repo, force_fallback_locks=True)
            standalone_root = root / "codex-home/packages/standalone"
            lock_path = standalone_root / "install.lock.d"
            lock_path.mkdir(parents=True)
            (lock_path / "pid").write_text("2147483647\n", encoding="utf-8")
            (lock_path / "started_at").write_text("1\n", encoding="utf-8")
            successor_marker = Path(f"{lock_path}.reclaim.successor")
            successor_guard = Path(f"{lock_path}.reclaim.guard")
            env["CODEX_TEST_SUCCESSOR_MARKER"] = str(successor_marker)
            install_guard_unlink_successor_signal(env)
            process = subprocess.Popen(
                ["sh", str(repo / "scripts/install/install-local.sh")],
                cwd=repo,
                env=env,
                start_new_session=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                stdout, stderr = communicate_bounded(process)

                self.assertEqual(process.returncode, 143, stderr + stdout)
                self.assertTrue(successor_marker.is_file())
                self.assertTrue(successor_guard.is_file())
                self.assertTrue(os.path.samefile(successor_marker, successor_guard))
                self.assertFalse((root / "build.json").exists())
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.communicate(timeout=2)

    def test_local_ripgrep_and_successful_retention_remain_supported(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repo = create_repo(root)
            releases_dir = prepare_previous_releases(root, timestamped=True)
            local_rg = root / "local-rg"
            write_executable(local_rg, "#!/bin/sh\nexit 0\n")

            result = run_installer(
                root,
                repo,
                extra_env={"CODEX_LOCAL_RG": str(local_rg)},
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(read_build_log(root)["rg_bin"], str(local_rg))
            current = root / "codex-home/packages/standalone/current"
            self.assertEqual(len(list(releases_dir.iterdir())), 3)
            self.assertTrue(current.resolve().is_dir())


def create_repo(root: Path, *, include_releases: bool = True) -> Path:
    repo = root / "repo"
    (repo / "scripts/install").mkdir(parents=True)
    (repo / "scripts/codex_package").mkdir()
    (repo / "codex-rs").mkdir()
    shutil.copy2(INSTALL_SCRIPT, repo / "scripts/install/install-local.sh")
    for name in ("__init__.py", "targets.py", "version.py"):
        shutil.copy2(
            SOURCE_REPO / "scripts/codex_package" / name,
            repo / "scripts/codex_package" / name,
        )
    (repo / "scripts/build_codex_package.py").write_text(
        BUILD_STUB,
        encoding="utf-8",
    )
    write_workspace_version(repo / "codex-rs/Cargo.toml", "0.0.0")
    (repo / "codex-rs/Cargo.lock").write_bytes(b"# original lock\r\nversion = 4\r\n")
    git(repo, "init", "--initial-branch=main")
    git(repo, "config", "user.name", "Installer Test")
    git(repo, "config", "user.email", "installer@example.test")
    commit_all(repo, "Initial source", day=1)
    if not include_releases:
        return repo

    cargo_toml = repo / "codex-rs/Cargo.toml"
    for day, release in ((2, "0.148.0-alpha.9"), (3, "0.148.0-alpha.12")):
        write_workspace_version(cargo_toml, release)
        commit_all(repo, f"Release {release}", day=day)
    write_workspace_version(cargo_toml, "0.0.0")
    commit_all(repo, "Resume development", day=4)
    git(repo, "switch", "--create", "unmerged-release")
    write_workspace_version(cargo_toml, "0.148.0-alpha.19")
    commit_all(repo, "Release 0.148.0-alpha.19", day=5)
    git(repo, "switch", "main")
    git(repo, "tag", "rust-v99.0.0")
    return repo


BUILD_STUB = textwrap.dedent(
    r"""
    import json
    import os
    from pathlib import Path
    import re
    import signal
    import sys
    import time

    repo = Path(__file__).parents[1]
    cargo_toml = repo / "codex-rs/Cargo.toml"
    cargo_lock = repo / "codex-rs/Cargo.lock"
    version = re.search(
        r'(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"',
        cargo_toml.read_text(encoding="utf-8"),
    ).group(1)
    args = sys.argv[1:]
    value_after = lambda name: args[args.index(name) + 1] if name in args else None
    log = {
        "version": version,
        "cargo_profile": value_after("--cargo-profile"),
        "debug_assertions": os.environ.get("CARGO_PROFILE_DEV_DEBUG_ASSERTIONS"),
        "rg_bin": value_after("--rg-bin"),
        "probe": (repo / "untracked-probe.txt").read_text(encoding="utf-8")
        if (repo / "untracked-probe.txt").exists()
        else None,
    }
    Path(os.environ["CODEX_TEST_BUILD_LOG"]).write_text(json.dumps(log), encoding="utf-8")
    if os.environ.get("CODEX_TEST_MUTATE_LOCK") == "1":
        cargo_lock.write_bytes(b"mutated by builder\n")
    mode = os.environ.get("CODEX_TEST_BUILD_MODE", "success")
    if mode == "fail":
        raise SystemExit(23)
    if mode == "signal":
        os.kill(os.getppid(), signal.SIGTERM)
        raise SystemExit(23)
    if mode == "kill":
        os.kill(os.getppid(), signal.SIGKILL)
        raise SystemExit(23)
    if mode == "hold":
        Path(os.environ["CODEX_TEST_READY"]).touch()
        while not Path(os.environ["CODEX_TEST_CONTINUE"]).exists():
            time.sleep(0.01)

    package_dir = Path(value_after("--package-dir"))
    target = value_after("--target")
    for directory in ("bin", "codex-path", "codex-resources"):
        (package_dir / directory).mkdir(parents=True, exist_ok=True)
    codex_exit = os.environ.get("CODEX_TEST_CODEX_EXIT", "0")
    files = {
        "bin/codex": f"#!/bin/sh\nexit {codex_exit}\n",
        "codex-path/rg": "#!/bin/sh\nexit 0\n",
        "codex-resources/bwrap": "#!/bin/sh\nexit 0\n",
    }
    for name, content in files.items():
        path = package_dir / name
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)
    (package_dir / "codex-package.json").write_text(
        json.dumps({"target": target}), encoding="utf-8"
    )
    if mode == "break-restore":
        cargo_toml.unlink()
        cargo_toml.mkdir()
    """
).lstrip()


def run_installer(
    root: Path,
    repo: Path,
    *,
    arguments: list[str] | None = None,
    build_mode: str = "success",
    codex_exit: int = 0,
    mutate_lock: bool = False,
    extra_env: dict[str, str] | None = None,
    force_fallback_locks: bool = False,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["sh", str(repo / "scripts/install/install-local.sh"), *(arguments or [])],
        cwd=repo,
        env=installer_env(
            root,
            repo,
            build_mode=build_mode,
            codex_exit=codex_exit,
            mutate_lock=mutate_lock,
            extra_env=extra_env,
            force_fallback_locks=force_fallback_locks,
        ),
        text=True,
        capture_output=True,
        check=False,
    )


def installer_env(
    root: Path,
    repo: Path,
    *,
    build_mode: str = "success",
    codex_exit: int = 0,
    mutate_lock: bool = False,
    extra_env: dict[str, str] | None = None,
    force_fallback_locks: bool = False,
) -> dict[str, str]:
    fake_bin = root / "fake-bin"
    fake_bin.mkdir(exist_ok=True)
    home = root / "home"
    home.mkdir(exist_ok=True)
    write_executable(fake_bin / "cargo", "#!/bin/sh\nexit 0\n")
    write_executable(
        fake_bin / "uname",
        '#!/bin/sh\ncase "$1" in -s) echo Linux;; -m) echo x86_64;; *) echo Linux;; esac\n',
    )
    if force_fallback_locks:
        for command in (
            "awk",
            "basename",
            "cat",
            "chmod",
            "cmp",
            "cp",
            "dirname",
            "find",
            "git",
            "grep",
            "head",
            "ln",
            "mkdir",
            "mktemp",
            "mv",
            "python3",
            "readlink",
            "rm",
            "sed",
            "sh",
            "sleep",
            "sort",
            "tr",
        ):
            command_path = shutil.which(command)
            assert command_path is not None
            (fake_bin / command).symlink_to(command_path)
    write_executable(
        fake_bin / "date",
        '#!/bin/sh\ncase "$1" in '
        "+%Y%m%d%H%M%S) echo 20260825120000;; "
        "+%s) echo 1787659200;; "
        '*) exec /bin/date "$@";; esac\n',
    )
    env = {
        **os.environ,
        "PATH": str(fake_bin) if force_fallback_locks else f"{fake_bin}:/usr/bin:/bin",
        "HOME": str(home),
        "SHELL": "/bin/sh",
        "CODEX_HOME": str(root / "codex-home"),
        "CODEX_INSTALL_DIR": str(root / "install-bin"),
        "CODEX_TEST_BUILD_LOG": str(root / "build.json"),
        "CODEX_TEST_BUILD_MODE": build_mode,
        "CODEX_TEST_CODEX_EXIT": str(codex_exit),
        "CODEX_TEST_MUTATE_LOCK": "1" if mutate_lock else "0",
        "CODEX_TEST_READY": str(root / "build.ready"),
        "TMPDIR": str(root),
        "CARGO_PROFILE_DEV_DEBUG_ASSERTIONS": "caller-value",
    }
    env.pop("CODEX_REPO_ROOT", None)
    env.pop("CODEX_UPSTREAM_VERSION", None)
    env.update(extra_env or {})
    return env


def prepare_previous_releases(root: Path, *, timestamped: bool = False) -> Path:
    releases_dir = root / "codex-home/packages/standalone/releases"
    releases_dir.mkdir(parents=True)
    for index in range(1, 4):
        name = (
            f"{RELEASE_PREFIX}-2026082{index}120000-{index}"
            if timestamped
            else f"previous-{index}"
        )
        (releases_dir / name).mkdir()
    return releases_dir


def version_transaction_dir(repo: Path) -> Path:
    return Path(
        git(
            repo,
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "codex-local-version/transaction",
        ).stdout.strip()
    )


def version_lock_path(repo: Path) -> Path:
    return version_transaction_dir(repo).parent / "version.lock.d"


def read_build_log(root: Path) -> dict[str, object]:
    return json.loads((root / "build.json").read_text(encoding="utf-8"))


def wait_for_path(path: Path) -> None:
    deadline = time.monotonic() + 5
    while not path.exists():
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {path}")
        time.sleep(0.01)


def wait_for_any_path(paths: list[Path]) -> None:
    deadline = time.monotonic() + 5
    while not any(path.exists() for path in paths):
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for one of {paths}")
        time.sleep(0.01)


def wait_for_glob_count(pattern: str, count: int) -> None:
    deadline = time.monotonic() + 5
    parent = Path(pattern).parent
    name = Path(pattern).name
    while len(list(parent.glob(name))) < count:
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {count} matches of {pattern}")
        time.sleep(0.01)


def install_reclaim_guard_barrier(env: dict[str, str]) -> None:
    fake_ln = Path(env["PATH"]) / "ln"
    fake_ln.unlink()
    write_executable(
        fake_ln,
        textwrap.dedent(
            """\
            #!/bin/sh
            last=""
            for argument in "$@"; do last="$argument"; done
            case "$last" in
            *.reclaim.guard)
              while [ ! -e "$CODEX_TEST_RECLAIM_CONTINUE" ]; do
                sleep 0.01
              done
              ;;
            esac
            exec /usr/bin/ln "$@"
            """
        ),
    )


def install_guard_unlink_successor_signal(env: dict[str, str]) -> None:
    fake_rm = Path(env["PATH"]) / "rm"
    fake_rm.unlink()
    write_executable(
        fake_rm,
        textwrap.dedent(
            """\
            #!/bin/sh
            last=""
            for argument in "$@"; do last="$argument"; done
            case "$last" in
            *.reclaim.guard)
              if mkdir "$TMPDIR/guard-unlink-once" 2>/dev/null; then
                /usr/bin/rm "$@"
                {
                  printf '%s\n' "$$"
                  date +%s
                  printf '%s\n' 'fingerprint=successor'
                } >"$CODEX_TEST_SUCCESSOR_MARKER"
                /usr/bin/ln "$CODEX_TEST_SUCCESSOR_MARKER" "$last"
                kill -TERM "$PPID"
                exit 0
              fi
              ;;
            esac
            exec /usr/bin/rm "$@"
            """
        ),
    )


def communicate_bounded(process: subprocess.Popen[str]) -> tuple[str, str]:
    try:
        return process.communicate(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate(timeout=2)
        raise AssertionError(
            f"local installer did not fail closed promptly: {stderr}{stdout}"
        )


def write_workspace_version(path: Path, workspace_version: str) -> None:
    path.write_bytes(
        textwrap.dedent(
            f'''\
            [workspace]
            resolver = "2"

            [workspace.package]
            version = "{workspace_version}"
            edition = "2024"
            '''
        )
        .replace("\n", "\r\n")
        .encode()
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


def write_executable(path: Path, content: str) -> None:
    path.write_text(content, encoding="utf-8")
    path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
