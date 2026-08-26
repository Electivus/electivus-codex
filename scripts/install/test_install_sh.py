#!/usr/bin/env python3

import hashlib
import json
import os
import signal
from dataclasses import dataclass
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import textwrap
import time
import unittest


INSTALL_SCRIPT = Path(__file__).with_name("install.sh")
REPOSITORY = "Electivus/electivus-codex"
TARGET = "x86_64-unknown-linux-musl"
PACKAGE_ASSETS = (
    "codex-package-aarch64-pc-windows-msvc.tar.gz",
    "codex-package-aarch64-unknown-linux-musl.tar.gz",
    "codex-package-x86_64-pc-windows-msvc.tar.gz",
    f"codex-package-{TARGET}.tar.gz",
)
REQUIRED_ASSETS = (
    *PACKAGE_ASSETS,
    "codex-package_SHA256SUMS",
    "install.sh",
    "install.ps1",
    "installer_SHA256SUMS",
)


class InstallShTest(unittest.TestCase):
    def test_stable_default_selects_greatest_complete_release(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "1.10.0")
            inventory = [
                release_metadata("2.0.0-alpha.1", digests),
                release_metadata("1.2.0", digests),
                release_metadata("1.10.0", digests),
                release_metadata("3.0.0", digests, draft=True),
                release_metadata("4.0.0", digests, omit={"install.ps1"}),
                release_metadata("invalid", digests),
            ]

            result, requests = run_installer(root, selector=None, inventory=inventory)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Resolved version: 1.10.0", result.stdout)
            self.assertTrue(
                all("Electivus/electivus-codex" in request for request in requests)
            )
            self.assertTrue(
                all("openai" not in request.lower() for request in requests)
            )
            receipt = read_receipt(root, "1.10.0")
            self.assertEqual(
                receipt,
                {
                    "publisher": "Electivus",
                    "repository": REPOSITORY,
                    "tag": "electivus-v1.10.0",
                    "update_channel": "stable",
                    "target": TARGET,
                    "package_digest": digests[f"codex-package-{TARGET}.tar.gz"],
                    "installer_digest": digests["install.sh"],
                    "installer_protocol": "direct",
                },
            )

    def test_pre_release_channel_uses_full_semver_precedence(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "2.0.0-rc.1")
            inventory = [
                release_metadata("2.0.0-beta.11", digests),
                release_metadata("2.0.0-alpha.9", digests),
                release_metadata("2.0.0-alpha.10", digests),
                release_metadata("2.0.0-alpha.beta", digests),
                release_metadata("2.0.0-beta.2", digests),
                release_metadata("2.0.0-rc.1", digests),
                release_metadata("1.99.0", digests),
            ]

            result, _requests = run_installer(
                root, selector="pre-release", inventory=inventory
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Resolved version: 2.0.0-rc.1", result.stdout)

    def test_stable_channel_without_stable_release_gives_opt_in_command(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "2.0.0-alpha.1")

            result, requests = run_installer(
                root, inventory=[release_metadata("2.0.0-alpha.1", digests)]
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(len(requests), 1)
            self.assertIn("install.sh --release pre-release", result.stderr)

    def test_bare_and_electivus_tag_inputs_resolve_the_same_exact_release(self) -> None:
        for selector in ("1.4.2", "electivus-v1.4.2"):
            with (
                self.subTest(selector=selector),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_release_assets(root, "1.4.2")
                exact = release_metadata("1.4.2", digests)

                result, requests = run_installer(root, selector=selector, exact=exact)

                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    requests[0],
                    "https://api.github.com/repos/Electivus/electivus-codex/"
                    "releases/tags/electivus-v1.4.2",
                )
                self.assertEqual(read_receipt(root, "1.4.2")["tag"], "electivus-v1.4.2")

    def test_exact_bootstrap_delegation_persists_channel_and_protocol(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "1.4.3")

            result, _requests = run_installer(
                root,
                selector="1.4.3",
                exact=release_metadata("1.4.3", digests),
                channel="pre-release",
                protocol="installer-v1",
                installer_digest=digests["install.sh"],
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            receipt = read_receipt(root, "1.4.3")
            self.assertEqual(receipt["update_channel"], "pre-release")
            self.assertEqual(receipt["installer_protocol"], "installer-v1")
            clear_requests(root)
            missing_digest, requests = run_installer(
                root,
                selector="1.4.3",
                exact=release_metadata("1.4.3", digests),
                protocol="installer-v1",
            )
            self.assertNotEqual(missing_digest.returncode, 0)
            self.assertEqual(len(requests), 1)
            self.assertIn(
                "requires the exact verified installer digest", missing_digest.stderr
            )

    def test_direct_installer_binds_the_receipt_to_its_executing_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "1.4.5")
            installer = root / "assets/install.sh"
            installer.write_bytes(
                installer.read_bytes() + b"# different release installer\n"
            )
            digests["install.sh"] = sha256(installer)
            installer_manifest = root / "assets/installer_SHA256SUMS"
            installer_manifest.write_text(
                f"{digests['install.sh']}  install.sh\n"
                f"{digests['install.ps1']}  install.ps1\n",
                encoding="utf-8",
            )
            digests["installer_SHA256SUMS"] = sha256(installer_manifest)

            result, requests = run_installer(
                root,
                selector="1.4.5",
                exact=release_metadata("1.4.5", digests),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(len(requests), 1)
            self.assertIn("executing install.sh digest", result.stderr)

    def test_matching_managed_installation_rejects_full_semver_downgrades(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            current_version = "2.0.0-rc.10+build.1"
            current_digests = create_release_assets(root, current_version)
            installed, _requests = run_installer(
                root,
                selector=current_version,
                exact=release_metadata(current_version, current_digests),
                channel="pre-release",
            )
            self.assertEqual(installed.returncode, 0, installed.stderr)
            downgrade_version = "2.0.0-rc.9+build.999"
            digests = create_release_assets(root, downgrade_version)
            clear_requests(root)

            result, requests = run_installer(
                root,
                selector=downgrade_version,
                exact=release_metadata(downgrade_version, digests),
                channel="pre-release",
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(len(requests), 1)
            self.assertIn("Refusing to downgrade", result.stderr)
            current = root / "codex-home/packages/standalone/current"
            self.assertEqual(current.resolve(), release_dir(root, current_version))

    def test_concurrent_lower_release_cannot_replace_a_newer_activation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            lower_root = root / "lower"
            newer_root = root / "newer"
            lower_root.mkdir()
            newer_root.mkdir()
            lower_version = "2.0.0"
            newer_version = "3.0.0"
            lower_digests = create_release_assets(lower_root, lower_version)
            newer_digests = create_release_assets(newer_root, newer_version)
            lower = prepare_installer(
                lower_root,
                selector=lower_version,
                exact=release_metadata(lower_version, lower_digests),
            )
            newer = prepare_installer(
                newer_root,
                selector=newer_version,
                exact=release_metadata(newer_version, newer_digests),
            )
            shared_codex_home = root / "shared-codex-home"
            shared_install_dir = root / "shared-bin"
            shared_home = root / "shared-home"
            shared_home.mkdir()
            for invocation in (lower, newer):
                invocation.env.update(
                    {
                        "CODEX_HOME": str(shared_codex_home),
                        "CODEX_INSTALL_DIR": str(shared_install_dir),
                        "HOME": str(shared_home),
                    }
                )

            flock_gate = lower_root / "flock-gate"
            os.mkfifo(flock_gate)
            lower.env["CODEX_TEST_FLOCK_GATE"] = str(flock_gate)
            install_flock_gate(lower.env)
            lower_process = subprocess.Popen(
                lower.args,
                env=lower.env,
                start_new_session=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                wait_for_path(lower_root / "flock.ready")
                newer_result = subprocess.run(
                    newer.args,
                    capture_output=True,
                    check=False,
                    env=newer.env,
                    text=True,
                )
                self.assertEqual(
                    newer_result.returncode,
                    0,
                    newer_result.stderr + newer_result.stdout,
                )

                flock_gate.write_text("continue\n", encoding="utf-8")
                lower_stdout, lower_stderr = lower_process.communicate(timeout=10)

                self.assertNotEqual(lower_process.returncode, 0, lower_stdout)
                self.assertIn("Refusing to downgrade", lower_stderr)
                current = shared_codex_home / "packages/standalone/current"
                current_receipt = json.loads(
                    (current / "installation-receipt.json").read_text(encoding="utf-8")
                )
                self.assertEqual(current_receipt["tag"], f"electivus-v{newer_version}")
            finally:
                if lower_process.poll() is None:
                    os.killpg(lower_process.pid, signal.SIGKILL)
                    lower_process.communicate(timeout=2)

    def test_pre_release_channel_promotes_to_compatible_newer_stable(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            current_digests = create_release_assets(root, "2.0.0-rc.1")
            installed, _requests = run_installer(
                root,
                selector="2.0.0-rc.1",
                exact=release_metadata("2.0.0-rc.1", current_digests),
                channel="pre-release",
            )
            self.assertEqual(installed.returncode, 0, installed.stderr)
            digests = create_release_assets(root, "2.0.0")
            clear_requests(root)
            inventory = [
                release_metadata("2.0.0-rc.2", digests),
                release_metadata("2.0.0", digests),
                release_metadata("3.0.0", digests),
            ]

            result, _requests = run_installer(
                root, selector="pre-release", inventory=inventory
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Resolved version: 2.0.0", result.stdout)
            self.assertEqual(
                read_receipt(root, "2.0.0"),
                {
                    "publisher": "Electivus",
                    "repository": REPOSITORY,
                    "tag": "electivus-v2.0.0",
                    "update_channel": "stable",
                    "target": TARGET,
                    "package_digest": digests[f"codex-package-{TARGET}.tar.gz"],
                    "installer_digest": digests["install.sh"],
                    "installer_protocol": "direct",
                },
            )

    def test_rejects_ambiguous_upstream_and_invalid_selectors_before_network(
        self,
    ) -> None:
        for selector in (
            "latest",
            "rust-v1.2.3",
            "v1.2.3",
            "release-one",
            "01.2.3",
            "0.0.0",
        ):
            with (
                self.subTest(selector=selector),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, requests = run_installer(Path(temp_dir), selector=selector)

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [])
                self.assertIn("Invalid Electivus", result.stderr)

    def test_exact_release_rejects_draft_malformed_incomplete_and_missing_digest(
        self,
    ) -> None:
        cases = ("draft", "unpublished", "wrong-tag", "incomplete", "missing-digest")
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_release_assets(root, "1.5.0")
                kwargs: dict[str, object] = {}
                version = "1.5.0"
                if case == "draft":
                    kwargs["draft"] = True
                elif case == "unpublished":
                    kwargs["published"] = False
                elif case == "wrong-tag":
                    version = "invalid"
                elif case == "incomplete":
                    kwargs["omit"] = {"installer_SHA256SUMS"}
                else:
                    digests["install.ps1"] = None
                exact = release_metadata(version, digests, **kwargs)

                result, requests = run_installer(root, selector="1.5.0", exact=exact)

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(requests), 1)
                self.assertIn("not published, valid, and complete", result.stderr)

    def test_release_selector_rejects_cr_and_lf_before_network_access(self) -> None:
        for selector in (
            "1.6.0\n",
            "1.6.0\r",
            "1.6.0\r\n",
            "electivus-v1.6.0\ntrailing",
        ):
            with (
                self.subTest(selector=repr(selector)),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, requests = run_installer(Path(temp_dir), selector=selector)

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [])
                self.assertIn("must not contain CR or LF", result.stderr)

    def test_malformed_and_oversized_metadata_fail_closed(self) -> None:
        for mode, exact in (("", '{"tag_name":'), ("oversized", None)):
            with (
                self.subTest(mode=mode or "malformed"),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, requests = run_installer(
                    Path(temp_dir), selector="1.6.0", exact=exact, metadata_mode=mode
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(requests), 1)
                if mode:
                    self.assertIn("1048576-byte safety limit", result.stderr)
                else:
                    self.assertIn("Could not parse", result.stderr)

    def test_metadata_with_nul_in_tag_is_rejected_before_semantic_parsing(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "1.6.2")
            document = json.dumps(release_metadata("1.6.2", digests)).encode()
            document = document.replace(
                b"electivus-v1.6.2", b"electivus-v1.6.2\x00ignored"
            )

            result, requests = run_installer(
                root,
                selector="1.6.2",
                exact=document,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(requests, [exact_url("1.6.2")])
            self.assertIn("forbidden NUL byte", result.stderr)

    def test_structurally_malformed_metadata_fails_closed(self) -> None:
        malformed_documents = (
            '{"tag_name":"electivus-v1.6.1" "draft":false}',
            '{"tag_name":"electivus-v1.6.1",}',
            '{"assets":[{} {}],"tag_name":"electivus-v1.6.1"}',
            '{"tag_name" "electivus-v1.6.1"}',
            r'{"tag_name":"electivus-v1.6.1\q"}',
            '{"tag_name":"electivus-v1.6.1\ninvalid"}',
        )
        for document in malformed_documents:
            with (
                self.subTest(document=document),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, requests = run_installer(
                    Path(temp_dir), selector="1.6.1", exact=document
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [exact_url("1.6.1")])
                self.assertIn("Could not parse", result.stderr)

    def test_semantically_duplicate_escaped_keys_fail_closed_on_both_routes(
        self,
    ) -> None:
        for route in ("exact", "inventory"):
            with (
                self.subTest(route=route),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_release_assets(root, "1.6.3")
                serialized = json.dumps(
                    release_metadata("1.6.3", digests), separators=(",", ":")
                )
                duplicate = '{"tag\\u005fname":"electivus-v999.0.0",' + serialized[1:]
                options: dict[str, object]
                if route == "exact":
                    options = {"selector": "1.6.3", "exact": duplicate}
                    expected_requests = [exact_url("1.6.3")]
                else:
                    options = {
                        "selector": None,
                        "inventory_pages": [f"[{duplicate}]"],
                    }
                    expected_requests = [inventory_url(1)]

                result, requests = run_installer(root, **options)

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, expected_requests)
                self.assertIn("Could not parse", result.stderr)

    def test_release_version_and_inventory_pagination_bounds_fail_closed(self) -> None:
        cases = (
            ("version", "1.2.3+" + "a" * 123, None, "128-byte"),
            ("oversized-page", None, [[{}] * 101], "exceeds 100 releases"),
            (
                "four-full-pages",
                None,
                [[{}] * 100 for _ in range(4)],
                "4-page safety limit",
            ),
            ("wrong-root", None, [{"not": "an inventory"}], "must be a JSON array"),
            ("non-release-entry", None, [[None]], "Could not parse"),
        )
        for case, selector, pages, message in cases:
            with (
                self.subTest(case=case),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                result, requests = run_installer(
                    Path(temp_dir), selector=selector, inventory_pages=pages
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                if case == "version":
                    self.assertEqual(requests, [])
                elif case == "four-full-pages":
                    self.assertEqual(len(requests), 4)

    def test_full_inventory_page_followed_by_short_terminal_page_is_valid(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.0.0")
            pages = [[{}] * 100, [release_metadata("7.0.0", digests)]]

            result, requests = run_installer(root, selector=None, inventory_pages=pages)

            self.assertEqual(result.returncode, 0, result.stderr)
            expected_metadata_requests = [inventory_url(1), inventory_url(2)]
            metadata_requests = [
                request for request in requests if request in expected_metadata_requests
            ]
            self.assertEqual(metadata_requests, expected_metadata_requests)
            self.assertIn("Resolved version: 7.0.0", result.stdout)

    def test_duplicate_release_across_inventory_pages_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.0.1")
            metadata = release_metadata("7.0.1", digests)
            pages = [[metadata, *([{}] * 99)], [metadata]]

            result, requests = run_installer(root, selector=None, inventory_pages=pages)

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(requests, [inventory_url(1), inventory_url(2)])
            self.assertIn("duplicate release", result.stderr)

    def test_asset_state_size_count_and_name_bounds_fail_closed(self) -> None:
        cases = ("state", "zero-size", "oversized-package", "too-many", "long-name")
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_release_assets(root, "7.1.0")
                metadata = release_metadata("7.1.0", digests)
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
                else:
                    assets.extend(valid_extra_assets(1, final_name="x" * 257))

                result, requests = run_installer(root, selector="7.1.0", exact=metadata)

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(requests), 1)
                self.assertIn("not published, valid, and complete", result.stderr)

    def test_release_metadata_scalar_types_fail_closed(self) -> None:
        cases: tuple[tuple[str, tuple[object, ...], object], ...] = (
            ("string-draft", ("draft",), "false"),
            ("string-prerelease", ("prerelease",), "false"),
            ("null-published-at", ("published_at",), None),
            ("number-published-at", ("published_at",), 1),
            ("null-tag", ("tag_name",), None),
            ("number-name", ("assets", 0, "name"), 1),
            ("null-digest", ("assets", 0, "digest"), None),
            ("boolean-state", ("assets", 0, "state"), True),
            ("string-size", ("assets", 0, "size"), "1"),
            ("boolean-size", ("assets", 0, "size"), True),
        )
        for case, path, value in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_release_assets(root, "7.1.4")
                metadata = release_metadata("7.1.4", digests)
                assign_nested(metadata, path, value)

                result, requests = run_installer(root, selector="7.1.4", exact=metadata)

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [exact_url("7.1.4")])
                self.assertIn("not published, valid, and complete", result.stderr)

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
                digests = create_release_assets(root, "7.1.9")
                metadata = release_metadata("7.1.9", digests)
                metadata["published_at"] = value

                result, requests = run_installer(root, selector="7.1.9", exact=metadata)

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [exact_url("7.1.9")])
                self.assertFalse((root / "install-bin/codex").exists())

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
                digests = create_release_assets(root, "7.1.9")
                metadata = release_metadata("7.1.9", digests)
                metadata["published_at"] = value

                result, _requests = run_installer(
                    root, selector="7.1.9", exact=metadata
                )

                self.assertEqual(result.returncode, 0, result.stderr)

    def test_assets_array_rejects_every_non_object_entry(self) -> None:
        for value in (None, "scalar", 1, True):
            with (
                self.subTest(value=value),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_release_assets(root, "7.1.10")
                metadata = release_metadata("7.1.10", digests)
                assets = metadata["assets"]
                assert isinstance(assets, list)
                assets.append(value)

                result, requests = run_installer(
                    root, selector="7.1.10", exact=metadata
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [exact_url("7.1.10")])
                self.assertIn("Could not parse", result.stderr)
                self.assertFalse((root / "install-bin/codex").exists())

    def test_asset_count_name_and_type_size_upper_boundaries_are_valid(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.1.1")
            metadata = release_metadata("7.1.1", digests)
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
            assets.extend(valid_extra_assets(56, final_name="x" * 256))

            result, _requests = run_installer(root, selector="7.1.1", exact=metadata)

            self.assertEqual(result.returncode, 0, result.stderr)

    def test_wget_transport_succeeds_with_a_streaming_hard_cap(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.1.2")

            result, requests = run_installer(
                root,
                selector="7.1.2",
                exact=release_metadata("7.1.2", digests),
                transport="wget",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                requests,
                [
                    exact_url("7.1.2"),
                    asset_url("7.1.2", "codex-package_SHA256SUMS"),
                    asset_url("7.1.2", "installer_SHA256SUMS"),
                    asset_url("7.1.2", f"codex-package-{TARGET}.tar.gz"),
                ],
            )

    def test_wget_transport_stops_an_oversized_stream(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)

            result, requests = run_installer(
                root,
                selector="7.1.3",
                metadata_mode="oversized",
                transport="wget",
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(requests, [exact_url("7.1.3")])
            self.assertIn("exceeded the 1048576-byte safety limit", result.stderr)

    def test_curl_transport_stops_a_slow_unknown_length_stream_near_the_cap(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)

            started = time.monotonic()
            result, requests = run_installer(
                root,
                selector="7.1.5",
                metadata_mode="slow-oversized",
            )
            elapsed = time.monotonic() - started

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(requests, [exact_url("7.1.5")])
            self.assertIn("exceeded the 1048576-byte safety limit", result.stderr)
            self.assertLess(elapsed, 5)
            self.assertLessEqual(
                int((root / "curl-streamed-bytes").read_text(encoding="utf-8")),
                1_114_112,
            )
            wait_for_process_exit(
                int((root / "curl-stream-pid").read_text(encoding="utf-8"))
            )

    def test_signals_stop_blocked_curl_and_drip_wget_without_leaks(self) -> None:
        cases = (
            ("blocked-curl", "curl", signal.SIGHUP),
            ("blocked-curl", "curl", signal.SIGINT),
            ("blocked-curl", "curl", signal.SIGTERM),
            ("drip-wget", "wget", signal.SIGHUP),
            ("drip-wget", "wget", signal.SIGINT),
            ("drip-wget", "wget", signal.SIGTERM),
        )
        for mode, transport, sent_signal in cases:
            with (
                self.subTest(mode=mode, signal=sent_signal),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                digests = create_release_assets(root, "7.1.7")
                invocation = prepare_installer(
                    root,
                    selector="7.1.7",
                    exact=release_metadata("7.1.7", digests),
                    force_fallback_lock=True,
                    metadata_mode=mode,
                    transport=transport,
                )
                invocation.env["TMPDIR"] = str(root)
                process = subprocess.Popen(
                    invocation.args,
                    env=invocation.env,
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
                    assert_interrupted_install_left_no_state(root, "7.1.7")
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=2)

    def test_package_metadata_and_manifest_digests_must_agree(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "1.7.0")
            digests[f"codex-package-{TARGET}.tar.gz"] = "0" * 64

            result, requests = run_installer(
                root, selector="1.7.0", exact=release_metadata("1.7.0", digests)
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("digest disagreement", result.stderr)
            self.assertFalse(
                any(
                    request.endswith(f"codex-package-{TARGET}.tar.gz")
                    for request in requests
                )
            )

    def test_corrupt_package_is_not_extracted_or_activated(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "1.8.0")
            (root / "assets" / f"codex-package-{TARGET}.tar.gz").write_bytes(b"corrupt")

            result, _requests = run_installer(
                root, selector="1.8.0", exact=release_metadata("1.8.0", digests)
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("checksum did not match", result.stderr)
            self.assertFalse(release_dir(root, "1.8.0").exists())
            self.assertFalse((root / "install-bin" / "codex").exists())

    def test_installer_manifest_must_agree_with_installer_asset_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "1.9.0")
            manifest = root / "assets" / "installer_SHA256SUMS"
            manifest.write_text(
                f"{'0' * 64}  install.sh\n{digests['install.ps1']}  install.ps1\n",
                encoding="utf-8",
            )
            digests["installer_SHA256SUMS"] = sha256(manifest)

            result, _requests = run_installer(
                root, selector="1.9.0", exact=release_metadata("1.9.0", digests)
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("digest disagreement for install.sh", result.stderr)

    def test_fallback_reclaimers_cannot_remove_a_new_installer_lock_owner(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.1.6")
            invocation = prepare_installer(
                root,
                selector="7.1.6",
                exact=release_metadata("7.1.6", digests),
                force_fallback_lock=True,
                metadata_mode="hold-first-manifest",
            )
            standalone_root = root / "codex-home/packages/standalone"
            lock_path = standalone_root / "install.lock.d"
            lock_path.mkdir(parents=True)
            (lock_path / "pid").write_text("2147483647\n", encoding="utf-8")
            (lock_path / "started_at").write_text("1\n", encoding="utf-8")
            reclaim_continue = root / "allow-reclaim"
            invocation.env["CODEX_TEST_RECLAIM_CONTINUE"] = str(reclaim_continue)
            install_reclaim_guard_barrier(invocation.env)
            processes: list[subprocess.Popen[str]] = []
            try:
                for _index in range(2):
                    processes.append(
                        subprocess.Popen(
                            invocation.args,
                            env=invocation.env,
                            text=True,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                        )
                    )

                wait_for_glob_count(f"{lock_path}.reclaim.*", 2)
                reclaim_continue.touch()
                wait_for_path(root / "download.ready")
                live_owner = lock_path.read_text(encoding="utf-8")
                live_pid = int(live_owner.splitlines()[0])
                os.kill(live_pid, 0)

                processes.append(
                    subprocess.Popen(
                        invocation.args,
                        env=invocation.env,
                        text=True,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                    )
                )
                time.sleep(0.2)
                self.assertIsNone(processes[-1].poll())
                self.assertEqual(lock_path.read_text(encoding="utf-8"), live_owner)

                (root / "allow-download").touch()
                for process in processes:
                    stdout, stderr = process.communicate(timeout=10)
                    self.assertEqual(process.returncode, 0, stderr + stdout)
            finally:
                reclaim_continue.touch(exist_ok=True)
                (root / "allow-download").touch(exist_ok=True)
                for process in processes:
                    if process.poll() is None:
                        process.kill()
                        process.communicate()

    def test_signal_cleanup_cannot_unlink_a_successor_reclaim_guard(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.1.8")
            invocation = prepare_installer(
                root,
                selector="7.1.8",
                exact=release_metadata("7.1.8", digests),
                force_fallback_lock=True,
            )
            standalone_root = root / "codex-home/packages/standalone"
            lock_path = standalone_root / "install.lock.d"
            lock_path.mkdir(parents=True)
            (lock_path / "pid").write_text("2147483647\n", encoding="utf-8")
            (lock_path / "started_at").write_text("1\n", encoding="utf-8")
            successor_marker = Path(f"{lock_path}.reclaim.successor")
            successor_guard = Path(f"{lock_path}.reclaim.guard")
            invocation.env["CODEX_TEST_SUCCESSOR_MARKER"] = str(successor_marker)
            install_guard_unlink_successor_signal(invocation.env)

            process = subprocess.Popen(
                invocation.args,
                env=invocation.env,
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
                self.assertFalse((standalone_root / "current").exists())
                self.assertFalse((root / "install-bin/codex").exists())
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.communicate(timeout=2)

    def test_live_reused_or_unverifiable_lock_pid_fails_closed_promptly(self) -> None:
        for case in ("fingerprint-mismatch", "unknown-identity"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_release_assets(root, "7.1.11")
                invocation = prepare_installer(
                    root,
                    selector="7.1.11",
                    exact=release_metadata("7.1.11", digests),
                    force_fallback_lock=True,
                )
                standalone_root = root / "codex-home/packages/standalone"
                standalone_root.mkdir(parents=True)
                lock_path = standalone_root / "install.lock.d"
                lock_lines = [str(os.getpid()), str(int(time.time())), "foreign-owner"]
                if case == "fingerprint-mismatch":
                    lock_lines.append("fingerprint=definitely-not-this-process")
                lock_contents = "\n".join(lock_lines) + "\n"
                lock_path.write_text(lock_contents, encoding="utf-8")

                process = subprocess.Popen(
                    invocation.args,
                    env=invocation.env,
                    start_new_session=True,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                try:
                    started = time.monotonic()
                    stdout, stderr = communicate_bounded(process)
                    elapsed = time.monotonic() - started

                    self.assertNotEqual(process.returncode, 0, stdout)
                    self.assertLess(elapsed, 2)
                    self.assertIn(str(lock_path), stderr)
                    self.assertIn("manual recovery", stderr)
                    self.assertEqual(
                        lock_path.read_text(encoding="utf-8"), lock_contents
                    )
                    self.assertFalse((standalone_root / "current").exists())
                    self.assertFalse((root / "install-bin/codex").exists())
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=2)

    def test_malformed_fallback_lock_fails_closed_promptly(self) -> None:
        for case in (
            "file",
            "directory",
            "fifo",
            "symlink-directory",
            "huge-pid",
            "huge-started-at",
            "malformed-fingerprint",
        ):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_release_assets(root, "7.1.12")
                invocation = prepare_installer(
                    root,
                    selector="7.1.12",
                    exact=release_metadata("7.1.12", digests),
                    force_fallback_lock=True,
                )
                standalone_root = root / "codex-home/packages/standalone"
                standalone_root.mkdir(parents=True)
                lock_path = standalone_root / "install.lock.d"
                if case == "file":
                    lock_path.write_text("not-a-pid\n1\n", encoding="utf-8")
                elif case == "directory":
                    lock_path.mkdir()
                    (lock_path / "pid").write_text("not-a-pid\n", encoding="utf-8")
                    (lock_path / "started_at").write_text("1\n", encoding="utf-8")
                elif case == "fifo":
                    os.mkfifo(lock_path)
                elif case == "symlink-directory":
                    symlink_target = root / "foreign-lock-directory"
                    symlink_target.mkdir()
                    (symlink_target / "pid").write_text(
                        "2147483647\n", encoding="utf-8"
                    )
                    (symlink_target / "started_at").write_text(
                        f"{int(time.time())}\n", encoding="utf-8"
                    )
                    lock_path.symlink_to(symlink_target, target_is_directory=True)
                elif case == "huge-pid":
                    lock_path.write_text(
                        f"999999999999999999999999\n{int(time.time())}\nforeign-owner\n",
                        encoding="utf-8",
                    )
                elif case == "huge-started-at":
                    lock_path.write_text(
                        "2147483647\n999999999999999999999999\nforeign-owner\n",
                        encoding="utf-8",
                    )
                else:
                    lock_path.write_text(
                        f"2147483647\n{int(time.time())}\nforeign-owner\n"
                        "fingerprint=linux-proc:00000000-0000-0000-0000-000000000000:"
                        "999999999999999999999999\n",
                        encoding="utf-8",
                    )

                process = subprocess.Popen(
                    invocation.args,
                    env=invocation.env,
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
                    self.assertNotIn("Illegal number", stderr)
                    self.assertFalse((standalone_root / "current").exists())
                    if case == "symlink-directory":
                        self.assertTrue((symlink_target / "pid").exists())
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=2)

    def test_non_regular_reclaim_guard_fails_closed_promptly(self) -> None:
        for case in ("directory", "fifo", "broken-symlink"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_release_assets(root, "7.1.17")
                invocation = prepare_installer(
                    root,
                    selector="7.1.17",
                    exact=release_metadata("7.1.17", digests),
                    force_fallback_lock=True,
                )
                standalone_root = root / "codex-home/packages/standalone"
                standalone_root.mkdir(parents=True)
                guard = standalone_root / "install.lock.d.reclaim.guard"
                if case == "directory":
                    guard.mkdir()
                elif case == "fifo":
                    os.mkfifo(guard)
                else:
                    guard.symlink_to(root / "missing-reclaim-marker")

                process = subprocess.Popen(
                    invocation.args,
                    env=invocation.env,
                    start_new_session=True,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                try:
                    stdout, stderr = communicate_bounded(process)

                    self.assertNotEqual(process.returncode, 0, stdout)
                    self.assertIn(str(guard), stderr)
                    self.assertIn("manual recovery", stderr)
                    self.assertFalse((standalone_root / "current").exists())
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=2)

    def test_non_regular_legacy_lock_metadata_is_never_read(self) -> None:
        for metadata_name in ("pid", "started_at", "fingerprint"):
            for metadata_type in ("fifo", "symlink"):
                with (
                    self.subTest(
                        metadata_name=metadata_name, metadata_type=metadata_type
                    ),
                    tempfile.TemporaryDirectory() as temp_dir,
                ):
                    root = Path(temp_dir)
                    digests = create_release_assets(root, "7.1.19")
                    invocation = prepare_installer(
                        root,
                        selector="7.1.19",
                        exact=release_metadata("7.1.19", digests),
                        force_fallback_lock=True,
                    )
                    standalone_root = root / "codex-home/packages/standalone"
                    lock_path = standalone_root / "install.lock.d"
                    lock_path.mkdir(parents=True)
                    metadata_values = {
                        "pid": "2147483647\n",
                        "started_at": f"{int(time.time())}\n",
                        "fingerprint": (
                            "linux-proc:00000000-0000-0000-0000-000000000000:1\n"
                        ),
                    }
                    for name, value in metadata_values.items():
                        (lock_path / name).write_text(value, encoding="utf-8")
                    metadata_path = lock_path / metadata_name
                    metadata_path.unlink()
                    external_target = root / f"external-{metadata_name}"
                    if metadata_type == "fifo":
                        os.mkfifo(metadata_path)
                    else:
                        external_target.write_text(
                            metadata_values[metadata_name], encoding="utf-8"
                        )
                        metadata_path.symlink_to(external_target)

                    process = subprocess.Popen(
                        invocation.args,
                        env=invocation.env,
                        start_new_session=True,
                        text=True,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                    )
                    try:
                        stdout, stderr = communicate_bounded(process)

                        self.assertNotEqual(process.returncode, 0, stdout)
                        self.assertIn(str(metadata_path), stderr)
                        self.assertIn("manual recovery", stderr)
                        self.assertFalse((standalone_root / "current").exists())
                        if metadata_type == "symlink":
                            self.assertEqual(
                                external_target.read_text(encoding="utf-8"),
                                metadata_values[metadata_name],
                            )
                    finally:
                        if process.poll() is None:
                            os.killpg(process.pid, signal.SIGKILL)
                            process.communicate(timeout=2)

    def test_legacy_metadata_path_swap_cannot_reclaim_live_fallback_lock(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.1.20")
            invocation = prepare_installer(
                root,
                selector="7.1.20",
                exact=release_metadata("7.1.20", digests),
                force_fallback_lock=True,
            )
            standalone_root = root / "codex-home/packages/standalone"
            lock_path = standalone_root / "install.lock.d"
            lock_path.mkdir(parents=True)
            live_pid_path = lock_path / "pid"
            live_pid_contents = f"{os.getpid()}\n"
            live_pid_path.write_text(live_pid_contents, encoding="utf-8")
            (lock_path / "started_at").write_text(
                f"{int(time.time())}\n", encoding="utf-8"
            )
            boot_id = (
                Path("/proc/sys/kernel/random/boot_id")
                .read_text(encoding="utf-8")
                .strip()
            )
            process_stat = Path(f"/proc/{os.getpid()}/stat").read_text(encoding="utf-8")
            start_ticks = process_stat.rsplit(") ", maxsplit=1)[1].split()[19]
            (lock_path / "fingerprint").write_text(
                f"linux-proc:{boot_id}:{start_ticks}\n", encoding="utf-8"
            )
            replacement = root / "stale-pid"
            replacement.write_text("2147483647\n", encoding="utf-8")
            saved_path = root / "live-pid"
            real_mv = shutil.which("mv")
            real_ln = shutil.which("ln")
            real_rm = shutil.which("rm")
            assert real_mv is not None
            assert real_ln is not None
            assert real_rm is not None
            invocation.env.update(
                {
                    "CODEX_TEST_SWAP_METADATA_PATH": str(live_pid_path),
                    "CODEX_TEST_SWAP_METADATA_PARENT": str(lock_path),
                    "CODEX_TEST_SWAP_METADATA_REPLACEMENT": str(replacement),
                    "CODEX_TEST_SWAP_METADATA_SAVED": str(saved_path),
                }
            )
            for command in ("cat", "python3"):
                real_command = shutil.which(command)
                assert real_command is not None
                command_path = Path(invocation.env["PATH"]) / command
                command_path.unlink(missing_ok=True)
                write_executable(
                    command_path,
                    textwrap.dedent(
                        f"""\
                        #!/bin/sh
                        should_swap=false
                        if [ "{command}" = cat ] &&
                          [ "$1" = "$CODEX_TEST_SWAP_METADATA_PATH" ]; then
                          should_swap=true
                        elif [ "{command}" = python3 ]; then
                          last=""
                          for argument in "$@"; do last="$argument"; done
                          if [ "$last" = "$CODEX_TEST_SWAP_METADATA_PARENT" ]; then
                            should_swap=true
                          fi
                        fi
                        if [ "$should_swap" = true ]; then
                          "{real_mv}" "$CODEX_TEST_SWAP_METADATA_PATH" \
                            "$CODEX_TEST_SWAP_METADATA_SAVED"
                          "{real_ln}" -s "$CODEX_TEST_SWAP_METADATA_REPLACEMENT" \
                            "$CODEX_TEST_SWAP_METADATA_PATH"
                          status=0
                          "{real_command}" "$@" || status=$?
                          "{real_rm}" -f "$CODEX_TEST_SWAP_METADATA_PATH"
                          "{real_mv}" "$CODEX_TEST_SWAP_METADATA_SAVED" \
                            "$CODEX_TEST_SWAP_METADATA_PATH"
                          exit "$status"
                        fi
                        exec "{real_command}" "$@"
                        """
                    ),
                )

            result = run_prepared_installer(invocation)

            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn(str(live_pid_path), result.stderr)
            self.assertIn("manual recovery", result.stderr)
            self.assertTrue(lock_path.is_dir())
            self.assertEqual(
                live_pid_path.read_text(encoding="utf-8"), live_pid_contents
            )
            self.assertFalse((standalone_root / "current").exists())

    def test_dead_fallback_lock_is_reclaimed_without_an_age_wait(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.1.14")
            invocation = prepare_installer(
                root,
                selector="7.1.14",
                exact=release_metadata("7.1.14", digests),
                force_fallback_lock=True,
            )
            standalone_root = root / "codex-home/packages/standalone"
            standalone_root.mkdir(parents=True)
            lock_path = standalone_root / "install.lock.d"
            lock_path.write_text(
                f"2147483647\n{int(time.time())}\nforeign-owner\n",
                encoding="utf-8",
            )
            process = subprocess.Popen(
                invocation.args,
                env=invocation.env,
                start_new_session=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                stdout, stderr = communicate_bounded(process)

                self.assertEqual(process.returncode, 0, stderr + stdout)
                self.assertIn("Removing stale installer lock", stderr)
                self.assertEqual(
                    read_receipt(root, "7.1.14")["tag"], "electivus-v7.1.14"
                )
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.communicate(timeout=2)

    def test_fallback_hard_link_claim_failure_exits_promptly(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.1.13")
            invocation = prepare_installer(
                root,
                selector="7.1.13",
                exact=release_metadata("7.1.13", digests),
                force_fallback_lock=True,
            )
            install_failing_hard_link(invocation.env)
            process = subprocess.Popen(
                invocation.args,
                env=invocation.env,
                start_new_session=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            stdout, stderr = communicate_bounded(process)

            self.assertNotEqual(process.returncode, 0, stdout)
            self.assertIn("Could not claim the installer lock", stderr)
            self.assertIn("no competing lock exists", stderr)
            standalone_root = root / "codex-home/packages/standalone"
            self.assertFalse((standalone_root / "current").exists())

    def test_stale_reclaim_marker_removal_failure_is_explicit(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.1.15")
            invocation = prepare_installer(
                root,
                selector="7.1.15",
                exact=release_metadata("7.1.15", digests),
                force_fallback_lock=True,
            )
            standalone_root = root / "codex-home/packages/standalone"
            standalone_root.mkdir(parents=True)
            marker = standalone_root / "install.lock.d.reclaim.stale"
            marker.write_text(
                f"2147483647\n{int(time.time())}\nmarker=stale\n",
                encoding="utf-8",
            )
            install_rm_failure(invocation.env, failed_path=marker)
            result = run_prepared_installer(invocation)

            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("Could not remove stale reclaim marker", result.stderr)
            self.assertTrue(marker.exists())
            self.assertFalse((standalone_root / "current").exists())

    def test_stale_lock_removal_failures_are_explicit_and_terminal(self) -> None:
        for case in ("lock", "legacy-snapshot", "snapshot"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_release_assets(root, "7.1.18")
                invocation = prepare_installer(
                    root,
                    selector="7.1.18",
                    exact=release_metadata("7.1.18", digests),
                    force_fallback_lock=True,
                )
                standalone_root = root / "codex-home/packages/standalone"
                standalone_root.mkdir(parents=True)
                lock_path = standalone_root / "install.lock.d"
                if case == "legacy-snapshot":
                    lock_path.mkdir()
                    (lock_path / "pid").write_text("2147483647\n", encoding="utf-8")
                    (lock_path / "started_at").write_text(
                        f"{int(time.time())}\n", encoding="utf-8"
                    )
                else:
                    lock_path.write_text(
                        f"2147483647\n{int(time.time())}\nforeign-owner\n",
                        encoding="utf-8",
                    )
                install_rm_failure(
                    invocation.env,
                    failed_path=lock_path if case == "lock" else None,
                    fail_persistently=case == "lock",
                    fail_reclaim_stale_cleanup=case == "legacy-snapshot",
                    fail_reclaim_snapshot_cleanup=case == "snapshot",
                )
                process = subprocess.Popen(
                    invocation.args,
                    env=invocation.env,
                    start_new_session=True,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                try:
                    stdout, stderr = communicate_bounded(process)

                    self.assertNotEqual(process.returncode, 0, stdout)
                    self.assertIn("Could not remove", stderr)
                    self.assertIn(str(lock_path), stderr)
                    self.assertFalse((standalone_root / "current").exists())
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=2)

    def test_reclaim_marker_publication_failure_is_explicit(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "7.1.16")
            invocation = prepare_installer(
                root,
                selector="7.1.16",
                exact=release_metadata("7.1.16", digests),
                force_fallback_lock=True,
            )
            standalone_root = root / "codex-home/packages/standalone"
            standalone_root.mkdir(parents=True)
            lock_path = standalone_root / "install.lock.d"
            original_lock = f"2147483647\n{int(time.time())}\nforeign-owner\n"
            lock_path.write_text(original_lock, encoding="utf-8")
            install_reclaim_marker_mv_failure(invocation.env)

            result = run_prepared_installer(invocation)

            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("Could not publish reclaim marker", result.stderr)
            self.assertEqual(lock_path.read_text(encoding="utf-8"), original_lock)
            self.assertEqual(list(standalone_root.glob("*.reclaim-prepare.*")), [])

    def test_reclaim_marker_partial_publication_is_removed(self) -> None:
        for case in ("write", "missing-publication"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                digests = create_release_assets(root, "7.1.20")
                invocation = prepare_installer(
                    root,
                    selector="7.1.20",
                    exact=release_metadata("7.1.20", digests),
                    force_fallback_lock=True,
                )
                standalone_root = root / "codex-home/packages/standalone"
                standalone_root.mkdir(parents=True)
                lock_path = standalone_root / "install.lock.d"
                original_lock = f"2147483647\n{int(time.time())}\nforeign-owner\n"
                lock_path.write_text(original_lock, encoding="utf-8")
                if case == "write":
                    install_reclaim_marker_date_failure(invocation.env)
                else:
                    install_reclaim_marker_mv_failure(
                        invocation.env, reports_success=True
                    )

                result = run_prepared_installer(invocation)

                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("reclaim marker", result.stderr.lower())
                self.assertEqual(lock_path.read_text(encoding="utf-8"), original_lock)
                self.assertEqual(list(standalone_root.glob("*.reclaim-prepare.*")), [])
                self.assertEqual(list(standalone_root.glob("*.reclaim.*")), [])

    def test_process_identity_fails_closed_without_proc(self) -> None:
        script = INSTALL_SCRIPT.read_text(encoding="utf-8")
        function_start = script.index("process_start_fingerprint() {")
        function_end = script.index("\n}\n", function_start) + 3
        function_source = script[function_start:function_end]
        result = subprocess.run(
            [
                "/bin/sh",
                "-c",
                f"{function_source}\nprocess_start_fingerprint 2147483647",
            ],
            capture_output=True,
            check=False,
            text=True,
        )

        self.assertNotIn("ps ", function_source)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")

    def test_macos_fails_before_any_network_request(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result, requests = run_installer(Path(temp_dir), force_macos=True)

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(requests, [])
            self.assertIn(
                "does not yet publish or validate standalone macOS", result.stderr
            )
            self.assertIn("will not fall back to OpenAI", result.stderr)

    def test_namespaced_cache_is_safely_reinstalled_and_requires_exact_receipt(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "1.11.0")
            exact = release_metadata("1.11.0", digests)
            old_upstream_cache = (
                root
                / "codex-home"
                / "packages"
                / "standalone"
                / "releases"
                / f"1.11.0-{TARGET}"
            )
            old_upstream_cache.mkdir(parents=True)

            first, first_requests = run_installer(root, selector="1.11.0", exact=exact)
            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertTrue(release_dir(root, "1.11.0").is_dir())
            self.assertGreater(len(first_requests), 1)

            clear_requests(root)
            second, second_requests = run_installer(
                root, selector="1.11.0", exact=exact
            )
            self.assertEqual(second.returncode, 0, second.stderr)
            self.assertEqual(len(second_requests), 4)
            self.assertIn("Downloading Electivus checksum manifests", second.stdout)
            self.assertIn("cannot be authenticated", second.stderr)

            receipt_path = release_dir(root, "1.11.0") / "installation-receipt.json"
            receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
            receipt["package_digest"] = "0" * 64
            receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
            clear_requests(root)

            third, third_requests = run_installer(root, selector="1.11.0", exact=exact)
            self.assertEqual(third.returncode, 0, third.stderr)
            self.assertEqual(len(third_requests), 4)
            self.assertIn("provenance-mismatched", third.stderr)
            self.assertEqual(
                read_receipt(root, "1.11.0")["package_digest"],
                digests[f"codex-package-{TARGET}.tar.gz"],
            )

    def test_tampered_cached_executable_is_not_run_before_safe_reinstall(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            version = "1.11.1"
            digests = create_release_assets(root, version)
            exact = release_metadata(version, digests)
            installed, _requests = run_installer(root, selector=version, exact=exact)
            self.assertEqual(installed.returncode, 0, installed.stderr)
            tamper_marker = root / "tampered-executable-ran"
            write_executable(
                release_dir(root, version) / "bin" / "codex",
                f"#!/bin/sh\n: >'{tamper_marker}'\nprintf 'codex-cli {version}\\n'\n",
            )
            clear_requests(root)

            result, requests = run_installer(root, selector=version, exact=exact)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(requests), 4)
            self.assertFalse(tamper_marker.exists())
            self.assertNotIn(
                str(tamper_marker),
                (release_dir(root, version) / "bin" / "codex").read_text(
                    encoding="utf-8"
                ),
            )

    def test_failed_same_version_reinstall_restores_exact_active_release(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            version = "1.11.2"
            initial_digests = create_release_assets(root, version)
            initial, _requests = run_installer(
                root,
                selector=version,
                exact=release_metadata(version, initial_digests),
            )
            self.assertEqual(initial.returncode, 0, initial.stderr)
            installed_release = release_dir(root, version)
            current = root / "codex-home/packages/standalone/current"
            visible = root / "install-bin/codex"
            receipt_path = installed_release / "installation-receipt.json"
            receipt_path.write_bytes(b'{"publisher":"incomplete"}\n')
            old_codex_bytes = (installed_release / "bin/codex").read_bytes()
            old_receipt_bytes = receipt_path.read_bytes()
            old_current_target = os.readlink(current)
            old_visible_target = os.readlink(visible)
            failing_digests = create_release_assets(
                root, version, fail_during_activation=True
            )
            clear_requests(root)

            result, _requests = run_installer(
                root,
                selector=version,
                exact=release_metadata(version, failing_digests),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("restoring the previous installed bytes", result.stderr)
            self.assertEqual(
                (installed_release / "bin/codex").read_bytes(), old_codex_bytes
            )
            self.assertEqual(receipt_path.read_bytes(), old_receipt_bytes)
            self.assertEqual(os.readlink(current), old_current_target)
            self.assertEqual(os.readlink(visible), old_visible_target)
            self.assertEqual(
                subprocess.run(
                    [visible, "--version"], capture_output=True, text=True, check=True
                ).stdout,
                f"codex-cli {version}\n",
            )
            self.assertEqual(
                list(
                    (
                        root
                        / "codex-home/packages/standalone/releases/Electivus/electivus-codex"
                    ).glob(".rollback.*")
                ),
                [],
            )

    def test_failed_replacement_removal_preserves_backup_and_finishes_cleanup(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            version = "1.11.4"
            initial_digests = create_release_assets(root, version)
            initial, _requests = run_installer(
                root,
                selector=version,
                exact=release_metadata(version, initial_digests),
            )
            self.assertEqual(initial.returncode, 0, initial.stderr)
            installed_release = release_dir(root, version)
            receipt_path = installed_release / "installation-receipt.json"
            receipt_path.write_bytes(b'{"publisher":"incomplete"}\n')
            old_codex_bytes = (installed_release / "bin/codex").read_bytes()
            old_receipt_bytes = receipt_path.read_bytes()
            failing_digests = create_release_assets(
                root, version, fail_during_activation=True
            )
            invocation = prepare_installer(
                root,
                selector=version,
                exact=release_metadata(version, failing_digests),
                force_fallback_lock=True,
            )
            invocation.env["TMPDIR"] = str(root)
            install_rm_failure(invocation.env, failed_path=installed_release)

            result = run_prepared_installer(invocation)

            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn(
                f"Could not remove the failed replacement at {installed_release}",
                result.stderr,
            )
            self.assertNotIn("restoring the previous installed bytes", result.stderr)
            releases = installed_release.parents[1]
            backups = list(releases.glob(f".rollback.{version}.{TARGET}.*"))
            self.assertEqual(len(backups), 1)
            self.assertIn(
                f"The previous release remains preserved at {backups[0]}; "
                "manual recovery is required.",
                result.stderr,
            )
            self.assertEqual((backups[0] / "bin/codex").read_bytes(), old_codex_bytes)
            self.assertEqual(
                (backups[0] / "installation-receipt.json").read_bytes(),
                old_receipt_bytes,
            )
            self.assertEqual(list(installed_release.glob(".rollback.*")), [])
            standalone = root / "codex-home/packages/standalone"
            rm_log = (root / "rm-targets.log").read_text(encoding="utf-8")
            self.assertIn(str(installed_release), rm_log)
            self.assertIn(str(standalone / "install.lock.d"), rm_log)
            self.assertFalse((standalone / "install.lock.d").exists())
            self.assertEqual(list(standalone.glob("install.lock.owner.*")), [])
            self.assertEqual(list(root.glob("tmp.*")), [])

    def test_concurrent_recreation_during_release_rollback_preserves_backup(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            version = "1.11.5"
            initial_digests = create_release_assets(root, version)
            initial, _requests = run_installer(
                root,
                selector=version,
                exact=release_metadata(version, initial_digests),
            )
            self.assertEqual(initial.returncode, 0, initial.stderr)
            installed_release = release_dir(root, version)
            receipt_path = installed_release / "installation-receipt.json"
            receipt_path.write_bytes(b'{"publisher":"incomplete"}\n')
            old_codex_bytes = (installed_release / "bin/codex").read_bytes()
            old_receipt_bytes = receipt_path.read_bytes()
            failing_digests = create_release_assets(
                root, version, fail_during_activation=True
            )
            invocation = prepare_installer(
                root,
                selector=version,
                exact=release_metadata(version, failing_digests),
                force_fallback_lock=True,
            )
            invocation.env["TMPDIR"] = str(root)
            install_release_restore_destination_race(
                invocation.env, restored_path=installed_release
            )

            result = run_prepared_installer(invocation)

            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertNotIn("restoring the previous installed bytes", result.stderr)
            self.assertIn(
                f"Could not restore the previous release at {installed_release} "
                "because that destination reappeared during rollback.",
                result.stderr,
            )
            self.assertEqual(
                (installed_release / "concurrent-owner").read_text(encoding="utf-8"),
                "concurrent\n",
            )
            self.assertEqual(list(installed_release.glob(".rollback.*")), [])
            releases = installed_release.parents[1]
            backups = list(releases.glob(f".rollback.{version}.{TARGET}.*"))
            self.assertEqual(len(backups), 1)
            self.assertIn(
                f"The previous release remains preserved at {backups[0]}; "
                "manual recovery is required.",
                result.stderr,
            )
            self.assertEqual((backups[0] / "bin/codex").read_bytes(), old_codex_bytes)
            self.assertEqual(
                (backups[0] / "installation-receipt.json").read_bytes(),
                old_receipt_bytes,
            )
            standalone = root / "codex-home/packages/standalone"
            self.assertFalse((standalone / "install.lock.d").exists())
            self.assertEqual(list(standalone.glob("install.lock.owner.*")), [])
            self.assertEqual(list(root.glob("tmp.*")), [])

    def test_signals_during_same_version_reinstall_restore_exact_active_release(
        self,
    ) -> None:
        for sent_signal in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
            with (
                self.subTest(signal=sent_signal),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                version = "1.11.3"
                initial_digests = create_release_assets(root, version)
                initial, _requests = run_installer(
                    root,
                    selector=version,
                    exact=release_metadata(version, initial_digests),
                )
                self.assertEqual(initial.returncode, 0, initial.stderr)
                installed_release = release_dir(root, version)
                current = root / "codex-home/packages/standalone/current"
                visible = root / "install-bin/codex"
                receipt_path = installed_release / "installation-receipt.json"
                receipt_path.write_bytes(b'{"publisher":"incomplete"}\n')
                old_codex_bytes = (installed_release / "bin/codex").read_bytes()
                old_receipt_bytes = receipt_path.read_bytes()
                old_current_target = os.readlink(current)
                old_visible_target = os.readlink(visible)
                activation_gate = root / "activation.gate"
                os.mkfifo(activation_gate)
                blocking_digests = create_release_assets(
                    root, version, block_during_activation=True
                )
                invocation = prepare_installer(
                    root,
                    selector=version,
                    exact=release_metadata(version, blocking_digests),
                )
                invocation.env["CODEX_TEST_ACTIVATION_GATE"] = str(activation_gate)
                process = subprocess.Popen(
                    invocation.args,
                    env=invocation.env,
                    start_new_session=True,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                try:
                    wait_for_path(root / "activation.ready")
                    verification_pid = int(
                        (root / "activation.pid").read_text(encoding="utf-8")
                    )
                    os.kill(process.pid, sent_signal)
                    stdout, stderr = communicate_bounded(process)

                    self.assertEqual(
                        process.returncode, 128 + sent_signal, stderr + stdout
                    )
                    self.assertIn("restoring the previous installed bytes", stderr)
                    self.assertEqual(
                        (installed_release / "bin/codex").read_bytes(), old_codex_bytes
                    )
                    self.assertEqual(receipt_path.read_bytes(), old_receipt_bytes)
                    self.assertEqual(os.readlink(current), old_current_target)
                    self.assertEqual(os.readlink(visible), old_visible_target)
                    self.assertEqual(
                        subprocess.run(
                            [visible, "--version"],
                            capture_output=True,
                            text=True,
                            check=True,
                        ).stdout,
                        f"codex-cli {version}\n",
                    )
                    wait_for_process_exit(verification_pid)
                    wait_for_process_group_exit(process.pid)
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=2)

    def test_activation_failure_restores_previous_current_and_visible_links(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            install_bin = root / "install-bin"
            install_bin.mkdir()
            standalone = root / "codex-home" / "packages" / "standalone"
            previous = standalone / "previous"
            previous.mkdir(parents=True)
            write_executable(
                previous / "codex", "#!/bin/sh\nprintf 'codex-cli 0.9.0\\n'\n"
            )
            current = standalone / "current"
            current.symlink_to(previous)
            visible = install_bin / "codex"
            visible.symlink_to(current / "codex")
            digests = create_release_assets(root, "1.12.0", fail_during_activation=True)

            result, _requests = run_installer(
                root, selector="1.12.0", exact=release_metadata("1.12.0", digests)
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("restoring the previous runnable installation", result.stderr)
            self.assertEqual(os.readlink(current), str(previous))
            self.assertEqual(os.readlink(visible), str(current / "codex"))
            self.assertEqual(
                subprocess.run(
                    [visible, "--version"], capture_output=True, text=True, check=True
                ).stdout,
                "codex-cli 0.9.0\n",
            )

    def test_activation_rollback_failure_does_not_skip_remaining_cleanup(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            install_bin = root / "install-bin"
            install_bin.mkdir()
            standalone = root / "codex-home/packages/standalone"
            previous = standalone / "previous"
            previous.mkdir(parents=True)
            write_executable(
                previous / "codex", "#!/bin/sh\nprintf 'codex-cli 0.9.0\\n'\n"
            )
            current = standalone / "current"
            current.symlink_to(previous)
            visible = install_bin / "codex"
            visible.symlink_to(current / "codex")
            digests = create_release_assets(root, "1.12.2", fail_during_activation=True)
            invocation = prepare_installer(
                root,
                selector="1.12.2",
                exact=release_metadata("1.12.2", digests),
                force_fallback_lock=True,
            )
            invocation.env["TMPDIR"] = str(root)
            install_rm_failure(invocation.env, failed_path=visible)
            result = run_prepared_installer(invocation)

            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn(
                "Could not restore the previous visible Codex command", result.stderr
            )
            self.assertIn("Activation rollback encountered", result.stderr)
            self.assertEqual(os.readlink(current), str(previous))
            rm_log = (root / "rm-targets.log").read_text(encoding="utf-8")
            self.assertIn(str(current), rm_log)
            self.assertIn(str(visible), rm_log)
            self.assertIn(str(install_bin / "codex-code-mode-host"), rm_log)
            self.assertIn(str(standalone / "install.lock.d"), rm_log)
            self.assertFalse((standalone / "install.lock.d").exists())
            self.assertEqual(list(standalone.glob("install.lock.owner.*")), [])
            self.assertEqual(list(root.glob("tmp.*")), [])

    def test_stage_cleanup_failure_does_not_skip_lock_or_temp_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            digests = create_release_assets(root, "1.12.3")
            invocation = prepare_installer(
                root,
                selector="1.12.3",
                exact=release_metadata("1.12.3", digests),
                force_fallback_lock=True,
            )
            invocation.env["TMPDIR"] = str(root)
            install_extracting_tar_failure(invocation.env)
            install_rm_failure(invocation.env, fail_staging_cleanup=True)
            result = run_prepared_installer(invocation)

            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("Could not remove staged release", result.stderr)
            self.assertIn("Installer cleanup encountered", result.stderr)
            standalone = root / "codex-home/packages/standalone"
            releases = standalone / "releases/Electivus/electivus-codex"
            self.assertEqual(len(list(releases.glob(".staging.*"))), 1)
            self.assertFalse((standalone / "install.lock.d").exists())
            self.assertEqual(list(standalone.glob("install.lock.owner.*")), [])
            self.assertEqual(list(root.glob("tmp.*")), [])

    def test_signals_during_visible_verification_restore_previous_activation(
        self,
    ) -> None:
        for sent_signal in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
            with (
                self.subTest(signal=sent_signal),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                install_bin = root / "install-bin"
                install_bin.mkdir()
                standalone = root / "codex-home" / "packages" / "standalone"
                previous = standalone / "previous"
                (previous / "bin").mkdir(parents=True)
                write_executable(
                    previous / "bin" / "codex",
                    "#!/bin/sh\nprintf 'codex-cli 0.9.0\\n'\n",
                )
                write_executable(
                    previous / "bin" / "codex-code-mode-host",
                    "#!/bin/sh\nexit 0\n",
                )
                current = standalone / "current"
                current.symlink_to(previous)
                visible = install_bin / "codex"
                visible.symlink_to(current / "bin" / "codex")
                visible_code_mode_host = install_bin / "codex-code-mode-host"
                visible_code_mode_host.symlink_to(
                    current / "bin" / "codex-code-mode-host"
                )
                activation_gate = root / "activation.gate"
                os.mkfifo(activation_gate)
                digests = create_release_assets(
                    root, "1.12.1", block_during_activation=True
                )
                invocation = prepare_installer(
                    root,
                    selector="1.12.1",
                    exact=release_metadata("1.12.1", digests),
                )
                invocation.env["CODEX_TEST_ACTIVATION_GATE"] = str(activation_gate)
                process = subprocess.Popen(
                    invocation.args,
                    env=invocation.env,
                    start_new_session=True,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                try:
                    wait_for_path(root / "activation.ready")
                    verification_pid = int(
                        (root / "activation.pid").read_text(encoding="utf-8")
                    )
                    os.kill(process.pid, sent_signal)
                    stdout, stderr = communicate_bounded(process)

                    self.assertEqual(
                        process.returncode,
                        128 + sent_signal,
                        stderr + stdout,
                    )
                    self.assertEqual(os.readlink(current), str(previous))
                    self.assertEqual(
                        os.readlink(visible), str(current / "bin" / "codex")
                    )
                    self.assertEqual(
                        os.readlink(visible_code_mode_host),
                        str(current / "bin" / "codex-code-mode-host"),
                    )
                    self.assertEqual(
                        subprocess.run(
                            [visible, "--version"],
                            capture_output=True,
                            text=True,
                            check=True,
                        ).stdout,
                        "codex-cli 0.9.0\n",
                    )
                    wait_for_process_exit(verification_pid)
                    wait_for_process_group_exit(process.pid)
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=2)


@dataclass(frozen=True)
class InstallerInvocation:
    args: list[str]
    env: dict[str, str]
    request_log: Path


def run_installer(
    root: Path,
    **options: object,
) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    invocation = prepare_installer(root, **options)
    result = subprocess.run(
        invocation.args,
        capture_output=True,
        check=False,
        env=invocation.env,
        text=True,
    )
    return result, read_requests(invocation.request_log)


def run_prepared_installer(
    invocation: InstallerInvocation,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        invocation.args,
        capture_output=True,
        check=False,
        env=invocation.env,
        text=True,
    )


def prepare_installer(
    root: Path,
    *,
    selector: str | None = "stable",
    inventory: list[dict[str, object]] | None = None,
    inventory_pages: list[object] | None = None,
    exact: dict[str, object] | str | bytes | None = None,
    channel: str = "",
    protocol: str = "",
    installer_digest: str = "",
    force_macos: bool = False,
    force_fallback_lock: bool = False,
    metadata_mode: str = "",
    transport: str = "curl",
) -> InstallerInvocation:
    fake_bin = root / "fake-bin"
    fake_bin.mkdir(parents=True, exist_ok=True)
    metadata_dir = root / "metadata"
    metadata_dir.mkdir(exist_ok=True)
    exact_path = metadata_dir / "exact.json"
    if isinstance(exact, bytes):
        exact_path.write_bytes(exact)
    else:
        exact_document = exact if isinstance(exact, str) else json.dumps(exact or {})
        exact_path.write_text(exact_document, encoding="utf-8")
    pages = inventory_pages if inventory_pages is not None else [inventory or []]
    for page_number, page_document in enumerate(pages, start=1):
        page_path = metadata_dir / f"page-{page_number}.json"
        if isinstance(page_document, bytes):
            page_path.write_bytes(page_document)
        else:
            page_path.write_text(
                page_document
                if isinstance(page_document, str)
                else json.dumps(page_document),
                encoding="utf-8",
            )
    request_log = root / "requests.log"
    fake_curl = fake_bin / "curl"
    curl_script = textwrap.dedent(
        """\
            #!/bin/sh
            url=""
            output=""
            previous=""
            for arg in "$@"; do
              case "$arg" in https://*) url="$arg" ;; esac
              if [ "$previous" = "-o" ]; then output="$arg"; fi
              previous="$arg"
            done
            printf '%s\n' "$url" >>"$CODEX_TEST_REQUEST_LOG"
            case "$url" in
              *openai*) exit 88 ;;
              https://api.github.com/repos/Electivus/electivus-codex/releases/tags/*)
                if [ "$CODEX_TEST_METADATA_MODE" = "slow-oversized" ]; then
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
                elif [ "$CODEX_TEST_METADATA_MODE" = "oversized" ]; then
                  dd if=/dev/zero bs=1048577 count=1 2>/dev/null | tr '\\000' x
                else
                  cat "$CODEX_TEST_METADATA_DIR/exact.json"
                fi
                ;;
              'https://api.github.com/repos/Electivus/electivus-codex/releases?per_page=100&page='*)
                page="${url##*page=}"
                if [ -f "$CODEX_TEST_METADATA_DIR/page-$page.json" ]; then
                  cat "$CODEX_TEST_METADATA_DIR/page-$page.json"
                else
                  printf '[]\n'
                fi
                ;;
              https://github.com/Electivus/electivus-codex/releases/download/*)
                asset="${url##*/}"
                if { [ "$CODEX_TEST_METADATA_MODE" = "hold-first-manifest" ] ||
                    [ "$CODEX_TEST_METADATA_MODE" = "blocked-curl" ]; } &&
                  [ "$asset" = "codex-package_SHA256SUMS" ] &&
                  mkdir "$CODEX_TEST_ROOT/download-holder" 2>/dev/null; then
                  printf '%s\n' "$$" >"$CODEX_TEST_ROOT/downloader.pid"
                  : >"$CODEX_TEST_ROOT/download.ready"
                  while [ ! -e "$CODEX_TEST_DOWNLOAD_CONTINUE" ]; do
                    sleep 0.01
                  done
                fi
                cat "$CODEX_TEST_ASSET_DIR/$asset"
                ;;
              *) exit 89 ;;
            esac
            """
    )
    if transport == "curl":
        write_executable(fake_curl, curl_script)
    if transport == "wget" or force_fallback_lock:
        for command in (
            "awk",
            "basename",
            "cat",
            "chmod",
            "cmp",
            "cp",
            "date",
            "dd",
            "dirname",
            "find",
            "grep",
            "gzip",
            "head",
            "ln",
            "mkdir",
            "mkfifo",
            "mktemp",
            "mv",
            "od",
            "python3",
            "readlink",
            "rm",
            "sed",
            "sha256sum",
            "sleep",
            "sort",
            "tar",
            "tr",
            "wc",
        ):
            command_path = shutil.which(command)
            assert command_path is not None
            (fake_bin / command).symlink_to(command_path)
        fake_head = fake_bin / "head"
        fake_head.unlink()
        real_head = shutil.which("head")
        assert real_head is not None
        write_executable(
            fake_head,
            textwrap.dedent(
                f"""\
                #!/bin/sh
                printf '%s\\n' "$$" >>"$CODEX_TEST_ROOT/head.pids"
                exec "{real_head}" "$@"
                """
            ),
        )
        if transport == "wget":
            write_executable(
                fake_bin / "wget",
                textwrap.dedent(
                    """\
                #!/bin/sh
                url=""
                output=""
                previous=""
                for arg in "$@"; do
                  case "$arg" in https://*) url="$arg" ;; esac
                  if [ "$previous" = "-O" ]; then output="$arg"; fi
                  previous="$arg"
                done
                printf '%s\n' "$url" >>"$CODEX_TEST_REQUEST_LOG"
                case "$url" in
                  https://api.github.com/repos/Electivus/electivus-codex/releases/tags/*)
                    if [ "$CODEX_TEST_METADATA_MODE" = "drip-wget" ]; then
                      printf '%s\n' "$$" >"$CODEX_TEST_ROOT/downloader.pid"
                      : >"$CODEX_TEST_ROOT/download.ready"
                      while :; do
                        printf x
                        sleep 0.1
                      done
                    elif [ "$CODEX_TEST_METADATA_MODE" = "oversized" ]; then
                      dd if=/dev/zero bs=1048577 count=1 2>/dev/null | tr '\\000' x >"$output"
                    else
                      cp "$CODEX_TEST_METADATA_DIR/exact.json" "$output"
                    fi
                    ;;
                  'https://api.github.com/repos/Electivus/electivus-codex/releases?per_page=100&page='*)
                    page="${url##*page=}"
                    if [ -f "$CODEX_TEST_METADATA_DIR/page-$page.json" ]; then
                      cp "$CODEX_TEST_METADATA_DIR/page-$page.json" "$output"
                    else
                      printf '[]\n' >"$output"
                    fi
                    ;;
                  https://github.com/Electivus/electivus-codex/releases/download/*)
                    asset="${url##*/}"
                    cp "$CODEX_TEST_ASSET_DIR/$asset" "$output"
                    ;;
                  *) exit 89 ;;
                esac
                    """
                ),
            )
    if force_macos or transport == "wget" or force_fallback_lock:
        fake_uname = fake_bin / "uname"
        write_executable(
            fake_uname,
            '#!/bin/sh\ncase "$1" in '
            f"-s) echo {'Darwin' if force_macos else 'Linux'} ;; "
            f"-m) echo {'arm64' if force_macos else 'x86_64'} ;; esac\n",
        )

    home = root / "home"
    home.mkdir(exist_ok=True)
    env = os.environ.copy()
    env.update(
        {
            "CODEX_HOME": str(root / "codex-home"),
            "CODEX_INSTALL_DIR": str(root / "install-bin"),
            "CODEX_NON_INTERACTIVE": "1",
            "CODEX_TEST_ASSET_DIR": str(root / "assets"),
            "CODEX_TEST_METADATA_DIR": str(metadata_dir),
            "CODEX_TEST_METADATA_MODE": metadata_mode,
            "CODEX_TEST_REQUEST_LOG": str(request_log),
            "CODEX_TEST_ROOT": str(root),
            "CODEX_TEST_DOWNLOAD_CONTINUE": str(root / "allow-download"),
            "HOME": str(home),
            "PATH": str(fake_bin)
            if transport == "wget" or force_fallback_lock
            else f"{fake_bin}:/usr/bin:/bin",
            "SHELL": "/bin/sh",
        }
    )
    args = ["/bin/sh", str(INSTALL_SCRIPT)]
    if selector is not None:
        args.extend(("--release", selector))
    if channel:
        args.extend(("--channel", channel))
    if protocol:
        args.extend(("--installer-protocol", protocol))
    if installer_digest:
        args.extend(("--installer-digest", installer_digest))
    return InstallerInvocation(args=args, env=env, request_log=request_log)


def read_requests(request_log: Path) -> list[str]:
    return (
        request_log.read_text(encoding="utf-8").splitlines()
        if request_log.exists()
        else []
    )


def create_release_assets(
    root: Path,
    version: str,
    *,
    fail_during_activation: bool = False,
    block_during_activation: bool = False,
) -> dict[str, str | None]:
    assets = root / "assets"
    assets.mkdir(exist_ok=True)
    package = root / "package"
    (package / "bin").mkdir(parents=True, exist_ok=True)
    (package / "codex-path").mkdir(exist_ok=True)
    (package / "codex-resources").mkdir(exist_ok=True)
    (package / "codex-package.json").write_text("{}\n", encoding="utf-8")
    if fail_during_activation or block_during_activation:
        counter = root / "candidate-invocations"
        activation_behavior = ""
        if fail_during_activation:
            activation_behavior = 'if [ "$count" -ge 3 ]; then exit 1; fi'
        else:
            activation_behavior = f"""if [ "$count" -ge 3 ]; then
  printf '%s\\n' "$$" >'{root / "activation.pid"}'
  : >'{root / "activation.ready"}'
  IFS= read -r _continue <"$CODEX_TEST_ACTIVATION_GATE"
fi"""
        codex_body = f"""#!/bin/sh
count=0
if [ -f '{counter}' ]; then count=$(cat '{counter}'); fi
count=$((count + 1))
printf '%s\n' "$count" >'{counter}'
{activation_behavior}
printf 'codex-cli {version}\n'
"""
    else:
        codex_body = f"#!/bin/sh\nprintf 'codex-cli {version}\\n'\n"
    write_executable(package / "bin" / "codex", codex_body)
    write_executable(package / "bin" / "codex-code-mode-host", "#!/bin/sh\nexit 0\n")
    write_executable(package / "codex-path" / "rg", "#!/bin/sh\nexit 0\n")
    write_executable(package / "codex-resources" / "bwrap", "#!/bin/sh\nexit 0\n")

    target_archive = assets / f"codex-package-{TARGET}.tar.gz"
    with tarfile.open(target_archive, "w:gz") as archive:
        for path in package.iterdir():
            archive.add(path, arcname=path.name)
    for asset in PACKAGE_ASSETS:
        path = assets / asset
        if not path.exists():
            path.write_bytes(f"fixture for {asset}\n".encode())

    package_digests = {asset: sha256(assets / asset) for asset in PACKAGE_ASSETS}
    package_manifest = assets / "codex-package_SHA256SUMS"
    package_manifest.write_text(
        "".join(f"{package_digests[asset]}  {asset}\n" for asset in PACKAGE_ASSETS),
        encoding="utf-8",
    )
    (assets / "install.sh").write_bytes(INSTALL_SCRIPT.read_bytes())
    (assets / "install.ps1").write_text(
        "# Electivus installer fixture\n", encoding="utf-8"
    )
    installer_digests = {
        asset: sha256(assets / asset) for asset in ("install.sh", "install.ps1")
    }
    installer_manifest = assets / "installer_SHA256SUMS"
    installer_manifest.write_text(
        "".join(
            f"{installer_digests[asset]}  {asset}\n"
            for asset in ("install.sh", "install.ps1")
        ),
        encoding="utf-8",
    )
    return {
        **package_digests,
        "codex-package_SHA256SUMS": sha256(package_manifest),
        **installer_digests,
        "installer_SHA256SUMS": sha256(installer_manifest),
    }


def release_metadata(
    version: str,
    digests: dict[str, str | None],
    *,
    draft: bool = False,
    published: bool = True,
    omit: set[str] | None = None,
) -> dict[str, object]:
    omitted = omit or set()
    prerelease = "-" in version.split("+", 1)[0]
    return {
        "published_at": "2026-08-25T00:00:00Z" if published else None,
        "assets": [
            {
                "name": asset,
                "digest": f"sha256:{digests[asset]}"
                if digests[asset] is not None
                else None,
                "state": "uploaded",
                "size": 1,
            }
            for asset in REQUIRED_ASSETS
            if asset not in omitted
        ],
        "prerelease": prerelease,
        "tag_name": f"electivus-v{version}",
        "draft": draft,
    }


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


def assign_nested(
    value: dict[str, object], path: tuple[object, ...], replacement: object
) -> None:
    container: object = value
    for component in path[:-1]:
        if isinstance(component, str):
            assert isinstance(container, dict)
            container = container[component]
        else:
            assert isinstance(container, list)
            container = container[component]
    final = path[-1]
    assert isinstance(final, str)
    assert isinstance(container, dict)
    container[final] = replacement


def release_dir(root: Path, version: str) -> Path:
    return (
        root
        / "codex-home"
        / "packages"
        / "standalone"
        / "releases"
        / "Electivus"
        / "electivus-codex"
        / version
        / TARGET
    )


def read_receipt(root: Path, version: str) -> dict[str, str]:
    return json.loads(
        (release_dir(root, version) / "installation-receipt.json").read_text(
            encoding="utf-8"
        )
    )


def clear_requests(root: Path) -> None:
    (root / "requests.log").unlink(missing_ok=True)


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
    if (root / "head.pids").exists():
        pids.extend(
            int(pid)
            for pid in (root / "head.pids").read_text(encoding="utf-8").splitlines()
        )
    return pids


def communicate_bounded(process: subprocess.Popen[str]) -> tuple[str, str]:
    try:
        return process.communicate(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate(timeout=2)
        raise AssertionError(
            f"installer did not exit promptly after a direct signal: {stderr}{stdout}"
        )


def assert_interrupted_install_left_no_state(root: Path, version: str) -> None:
    standalone_root = root / "codex-home/packages/standalone"
    self_owned_artifacts = (
        standalone_root / "install.lock.d",
        root / "install-bin/codex",
        standalone_root / "current",
        release_dir(root, version),
    )
    for artifact in self_owned_artifacts:
        if artifact.exists() or artifact.is_symlink():
            raise AssertionError(f"interrupted installer leaked {artifact}")
    leaked = [
        path
        for pattern in (
            "tmp.*",
            "**/*.fifo",
            "**/install.lock.owner.*",
            "**/*.reclaim.*",
        )
        for path in root.glob(pattern)
    ]
    if leaked:
        raise AssertionError(f"interrupted installer leaked temporary state: {leaked}")


def wait_for_path(path: Path) -> None:
    deadline = time.monotonic() + 5
    while not path.exists():
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {path}")
        time.sleep(0.01)


def wait_for_glob_count(pattern: str, count: int) -> None:
    deadline = time.monotonic() + 5
    parent = Path(pattern).parent
    name = Path(pattern).name
    while len(list(parent.glob(name))) < count:
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {count} matches of {pattern}")
        time.sleep(0.01)


def install_flock_gate(env: dict[str, str]) -> None:
    write_executable(
        Path(env["PATH"].split(os.pathsep, maxsplit=1)[0]) / "flock",
        textwrap.dedent(
            """\
            #!/bin/sh
            : >"$CODEX_TEST_ROOT/flock.ready"
            IFS= read -r _continue <"$CODEX_TEST_FLOCK_GATE"
            exec /usr/bin/flock "$@"
            """
        ),
    )


def install_failing_hard_link(env: dict[str, str]) -> None:
    fake_ln = Path(env["PATH"]) / "ln"
    fake_ln.unlink()
    write_executable(fake_ln, "#!/bin/sh\nexit 73\n")


def install_rm_failure(
    env: dict[str, str],
    *,
    failed_path: Path | None = None,
    fail_staging_cleanup: bool = False,
    fail_persistently: bool = False,
    fail_reclaim_stale_cleanup: bool = False,
    fail_reclaim_snapshot_cleanup: bool = False,
) -> None:
    fake_rm = Path(env["PATH"]) / "rm"
    fake_rm.unlink()
    real_rm = shutil.which("rm")
    assert real_rm is not None
    env["CODEX_TEST_FAIL_RM_PATH"] = str(failed_path or "")
    env["CODEX_TEST_FAIL_STAGE_CLEANUP"] = "1" if fail_staging_cleanup else ""
    env["CODEX_TEST_FAIL_RM_PERSISTENT"] = "1" if fail_persistently else ""
    env["CODEX_TEST_FAIL_RECLAIM_STALE"] = "1" if fail_reclaim_stale_cleanup else ""
    env["CODEX_TEST_FAIL_RECLAIM_SNAPSHOT"] = (
        "1" if fail_reclaim_snapshot_cleanup else ""
    )
    write_executable(
        fake_rm,
        textwrap.dedent(
            f"""\
            #!/bin/sh
            last=""
            for argument in "$@"; do last="$argument"; done
            printf '%s\n' "$last" >>"$CODEX_TEST_ROOT/rm-targets.log"
            if [ "$last" = "$CODEX_TEST_FAIL_RM_PATH" ]; then
              if [ -n "$CODEX_TEST_FAIL_RM_PERSISTENT" ] ||
                mkdir "$CODEX_TEST_ROOT/fail-rm-once" 2>/dev/null; then
                exit 74
              fi
            fi
            case "$last" in
            *.stale.*)
              if [ -n "$CODEX_TEST_FAIL_RECLAIM_STALE" ]; then
                exit 74
              fi
              ;;
            *.snapshot.*)
              if [ -n "$CODEX_TEST_FAIL_RECLAIM_SNAPSHOT" ]; then
                exit 74
              fi
              ;;
            */.staging.*)
              if [ -n "$CODEX_TEST_FAIL_STAGE_CLEANUP" ] &&
                mkdir "$CODEX_TEST_ROOT/saw-initial-stage-rm" 2>/dev/null; then
                exec "{real_rm}" "$@"
              elif [ -n "$CODEX_TEST_FAIL_STAGE_CLEANUP" ] &&
                mkdir "$CODEX_TEST_ROOT/fail-stage-rm-once" 2>/dev/null; then
                exit 75
              fi
              ;;
            esac
            exec "{real_rm}" "$@"
            """
        ),
    )


def install_extracting_tar_failure(env: dict[str, str]) -> None:
    fake_tar = Path(env["PATH"]) / "tar"
    fake_tar.unlink()
    real_tar = shutil.which("tar")
    assert real_tar is not None
    write_executable(
        fake_tar,
        f'#!/bin/sh\n"{real_tar}" "$@"\nexit 76\n',
    )


def install_reclaim_marker_mv_failure(
    env: dict[str, str], *, reports_success: bool = False
) -> None:
    fake_mv = Path(env["PATH"]) / "mv"
    fake_mv.unlink()
    real_mv = shutil.which("mv")
    assert real_mv is not None
    env["CODEX_TEST_MV_REPORTS_SUCCESS"] = "1" if reports_success else ""
    write_executable(
        fake_mv,
        textwrap.dedent(
            f"""\
            #!/bin/sh
            last=""
            for argument in "$@"; do last="$argument"; done
            case "$last" in
            *.reclaim.*)
              if [ -n "$CODEX_TEST_MV_REPORTS_SUCCESS" ]; then exit 0; fi
              exit 77
              ;;
            esac
            exec "{real_mv}" "$@"
            """
        ),
    )


def install_release_restore_destination_race(
    env: dict[str, str], *, restored_path: Path
) -> None:
    fake_mv = Path(env["PATH"]) / "mv"
    fake_mv.unlink()
    real_mv = shutil.which("mv")
    assert real_mv is not None
    env["CODEX_TEST_RESTORED_RELEASE_PATH"] = str(restored_path)
    write_executable(
        fake_mv,
        textwrap.dedent(
            f"""\
            #!/bin/sh
            source=""
            destination=""
            for argument in "$@"; do
              case "$argument" in
              -*) ;;
              *) source="$destination"; destination="$argument" ;;
              esac
            done
            case "$source:$destination" in
            */.rollback.*:"$CODEX_TEST_RESTORED_RELEASE_PATH")
              if mkdir "$CODEX_TEST_ROOT/recreated-release-once" 2>/dev/null; then
                mkdir -p "$destination"
                printf 'concurrent\n' >"$destination/concurrent-owner"
              fi
              ;;
            esac
            exec "{real_mv}" "$@"
            """
        ),
    )


def install_reclaim_marker_date_failure(env: dict[str, str]) -> None:
    fake_date = Path(env["PATH"]) / "date"
    fake_date.unlink()
    real_date = shutil.which("date")
    assert real_date is not None
    write_executable(
        fake_date,
        textwrap.dedent(
            f"""\
            #!/bin/sh
            if mkdir "$CODEX_TEST_ROOT/date-call-1" 2>/dev/null; then
              exec "{real_date}" "$@"
            fi
            if mkdir "$CODEX_TEST_ROOT/date-call-2" 2>/dev/null; then
              exec "{real_date}" "$@"
            fi
            exit 78
            """
        ),
    )


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
              if mkdir "$CODEX_TEST_ROOT/guard-unlink-once" 2>/dev/null; then
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


def inventory_url(page: int) -> str:
    return (
        "https://api.github.com/repos/Electivus/electivus-codex/"
        f"releases?per_page=100&page={page}"
    )


def exact_url(version: str) -> str:
    return (
        "https://api.github.com/repos/Electivus/electivus-codex/"
        f"releases/tags/electivus-v{version}"
    )


def asset_url(version: str, asset: str) -> str:
    return (
        "https://github.com/Electivus/electivus-codex/releases/download/"
        f"electivus-v{version}/{asset}"
    )


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_executable(path: Path, contents: str) -> None:
    path.write_text(contents, encoding="utf-8")
    path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
