import subprocess
import tempfile
import unittest
from pathlib import Path

from v8_canary_changes import CanaryDecision
from v8_canary_changes import CanaryMetadata
from v8_canary_changes import EXACT_V8_IRRELEVANT_PATHS
from v8_canary_changes import changed_files
from v8_canary_changes import canary_required
from v8_canary_changes import classify_changed_files
from v8_canary_changes import decision_for_revisions
from v8_canary_changes import merge_base
from v8_canary_changes import metadata_for_revisions
from v8_canary_changes import resolved_v8_version
from v8_canary_changes import windows_source_required


class V8CanaryChangesTest(unittest.TestCase):
    def test_non_authoritative_shadow_validation_paths_are_irrelevant(self) -> None:
        paths = {
            ".github/scripts/validation_contracts.py",
            ".github/scripts/test_validation.py",
            ".github/scripts/test_validation_contracts.py",
            ".github/scripts/check_validation_topology.py",
            ".github/scripts/test_check_validation_topology.py",
            ".github/scripts/legacy_validation_observation.py",
            ".github/workflows/validation-shadow.yml",
            ".github/workflows/validation-integrated.yaml",
        }

        self.assertEqual(
            CanaryDecision(
                False,
                "all 8 changed paths are explicitly V8-irrelevant",
            ),
            classify_changed_files(paths, "149.2.0", "149.2.0"),
        )

    def test_exact_installer_and_version_paths_are_irrelevant(self) -> None:
        expected_paths = {
            "scripts/codex_package/test_version.py",
            "scripts/codex_package/version.py",
            "scripts/install/install-local.sh",
            "scripts/install/install.sh",
            "scripts/install/installer-v1.sh",
            "scripts/install/test_install_local_sh.py",
            "scripts/install/test_install_sh.py",
            "scripts/install/test_installer_v1_sh.py",
        }
        self.assertEqual(expected_paths, EXACT_V8_IRRELEVANT_PATHS)

        for path in expected_paths:
            with self.subTest(path=path):
                self.assertEqual(
                    CanaryDecision(
                        False,
                        "all 1 changed path is explicitly V8-irrelevant",
                    ),
                    classify_changed_files({path}, "149.2.0", "149.2.0"),
                )

    def test_nearby_installer_and_package_paths_remain_fail_closed(self) -> None:
        relevant_paths = (
            ".github/scripts/check_installer_v2_topology.py",
            ".github/scripts/validation.py",
            ".github/scripts/validation_contracts.py.backup",
            ".github/workflows/electivus-release.yml",
            ".github/workflows/installer-v2-release.yml",
            ".github/workflows/validation.yml",
            ".github/workflows/validation-shadow.yml.backup",
            "scripts/build_codex_package.py",
            "scripts/codex_package/cargo.py",
            "scripts/codex_package/test_cargo.py",
            "scripts/codex_package/v8.py",
            "scripts/codex_package/version_helper.py",
            "scripts/install/helper.py",
            "scripts/install/install.sh.backup",
            "scripts/install/installer-v2.sh",
            "scripts/install/nested/install.sh",
            "scripts/install/test_install.py",
        )

        for path in relevant_paths:
            with self.subTest(path=path):
                self.assertEqual(
                    CanaryDecision(True, f"unknown V8 impact: {path}"),
                    classify_changed_files({path}, "149.2.0", "149.2.0"),
                )

    def test_mixed_installer_changes_require_canary(self) -> None:
        cases = (
            (
                "scripts/codex_package/v8.py",
                CanaryDecision(
                    True,
                    "unknown V8 impact: scripts/codex_package/v8.py",
                ),
            ),
            (
                "third_party/v8/BUILD.bazel",
                CanaryDecision(
                    True,
                    "V8 canary path changed: third_party/v8/BUILD.bazel",
                ),
            ),
        )
        for path, expected in cases:
            with self.subTest(path=path):
                self.assertEqual(
                    expected,
                    classify_changed_files(
                        {
                            "scripts/codex_package/version.py",
                            "scripts/install/install.sh",
                            path,
                        },
                        "149.2.0",
                        "149.2.0",
                    ),
                )

    def test_relevant_known_irrelevant_and_unknown_paths(self) -> None:
        cases = (
            (
                {"codex-rs/v8-poc/Cargo.toml"},
                CanaryDecision(True, "V8 canary path changed: codex-rs/v8-poc/Cargo.toml"),
            ),
            (
                {"codex-rs/v8-poc/BUILD.bazel"},
                CanaryDecision(True, "V8 canary path changed: codex-rs/v8-poc/BUILD.bazel"),
            ),
            (
                {"codex-rs/v8-poc/src/lib.rs"},
                CanaryDecision(True, "V8 canary path changed: codex-rs/v8-poc/src/lib.rs"),
            ),
            (
                {"third_party/v8/BUILD.bazel"},
                CanaryDecision(True, "V8 canary path changed: third_party/v8/BUILD.bazel"),
            ),
            (
                {"codex-rs/core/src/lib.rs", "docs/architecture.md"},
                CanaryDecision(False, "all 2 changed paths are explicitly V8-irrelevant"),
            ),
            (
                {"new-top-level/tool.py"},
                CanaryDecision(True, "unknown V8 impact: new-top-level/tool.py"),
            ),
        )
        for paths, expected in cases:
            with self.subTest(paths=paths):
                self.assertEqual(
                    expected,
                    classify_changed_files(paths, "149.2.0", "149.2.0"),
                )
        self.assertEqual(
            CanaryDecision(True, "v8 version changed from 149.2.0 to 150.0.0"),
            classify_changed_files(set(), "149.2.0", "150.0.0"),
        )
        self.assertEqual(
            CanaryDecision(True, "comparison returned no changed paths"),
            classify_changed_files(set(), "149.2.0", "149.2.0"),
        )

    def test_manual_missing_comparison_and_git_error_require_canary(self) -> None:
        self.assertEqual(
            CanaryDecision(True, "manual workflow dispatch"),
            decision_for_revisions(None, None, force=True),
        )
        self.assertEqual(
            CanaryDecision(True, "comparison is missing base or head"),
            decision_for_revisions(None, "head"),
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            self.assertEqual(
                CanaryDecision(True, "comparison failed (CalledProcessError)"),
                decision_for_revisions(
                    "missing-base", "missing-head", root=Path(temp_dir)
                ),
            )

    def test_windows_source_metadata_fails_closed(self) -> None:
        self.assertEqual(
            CanaryMetadata(
                CanaryDecision(True, "manual workflow dispatch"),
                windows_source_required=True,
            ),
            metadata_for_revisions(None, None, force=True),
        )
        self.assertEqual(
            CanaryMetadata(
                CanaryDecision(True, "comparison is missing base or head"),
                windows_source_required=True,
            ),
            metadata_for_revisions(None, "head"),
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            self.assertEqual(
                CanaryMetadata(
                    CanaryDecision(True, "comparison failed (CalledProcessError)"),
                    windows_source_required=True,
                ),
                metadata_for_revisions(
                    "missing-base", "missing-head", root=Path(temp_dir)
                ),
            )

    def test_resolved_v8_version(self) -> None:
        cargo_lock = b"""\
[[package]]
name = "other"
version = "1.0.0"

[[package]]
name = "v8"
version = "149.2.0"
"""

        self.assertEqual(resolved_v8_version(cargo_lock), "149.2.0")

    def test_unrelated_cargo_manifest_change_does_not_require_source_build(
        self,
    ) -> None:
        self.assertFalse(
            windows_source_required(
                {"codex-rs/Cargo.toml"},
                "149.2.0",
                "149.2.0",
            )
        )

    def test_v8_version_change_requires_source_build(self) -> None:
        self.assertTrue(windows_source_required(set(), "149.2.0", "150.0.0"))

    def test_module_helper_change_requires_source_build(self) -> None:
        self.assertTrue(
            windows_source_required(
                {".github/scripts/rusty_v8_module_bazel.py"},
                "149.2.0",
                "149.2.0",
            )
        )

    def test_shared_ci_setup_changes_require_canary_and_source_build(self) -> None:
        for path in (
            ".github/actions/setup-ci/action.yml",
            ".github/scripts/setup-dev-drive.ps1",
        ):
            with self.subTest(path=path):
                changed_files = {path}
                self.assertTrue(canary_required(changed_files, "149.2.0", "149.2.0"))
                self.assertTrue(
                    windows_source_required(changed_files, "149.2.0", "149.2.0")
                )

    def test_manual_dispatch_requires_source_build(self) -> None:
        self.assertTrue(
            windows_source_required(
                set(),
                "149.2.0",
                "149.2.0",
                force=True,
            )
        )

    def test_changed_files_excludes_changes_made_only_on_base_branch(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            self.run_git(root, "init", "--initial-branch=main")
            self.run_git(root, "config", "user.name", "Test User")
            self.run_git(root, "config", "user.email", "test@example.com")

            self.write_and_commit(root, "initial", "initial.txt")
            common = self.run_git(root, "rev-parse", "HEAD")
            self.run_git(root, "switch", "-c", "feature")
            self.run_git(root, "switch", "main")
            self.write_and_commit(root, "base-only", "base-only.txt")
            base = self.run_git(root, "rev-parse", "HEAD")

            self.run_git(root, "switch", "feature")
            self.write_and_commit(root, "feature-only", "feature-only.txt")
            head = self.run_git(root, "rev-parse", "HEAD")

            self.assertEqual(
                changed_files(base, head, root=root),
                {"feature-only.txt"},
            )
            self.assertEqual(merge_base(base, head, root=root), common)

    def write_and_commit(self, root: Path, contents: str, path: str) -> None:
        (root / path).write_text(contents)
        self.run_git(root, "add", path)
        self.run_git(root, "commit", "-m", contents)

    def run_git(self, root: Path, *args: str) -> str:
        return subprocess.check_output(
            ["git", *args],
            cwd=root,
            stderr=subprocess.PIPE,
            text=True,
        ).strip()


if __name__ == "__main__":
    unittest.main()
