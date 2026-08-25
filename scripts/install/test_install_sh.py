#!/usr/bin/env python3

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import textwrap
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
                    "installer_protocol": "installer-v1",
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
            self.assertEqual(len(requests), 2)
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
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            receipt = read_receipt(root, "1.4.3")
            self.assertEqual(receipt["update_channel"], "pre-release")
            self.assertEqual(receipt["installer_protocol"], "installer-v1")

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

    def test_macos_fails_before_any_network_request(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result, requests = run_installer(Path(temp_dir), force_macos=True)

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(requests, [])
            self.assertIn(
                "does not yet publish or validate standalone macOS", result.stderr
            )
            self.assertIn("will not fall back to OpenAI", result.stderr)

    def test_namespaced_cache_requires_an_exact_receipt_match(self) -> None:
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
            self.assertEqual(len(second_requests), 1)
            self.assertNotIn("Downloading Electivus checksum manifests", second.stdout)

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


def run_installer(
    root: Path,
    *,
    selector: str | None = "stable",
    inventory: list[dict[str, object]] | None = None,
    exact: dict[str, object] | str | None = None,
    channel: str = "",
    protocol: str = "",
    force_macos: bool = False,
    metadata_mode: str = "",
) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    fake_bin = root / "fake-bin"
    fake_bin.mkdir(parents=True, exist_ok=True)
    request_log = root / "requests.log"
    fake_curl = fake_bin / "curl"
    fake_curl.write_text(
        textwrap.dedent(
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
                if [ "$CODEX_TEST_METADATA_MODE" = "oversized" ]; then
                  dd if=/dev/zero bs=1048577 count=1 2>/dev/null | tr '\\000' x >"$output"
                else
                  printf '%s\n' "$CODEX_TEST_EXACT_METADATA" >"$output"
                fi
                ;;
              'https://api.github.com/repos/Electivus/electivus-codex/releases?per_page=100&page=1')
                printf '%s\n' "$CODEX_TEST_INVENTORY_METADATA" >"$output"
                ;;
              'https://api.github.com/repos/Electivus/electivus-codex/releases?per_page=100&page='*)
                printf '[]\n' >"$output"
                ;;
              https://github.com/Electivus/electivus-codex/releases/download/*)
                asset="${url##*/}"
                cp "$CODEX_TEST_ASSET_DIR/$asset" "$output"
                ;;
              *) exit 89 ;;
            esac
            """
        ),
        encoding="utf-8",
    )
    fake_curl.chmod(0o755)
    if force_macos:
        fake_uname = fake_bin / "uname"
        write_executable(
            fake_uname,
            '#!/bin/sh\ncase "$1" in -s) echo Darwin ;; -m) echo arm64 ;; esac\n',
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
            "CODEX_TEST_EXACT_METADATA": (
                exact if isinstance(exact, str) else json.dumps(exact or {})
            ),
            "CODEX_TEST_INVENTORY_METADATA": json.dumps(inventory or []),
            "CODEX_TEST_METADATA_MODE": metadata_mode,
            "CODEX_TEST_REQUEST_LOG": str(request_log),
            "HOME": str(home),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
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
    result = subprocess.run(args, capture_output=True, check=False, env=env, text=True)
    requests = (
        request_log.read_text(encoding="utf-8").splitlines()
        if request_log.exists()
        else []
    )
    return result, requests


def create_release_assets(
    root: Path, version: str, *, fail_during_activation: bool = False
) -> dict[str, str | None]:
    assets = root / "assets"
    assets.mkdir(exist_ok=True)
    package = root / "package"
    (package / "bin").mkdir(parents=True)
    (package / "codex-path").mkdir()
    (package / "codex-resources").mkdir()
    (package / "codex-package.json").write_text("{}\n", encoding="utf-8")
    if fail_during_activation:
        counter = root / "candidate-invocations"
        codex_body = f"""#!/bin/sh
count=0
if [ -f '{counter}' ]; then count=$(cat '{counter}'); fi
count=$((count + 1))
printf '%s\n' "$count" >'{counter}'
if [ "$count" -ge 2 ]; then exit 1; fi
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
            }
            for asset in REQUIRED_ASSETS
            if asset not in omitted
        ],
        "prerelease": prerelease,
        "tag_name": f"electivus-v{version}",
        "draft": draft,
    }


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


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_executable(path: Path, contents: str) -> None:
    path.write_text(contents, encoding="utf-8")
    path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
