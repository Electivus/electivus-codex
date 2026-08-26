#!/usr/bin/env python3

import hashlib
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


BOOTSTRAP = Path(__file__).with_name("installer-v1.sh")
PACKAGE_ASSETS = (
    "codex-package-aarch64-pc-windows-msvc.tar.gz",
    "codex-package-aarch64-unknown-linux-musl.tar.gz",
    "codex-package-x86_64-pc-windows-msvc.tar.gz",
    "codex-package-x86_64-unknown-linux-musl.tar.gz",
)
REQUIRED_ASSETS = (
    *PACKAGE_ASSETS,
    "codex-package_SHA256SUMS",
    "install.sh",
    "install.ps1",
    "installer_SHA256SUMS",
)


class InstallerV1ShTest(unittest.TestCase):
    def test_help_identifies_protocol_without_network(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result, requests = run_bootstrap(Path(temp_dir), arguments=["--help"])

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Electivus Installer protocol v1", result.stdout)
            self.assertEqual(requests, [])

    def test_stable_default_selects_greatest_complete_stable_and_delegates_exactly(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_assets(root)
            inventory = [
                release("9.0.0-alpha.1", digests),
                release("1.9.0", digests),
                release("2.0.0", digests),
                release("99.0.0", digests, draft=True),
                release("3.0.0", digests, omit={"install.ps1"}),
            ]

            result, requests = run_bootstrap(root, inventory=inventory)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                read_delegation(root),
                {
                    "arguments": [
                        "--release",
                        "2.0.0",
                        "--channel",
                        "stable",
                        "--installer-protocol",
                        "installer-v1",
                        "--installer-digest",
                        digests["install.sh"],
                    ],
                    "release": "2.0.0",
                    "channel": "stable",
                    "protocol": "installer-v1",
                    "installer_digest": digests["install.sh"],
                    "non_interactive": "1",
                },
            )
            self.assertEqual(
                requests,
                [
                    inventory_url(1),
                    asset_url("2.0.0", "installer_SHA256SUMS"),
                    asset_url("2.0.0", "install.sh"),
                ],
            )
            assert_fork_only_requests(requests)

    def test_stable_absence_reports_explicit_prerelease_invocation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_assets(root)

            result, requests = run_bootstrap(
                root,
                inventory=[release("2.0.0-alpha.1", digests)],
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn(
                "installer-v1.sh --release pre-release",
                result.stderr,
            )
            self.assertEqual(requests, [inventory_url(1)])
            self.assertFalse((root / "delegation.json").exists())

    def test_prerelease_uses_full_numeric_semver_precedence(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_assets(root)
            inventory = [
                release("2.0.0-alpha.9", digests),
                release("2.0.0-alpha.12", digests),
                release("1.0.0", digests),
            ]

            result, requests = run_bootstrap(
                root,
                arguments=["--release", "pre-release"],
                inventory=inventory,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(read_delegation(root)["release"], "2.0.0-alpha.12")
            self.assertEqual(read_delegation(root)["channel"], "pre-release")
            self.assertEqual(
                requests[-1],
                asset_url("2.0.0-alpha.12", "install.sh"),
            )
            assert_fork_only_requests(requests)

    def test_bare_and_tag_exact_selectors_use_the_same_exact_release(self) -> None:
        for selector in ("3.4.5", "electivus-v3.4.5"):
            with (
                self.subTest(selector=selector),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_assets(root)
                metadata = release("3.4.5", digests)

                result, requests = run_bootstrap(
                    root,
                    arguments=["--release", selector],
                    exact=metadata,
                )

                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(read_delegation(root)["release"], "3.4.5")
                self.assertEqual(
                    requests,
                    [
                        exact_url("3.4.5"),
                        asset_url("3.4.5", "installer_SHA256SUMS"),
                        asset_url("3.4.5", "install.sh"),
                    ],
                )
                assert_fork_only_requests(requests)

    def test_invalid_draft_malformed_and_incomplete_releases_fail_closed(self) -> None:
        cases: tuple[tuple[str, object], ...] = (
            ("draft", None),
            ("malformed-json", '{"tag_name":'),
            ("incomplete", None),
            ("wrong-tag", None),
        )
        for case, raw_metadata in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_assets(root)
                if raw_metadata is not None:
                    metadata = raw_metadata
                elif case == "draft":
                    metadata = release("4.0.0", digests, draft=True)
                elif case == "incomplete":
                    metadata = release("4.0.0", digests, omit={"install.ps1"})
                else:
                    metadata = release("4.0.1", digests)

                result, requests = run_bootstrap(
                    root,
                    arguments=["--release", "4.0.0"],
                    exact=metadata,
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [exact_url("4.0.0")])
                self.assertFalse((root / "delegation.json").exists())
                assert_fork_only_requests(requests)

        for selector in ("latest", "rust-v4.0.0", "v4.0.0", "0.0.0", "4.0"):
            with (
                self.subTest(selector=selector),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, requests = run_bootstrap(
                    Path(temp_dir),
                    arguments=["--release", selector],
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [])

    def test_duplicate_release_keys_fail_closed_in_exact_and_inventory_metadata(
        self,
    ) -> None:
        duplicate_values: dict[str, object] = {
            "draft": True,
            "tag_name": "electivus-v999.0.0",
            "assets": [],
        }
        for key, duplicate_value in duplicate_values.items():
            for route in ("exact", "inventory"):
                with (
                    self.subTest(key=key, route=route),
                    tempfile.TemporaryDirectory() as temp_dir,
                ):
                    root = Path(temp_dir)
                    digests = create_assets(root)
                    raw_release = release_json_with_duplicate_key(
                        release("4.1.0", digests), key, duplicate_value
                    )
                    options: dict[str, object]
                    if route == "exact":
                        options = {
                            "arguments": ["--release", "4.1.0"],
                            "exact": raw_release,
                        }
                        expected_requests = [exact_url("4.1.0")]
                    else:
                        options = {"inventory": f"[{raw_release}]"}
                        expected_requests = [inventory_url(1)]

                    result, requests = run_bootstrap(root, **options)

                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(requests, expected_requests)
                    self.assertFalse((root / "delegation.json").exists())

    def test_release_selectors_reject_cr_and_lf_before_network(self) -> None:
        for selector in (
            "1.2.3\n",
            "electivus-v1.2.3\r",
            "stable\n",
            "pre-release\r\n",
        ):
            with (
                self.subTest(selector=repr(selector)),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, requests = run_bootstrap(
                    Path(temp_dir), arguments=["--release", selector]
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must not contain CR or LF", result.stderr)
                self.assertEqual(requests, [])

    def test_metadata_and_installer_body_limits_fail_before_delegation(self) -> None:
        for mode, expected_limit in (
            ("metadata-oversized", "1048576-byte safety limit"),
            ("installer-oversized", "4194304-byte safety limit"),
        ):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                installer = b"x" * 4194305 if mode == "installer-oversized" else None
                digests = create_assets(root, installer=installer)

                result, requests = run_bootstrap(
                    root,
                    arguments=["--release", "5.0.0"],
                    exact=release("5.0.0", digests),
                    mode=mode,
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected_limit, result.stderr)
                self.assertFalse((root / "delegation.json").exists())
                assert_fork_only_requests(requests)

    def test_curl_stops_a_slow_unknown_length_stream_near_the_cap(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)

            started = time.monotonic()
            result, requests = run_bootstrap(
                root,
                arguments=["--release", "5.0.1"],
                mode="metadata-slow-oversized",
            )
            elapsed = time.monotonic() - started

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(requests, [exact_url("5.0.1")])
            self.assertIn("1048576-byte safety limit", result.stderr)
            self.assertLess(elapsed, 5)
            self.assertLessEqual(
                int((root / "curl-streamed-bytes").read_text(encoding="utf-8")),
                1_114_112,
            )
            wait_for_process_exit(
                int((root / "curl-stream-pid").read_text(encoding="utf-8"))
            )

    def test_signals_stop_blocked_curl_without_delegation_or_leaks(self) -> None:
        for sent_signal in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
            with (
                self.subTest(signal=sent_signal),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_assets(root)
                args, env, _request_log = prepare_bootstrap(
                    root,
                    arguments=["--release", "5.0.2"],
                    exact=release("5.0.2", digests),
                    mode="metadata-blocked",
                )
                process = subprocess.Popen(
                    args,
                    env=env,
                    start_new_session=True,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                try:
                    wait_for_path(root / "download.ready")
                    worker_pids = recorded_download_pids(root)
                    started = time.monotonic()
                    os.kill(process.pid, sent_signal)
                    stdout, stderr = communicate_bounded(process)
                    elapsed = time.monotonic() - started

                    self.assertEqual(
                        process.returncode,
                        128 + sent_signal,
                        stderr + stdout,
                    )
                    self.assertLess(elapsed, 2)
                    for worker_pid in worker_pids:
                        wait_for_process_exit(worker_pid)
                    wait_for_process_group_exit(process.pid)
                    self.assertFalse((root / "delegation.json").exists())
                    leaked = [
                        path
                        for pattern in ("tmp.*", "**/*.fifo")
                        for path in root.glob(pattern)
                    ]
                    self.assertEqual(leaked, [])
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=2)

    def test_signals_stop_and_reap_a_blocked_delegate_before_cleanup(self) -> None:
        for sent_signal, expected_name in (
            (signal.SIGHUP, "HUP"),
            (signal.SIGINT, "INT"),
            (signal.SIGTERM, "TERM"),
        ):
            with (
                self.subTest(signal=sent_signal),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_assets(
                    root,
                    installer=BLOCKING_INSTALLER.encode(),
                )
                args, env, _request_log = prepare_bootstrap(
                    root,
                    arguments=["--release", "5.0.3"],
                    exact=release("5.0.3", digests),
                )
                process = subprocess.Popen(
                    args,
                    env=env,
                    start_new_session=True,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                delegate_pid: int | None = None
                try:
                    wait_for_path(root / "delegate.ready")
                    delegate_pid = int(
                        (root / "delegate.pid").read_text(encoding="utf-8")
                    )
                    delegated_installer = Path(
                        (root / "delegate.path").read_text(encoding="utf-8").strip()
                    )
                    self.assertTrue(delegated_installer.is_file())

                    started = time.monotonic()
                    os.kill(process.pid, sent_signal)
                    stdout, stderr = communicate_bounded(
                        process,
                        additional_process_groups=(delegate_pid,),
                    )
                    elapsed = time.monotonic() - started

                    self.assertEqual(
                        process.returncode,
                        128 + sent_signal,
                        stderr + stdout,
                    )
                    self.assertLess(elapsed, 2)
                    self.assertEqual(
                        (root / "delegate.signal").read_text(encoding="utf-8"),
                        f"{expected_name}\n",
                    )
                    self.assertEqual(
                        (root / "delegate.path-state").read_text(encoding="utf-8"),
                        "present\n",
                    )
                    wait_for_process_exit(delegate_pid)
                    wait_for_process_group_exit(delegate_pid)
                    self.assertFalse(delegated_installer.exists())
                    self.assertEqual(list(root.glob("tmp.*")), [])
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                    if delegate_pid is not None:
                        try:
                            os.killpg(delegate_pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    if process.poll() is None:
                        process.communicate(timeout=2)

    def test_metadata_manifest_and_downloaded_installer_digests_must_agree(
        self,
    ) -> None:
        for case in ("metadata-disagreement", "corrupt-manifest", "corrupt-installer"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_assets(root)
                metadata_digests = dict(digests)
                if case == "metadata-disagreement":
                    metadata_digests["install.sh"] = "0" * 64
                elif case == "corrupt-manifest":
                    (root / "assets/installer_SHA256SUMS").write_text(
                        "corrupt\n",
                        encoding="utf-8",
                    )
                else:
                    (root / "assets/install.sh").write_bytes(b"corrupt\n")

                result, requests = run_bootstrap(
                    root,
                    arguments=["--release", "6.0.0"],
                    exact=release("6.0.0", metadata_digests),
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((root / "delegation.json").exists())
                if case == "metadata-disagreement":
                    self.assertIn("digest disagreement for install.sh", result.stderr)
                    self.assertNotIn(asset_url("6.0.0", "install.sh"), requests)
                else:
                    self.assertIn("SHA-256 mismatch", result.stderr)
                assert_fork_only_requests(requests)

    def test_inventory_page_and_platform_bounds_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_assets(root)
            full_page = [release(f"7.0.{index}", digests) for index in range(100)]

            result, requests = run_bootstrap(root, inventory=full_page)

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("4-page safety limit", result.stderr)
            self.assertEqual(requests, [inventory_url(page) for page in range(1, 5)])

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_assets(root)

            result, requests = run_bootstrap(
                root,
                inventory=[release(f"7.1.{index}", digests) for index in range(101)],
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("exceeds 100 releases", result.stderr)
            self.assertEqual(requests, [inventory_url(1)])

        with tempfile.TemporaryDirectory() as temp_dir:
            result, requests = run_bootstrap(Path(temp_dir), operating_system="Darwin")

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("does not support macOS", result.stderr)
            self.assertEqual(requests, [])

    def test_duplicate_release_tags_fail_closed_at_any_inventory_position(
        self,
    ) -> None:
        for position in ("same-page-identical", "same-page-conflicting", "later-page"):
            with (
                self.subTest(position=position),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_assets(root)
                metadata = release("7.1.1", digests)
                duplicate = release("7.1.1", digests)
                if position != "same-page-identical":
                    duplicate["draft"] = True
                if position == "later-page":
                    pages = [[metadata, *({} for _ in range(99))], [duplicate]]
                    expected_requests = [inventory_url(1), inventory_url(2)]
                else:
                    pages = [[metadata, duplicate]]
                    expected_requests = [inventory_url(1)]

                result, requests = run_bootstrap(root, inventory_pages=pages)

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, expected_requests)
                self.assertIn("duplicate release tag in inventory", result.stderr)
                self.assertFalse((root / "delegation.json").exists())

    def test_release_version_byte_bound_is_enforced_before_network(self) -> None:
        maximum_version = "1.2.3+" + "a" * 122
        self.assertEqual(len(maximum_version.encode()), 128)
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_assets(root)

            accepted, _requests = run_bootstrap(
                root,
                arguments=["--release", maximum_version],
                exact=release(maximum_version, digests),
            )

            self.assertEqual(accepted.returncode, 0, accepted.stderr)

        with tempfile.TemporaryDirectory() as temp_dir:
            rejected, requests = run_bootstrap(
                Path(temp_dir), arguments=["--release", maximum_version + "a"]
            )

            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("128-byte safety limit", rejected.stderr)
            self.assertEqual(requests, [])

    def test_asset_state_size_count_and_name_bounds_fail_closed(self) -> None:
        cases = (
            "state",
            "zero-size",
            "oversized-package",
            "too-many",
            "long-name",
            "long-multibyte-name",
        )
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_assets(root)
                metadata = release("7.2.0", digests)
                assets = metadata["assets"]
                assert isinstance(assets, list)
                if case == "state":
                    assets[0]["state"] = "new"
                elif case == "zero-size":
                    assets[0]["size"] = 0
                elif case == "oversized-package":
                    assets[0]["size"] = 1_073_741_825
                elif case == "too-many":
                    assets.extend(valid_extra_assets(57))
                elif case == "long-name":
                    assets.extend(valid_extra_assets(1, final_name="x" * 257))
                else:
                    assets.extend(valid_extra_assets(1, final_name="é" * 129))

                result, requests = run_bootstrap(
                    root, arguments=["--release", "7.2.0"], exact=metadata
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [exact_url("7.2.0")])
                self.assertFalse((root / "delegation.json").exists())

    def test_asset_count_name_and_type_size_upper_boundaries_are_valid(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_assets(root)
            metadata = release("7.2.1", digests)
            assets = metadata["assets"]
            assert isinstance(assets, list)
            for asset in assets:
                name = asset["name"]
                if str(name).startswith("codex-package-") and str(name).endswith(
                    ".tar.gz"
                ):
                    asset["size"] = 1_073_741_824
                elif name in {"install.sh", "install.ps1"}:
                    asset["size"] = 4_194_304
                else:
                    asset["size"] = 1_048_576
            assets.extend(valid_extra_assets(55, final_name="x" * 256))
            assets.extend(valid_extra_assets(1, final_name="é" * 128))

            result, _requests = run_bootstrap(
                root, arguments=["--release", "7.2.1"], exact=metadata
            )

            self.assertEqual(result.returncode, 0, result.stderr)

    def test_published_at_requires_a_semantic_rfc3339_timestamp(self) -> None:
        invalid_values: tuple[object, ...] = (
            None,
            1,
            "not-a-timestamp",
            "2026-02-30T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-08-25T24:00:00Z",
            "2026-08-25T00:00:00+24:00",
        )
        for value in invalid_values:
            with (
                self.subTest(value=value),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_assets(root)
                metadata = release("7.2.2", digests)
                metadata["published_at"] = value

                result, requests = run_bootstrap(
                    root,
                    arguments=["--release", "7.2.2"],
                    exact=metadata,
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [exact_url("7.2.2")])
                self.assertFalse((root / "delegation.json").exists())

        valid_values = (
            "2026-08-25T00:00:00.123Z",
            "2026-08-25T03:00:00+03:00",
        )
        for value in valid_values:
            with (
                self.subTest(value=value),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_assets(root)
                metadata = release("7.2.2", digests)
                metadata["published_at"] = value

                result, _requests = run_bootstrap(
                    root,
                    arguments=["--release", "7.2.2"],
                    exact=metadata,
                )

                self.assertEqual(result.returncode, 0, result.stderr)


def prepare_bootstrap(
    root: Path,
    *,
    arguments: list[str] | None = None,
    inventory: list[dict[str, object]] | str | None = None,
    inventory_pages: list[object] | None = None,
    exact: object | None = None,
    mode: str = "",
    operating_system: str = "Linux",
) -> tuple[list[str], dict[str, str], Path]:
    fake_bin = root / "fake-bin"
    fake_bin.mkdir(parents=True, exist_ok=True)
    metadata_dir = root / "metadata-fixtures"
    metadata_dir.mkdir(exist_ok=True)
    (metadata_dir / "exact.json").write_text(
        exact if isinstance(exact, str) else json.dumps(exact or {}),
        encoding="utf-8",
    )
    (metadata_dir / "inventory.json").write_text(
        inventory if isinstance(inventory, str) else json.dumps(inventory or []),
        encoding="utf-8",
    )
    if inventory_pages is not None:
        (metadata_dir / "pages.enabled").touch()
        for page_number, page in enumerate(inventory_pages, start=1):
            (metadata_dir / f"page-{page_number}.json").write_text(
                page if isinstance(page, str) else json.dumps(page),
                encoding="utf-8",
            )
    request_log = root / "requests.log"
    write_executable(
        fake_bin / "curl",
        textwrap.dedent(
            """\
            #!/bin/sh
            url=""
            for argument in "$@"; do
              case "$argument" in https://*) url="$argument" ;; esac
            done
            printf '%s\n' "$url" >>"$CODEX_TEST_REQUEST_LOG"
            case "$url" in
              *openai*|*/main/*) exit 88 ;;
              https://api.github.com/repos/Electivus/electivus-codex/releases/tags/*)
                if [ "$CODEX_TEST_MODE" = "metadata-blocked" ]; then
                  printf '%s\n' "$$" >"$CODEX_TEST_ROOT/downloader.pid"
                  : >"$CODEX_TEST_ROOT/download.ready"
                  while :; do sleep 0.1; done
                elif [ "$CODEX_TEST_MODE" = "metadata-slow-oversized" ]; then
                  printf '%s\n' "$$" >"$CODEX_TEST_ROOT/curl-stream-pid"
                  streamed=0
                  trap 'printf "%s\n" "$streamed" >"$CODEX_TEST_ROOT/curl-streamed-bytes"; exit 0' TERM INT PIPE
                  while :; do
                    dd if=/dev/zero bs=65536 count=1 2>/dev/null | tr '\\000' x || break
                    streamed=$((streamed + 65536))
                    printf '%s\n' "$streamed" >"$CODEX_TEST_ROOT/curl-streamed-bytes"
                    sleep 0.01
                  done
                  exit 0
                elif [ "$CODEX_TEST_MODE" = "metadata-oversized" ]; then
                  dd if=/dev/zero bs=1048577 count=1 2>/dev/null | tr '\\000' x
                else
                  cat "$CODEX_TEST_METADATA_DIR/exact.json"
                fi
                ;;
              'https://api.github.com/repos/Electivus/electivus-codex/releases?per_page=100&page='*)
                page="${url##*page=}"
                if [ -f "$CODEX_TEST_METADATA_DIR/pages.enabled" ]; then
                  if [ -f "$CODEX_TEST_METADATA_DIR/page-$page.json" ]; then
                    cat "$CODEX_TEST_METADATA_DIR/page-$page.json"
                  else
                    printf '[]\n'
                  fi
                else
                  cat "$CODEX_TEST_METADATA_DIR/inventory.json"
                fi
                ;;
              https://github.com/Electivus/electivus-codex/releases/download/*)
                cat "$CODEX_TEST_ASSETS/${url##*/}"
                ;;
              *) exit 89 ;;
            esac
            """
        ),
    )
    real_head = shutil.which("head")
    assert real_head is not None
    write_executable(
        fake_bin / "head",
        textwrap.dedent(
            f"""\
            #!/bin/sh
            printf '%s\\n' "$$" >>"$CODEX_TEST_ROOT/head.pids"
            exec "{real_head}" "$@"
            """
        ),
    )
    write_executable(
        fake_bin / "uname", f"#!/bin/sh\nprintf '%s\\n' '{operating_system}'\n"
    )
    home = root / "home"
    home.mkdir(exist_ok=True)
    env = {
        **os.environ,
        "CODEX_TEST_ASSETS": str(root / "assets"),
        "CODEX_TEST_DELEGATION": str(root / "delegation.json"),
        "CODEX_TEST_METADATA_DIR": str(metadata_dir),
        "CODEX_TEST_MODE": mode,
        "CODEX_TEST_REQUEST_LOG": str(request_log),
        "CODEX_TEST_ROOT": str(root),
        "HOME": str(home),
        "PATH": f"{fake_bin}:/usr/bin:/bin",
        "TMPDIR": str(root),
    }
    env.pop("CODEX_RELEASE", None)
    return ["/bin/sh", str(BOOTSTRAP), *(arguments or [])], env, request_log


def run_bootstrap(
    root: Path,
    **options: object,
) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    args, env, request_log = prepare_bootstrap(root, **options)
    result = subprocess.run(
        args,
        capture_output=True,
        check=False,
        env=env,
        text=True,
    )
    requests = (
        request_log.read_text(encoding="utf-8").splitlines()
        if request_log.exists()
        else []
    )
    return result, requests


def create_assets(root: Path, *, installer: bytes | None = None) -> dict[str, str]:
    assets = root / "assets"
    assets.mkdir()
    installer_path = assets / "install.sh"
    installer_path.write_bytes(installer or DELEGATING_INSTALLER.encode())
    (assets / "install.ps1").write_text("# installer fixture\n", encoding="utf-8")
    installer_digests = {
        name: sha256(assets / name) for name in ("install.sh", "install.ps1")
    }
    manifest = assets / "installer_SHA256SUMS"
    manifest.write_text(
        "".join(
            f"{installer_digests[name]}  {name}\n"
            for name in ("install.sh", "install.ps1")
        ),
        encoding="utf-8",
    )
    digests = {
        name: hashlib.sha256(name.encode()).hexdigest() for name in PACKAGE_ASSETS
    }
    digests["codex-package_SHA256SUMS"] = hashlib.sha256(b"packages").hexdigest()
    return {
        **digests,
        **installer_digests,
        "installer_SHA256SUMS": sha256(manifest),
    }


DELEGATING_INSTALLER = """#!/bin/sh
python3 - "$@" <<'PY'
import json
import os
from pathlib import Path
import sys

Path(os.environ["CODEX_TEST_DELEGATION"]).write_text(
    json.dumps(
        {
            "arguments": sys.argv[1:],
            "release": os.environ.get("CODEX_RELEASE"),
            "channel": os.environ.get("CODEX_UPDATE_CHANNEL"),
            "protocol": os.environ.get("CODEX_INSTALLER_PROTOCOL"),
            "installer_digest": os.environ.get("CODEX_INSTALLER_DIGEST"),
            "non_interactive": os.environ.get("CODEX_NON_INTERACTIVE"),
        }
    ),
    encoding="utf-8",
)
PY
"""


BLOCKING_INSTALLER = """#!/bin/sh
handle_signal() {
  printf '%s\n' "$1" >"$CODEX_TEST_ROOT/delegate.signal"
  if [ -f "$0" ]; then
    printf 'present\n' >"$CODEX_TEST_ROOT/delegate.path-state"
  else
    printf 'missing\n' >"$CODEX_TEST_ROOT/delegate.path-state"
  fi
  exit 0
}
trap 'handle_signal HUP' HUP
trap 'handle_signal INT' INT
trap 'handle_signal TERM' TERM
printf '%s\n' "$$" >"$CODEX_TEST_ROOT/delegate.pid"
printf '%s\n' "$0" >"$CODEX_TEST_ROOT/delegate.path"
: >"$CODEX_TEST_ROOT/delegate.ready"
while :; do
  sleep 1
done
"""


def release(
    version: str,
    digests: dict[str, str],
    *,
    draft: bool = False,
    omit: set[str] | None = None,
) -> dict[str, object]:
    omitted = omit or set()
    return {
        "tag_name": f"electivus-v{version}",
        "draft": draft,
        "prerelease": "-" in version.split("+", 1)[0],
        "published_at": "2026-08-25T00:00:00Z",
        "assets": [
            {
                "name": name,
                "digest": f"sha256:{digests[name]}",
                "state": "uploaded",
                "size": 1,
            }
            for name in REQUIRED_ASSETS
            if name not in omitted
        ],
    }


def release_json_with_duplicate_key(
    metadata: dict[str, object], key: str, duplicate_value: object
) -> str:
    serialized = json.dumps(metadata, separators=(",", ":"))
    return (
        "{"
        + json.dumps(key)
        + ":"
        + json.dumps(duplicate_value, separators=(",", ":"))
        + ","
        + serialized[1:]
    )


def valid_extra_assets(
    count: int, *, final_name: str | None = None
) -> list[dict[str, object]]:
    names = [f"extra-{index}" for index in range(count)]
    if final_name is not None:
        names[-1] = final_name
    return [
        {
            "name": name,
            "digest": "sha256:" + hashlib.sha256(name.encode()).hexdigest(),
            "state": "uploaded",
            "size": 1,
        }
        for name in names
    ]


def read_delegation(root: Path) -> dict[str, object]:
    return json.loads((root / "delegation.json").read_text(encoding="utf-8"))


def assert_fork_only_requests(requests: list[str]) -> None:
    for request in requests:
        if "openai" in request.lower() or "/main/" in request:
            raise AssertionError(f"non-Electivus or mutable request: {request}")


def wait_for_process_exit(pid: int) -> None:
    deadline = time.monotonic() + 2
    while True:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        if time.monotonic() >= deadline:
            raise AssertionError(f"process {pid} remained alive")
        time.sleep(0.01)


def wait_for_process_group_exit(process_group: int) -> None:
    deadline = time.monotonic() + 2
    while True:
        try:
            os.killpg(process_group, 0)
        except ProcessLookupError:
            return
        proc_root = Path("/proc")
        if proc_root.is_dir():
            group_states: list[bytes] = []
            proc_read_failed = False
            for stat_path in proc_root.glob("[0-9]*/stat"):
                try:
                    stat_fields = stat_path.read_bytes().rpartition(b") ")[2].split()
                    state = stat_fields[0]
                    member_process_group = int(stat_fields[2])
                except FileNotFoundError:
                    continue
                except (IndexError, OSError, ValueError):
                    proc_read_failed = True
                    continue
                if member_process_group == process_group:
                    group_states.append(state)
            if (
                not proc_read_failed
                and group_states
                and all(state == b"Z" for state in group_states)
            ):
                return
        if time.monotonic() >= deadline:
            raise AssertionError(f"process group {process_group} remained alive")
        time.sleep(0.01)


def recorded_download_pids(root: Path) -> list[int]:
    pids = [int((root / "downloader.pid").read_text(encoding="utf-8"))]
    pids.extend(
        int(pid)
        for pid in (root / "head.pids").read_text(encoding="utf-8").splitlines()
    )
    return pids


def communicate_bounded(
    process: subprocess.Popen[str],
    *,
    additional_process_groups: tuple[int, ...] = (),
) -> tuple[str, str]:
    try:
        return process.communicate(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        for process_group in additional_process_groups:
            try:
                os.killpg(process_group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        stdout, stderr = process.communicate(timeout=2)
        raise AssertionError(
            f"bootstrap did not exit promptly after a direct signal: {stderr}{stdout}"
        )


def wait_for_path(path: Path) -> None:
    deadline = time.monotonic() + 5
    while not path.exists():
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {path}")
        time.sleep(0.01)


def inventory_url(page: int) -> str:
    return (
        "https://api.github.com/repos/Electivus/electivus-codex/"
        f"releases?per_page=100&page={page}"
    )


def exact_url(version: str) -> str:
    return (
        "https://api.github.com/repos/Electivus/electivus-codex/releases/tags/"
        f"electivus-v{version}"
    )


def asset_url(version: str, asset: str) -> str:
    return (
        "https://github.com/Electivus/electivus-codex/releases/download/"
        f"electivus-v{version}/{asset}"
    )


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_executable(path: Path, content: str) -> None:
    path.write_text(content, encoding="utf-8")
    path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
