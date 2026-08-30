import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from deep_linux_changes import DeepLinuxDecision
from deep_linux_changes import changed_files
from deep_linux_changes import classify_changed_files
from deep_linux_changes import decision_for_event


class DeepLinuxChangesTest(unittest.TestCase):
    def test_non_authoritative_shadow_validation_paths_are_ineligible(self) -> None:
        paths = (
            ".github/scripts/validation_contracts.py",
            ".github/scripts/test_validation.py",
            ".github/scripts/test_validation_contracts.py",
            ".github/scripts/check_validation_topology.py",
            ".github/scripts/test_check_validation_topology.py",
            ".github/scripts/legacy_validation_observation.py",
            ".github/workflows/validation-shadow.yml",
            ".github/workflows/validation-integrated.yaml",
        )

        for path in paths:
            with self.subTest(path=path):
                self.assertEqual(
                    DeepLinuxDecision(
                        eligible=False,
                        reason=(
                            "all 1 changed path is explicitly irrelevant documentation "
                            "or repository metadata"
                        ),
                    ),
                    classify_changed_files({path}),
                )

    def test_relevant_categories_are_eligible(self) -> None:
        paths = (
            "codex-rs/core/src/lib.rs",
            "codex-rs/Cargo.toml",
            "MODULE.bazel",
            "rust-toolchain.toml",
            "codex-rs/core/tests/suite/main.rs",
            ".github/actions/setup-ci/action.yml",
            ".github/scripts/deep_linux_changes.py",
            ".github/scripts/validation.py",
            ".github/scripts/validation_contracts.py.backup",
            ".github/workflows/blocking-ci.yml",
            ".github/workflows/repo-checks.yml",
            ".github/workflows/README.md",
            ".github/workflows/validation.yml",
            ".github/workflows/validation-shadow.yml.backup",
            "codex-rs/README.md",
            "unexpected/new-area/file.txt",
        )

        for path in paths:
            with self.subTest(path=path):
                self.assertEqual(
                    classify_changed_files({path}),
                    DeepLinuxDecision(
                        eligible=True,
                        reason="1 changed path is not explicitly irrelevant",
                    ),
                )

    def test_explicit_documentation_and_metadata_paths_are_ineligible(self) -> None:
        paths = (
            "README.md",
            "CODE_OF_CONDUCT.md",
            "CONTRIBUTING.md",
            "SECURITY.md",
            "docs/architecture/fork-ci.md",
            ".github/CODEOWNERS",
            ".github/ISSUE_TEMPLATE/bug_report.yml",
            ".github/pull_request_template.md",
            ".github/PULL_REQUEST_TEMPLATE/release.md",
        )

        for path in paths:
            with self.subTest(path=path):
                self.assertEqual(
                    classify_changed_files({path}),
                    DeepLinuxDecision(
                        eligible=False,
                        reason=(
                            "all 1 changed path is explicitly irrelevant documentation "
                            "or repository metadata"
                        ),
                    ),
                )

    def test_empty_comparison_is_eligible(self) -> None:
        self.assertEqual(
            classify_changed_files(set()),
            DeepLinuxDecision(
                eligible=True,
                reason=(
                    "comparison returned no changed paths; Deep Linux remains eligible"
                ),
            ),
        )

    def test_all_irrelevant_changes_are_ineligible(self) -> None:
        self.assertEqual(
            classify_changed_files({"README.md", "docs/architecture/fork-ci.md"}),
            DeepLinuxDecision(
                eligible=False,
                reason=(
                    "all 2 changed paths are explicitly irrelevant documentation or "
                    "repository metadata"
                ),
            ),
        )

    def test_mixed_changes_are_eligible(self) -> None:
        self.assertEqual(
            classify_changed_files(
                {
                    ".github/workflows/validation-shadow.yml",
                    "codex-rs/core/src/lib.rs",
                }
            ),
            DeepLinuxDecision(
                eligible=True,
                reason="1 of 2 changed paths is not explicitly irrelevant",
            ),
        )

    def test_changed_files_uses_pr_merge_base_and_disables_renames(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            self.run_git(root, "init", "--initial-branch=main")
            self.run_git(root, "config", "user.name", "Test User")
            self.run_git(root, "config", "user.email", "test@example.com")
            (root / "old.txt").write_text("initial")
            self.run_git(root, "add", "old.txt")
            self.run_git(root, "commit", "-m", "initial")

            self.run_git(root, "switch", "-c", "feature")
            self.run_git(root, "mv", "old.txt", "new.txt")
            self.run_git(root, "commit", "-m", "rename")
            head = self.run_git(root, "rev-parse", "HEAD")

            self.run_git(root, "switch", "main")
            (root / "base-only.txt").write_text("base")
            self.run_git(root, "add", "base-only.txt")
            self.run_git(root, "commit", "-m", "base only")
            base = self.run_git(root, "rev-parse", "HEAD")

            self.assertEqual(
                changed_files(base, head, root=root), {"new.txt", "old.txt"}
            )

    def test_non_pull_request_events_default_to_eligible(self) -> None:
        events = (
            ("workflow_dispatch", "workflow_dispatch"),
            ("push", "push"),
            ("schedule", "schedule"),
            ("unknown", "unknown"),
            ("", "unknown"),
        )
        for event_name, reason_event_name in events:
            with self.subTest(event_name=event_name):
                self.assertEqual(
                    decision_for_event(event_name, base=None, head=None),
                    DeepLinuxDecision(
                        eligible=True,
                        reason=(
                            f"{reason_event_name} event has no pull request comparison; "
                            "Deep Linux remains eligible"
                        ),
                    ),
                )

    def test_missing_pull_request_revision_defaults_to_eligible(self) -> None:
        for base, head in ((None, "head"), ("base", None), ("", "head")):
            with self.subTest(base=base, head=head):
                self.assertEqual(
                    decision_for_event("pull_request", base=base, head=head),
                    DeepLinuxDecision(
                        eligible=True,
                        reason=(
                            "pull request comparison is missing base or head; "
                            "Deep Linux remains eligible"
                        ),
                    ),
                )

    def test_comparison_error_defaults_to_eligible(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            self.assertEqual(
                decision_for_event(
                    "pull_request",
                    base="missing-base",
                    head="missing-head",
                    root=Path(temp_dir),
                ),
                DeepLinuxDecision(
                    eligible=True,
                    reason=(
                        "pull request comparison failed (CalledProcessError); "
                        "Deep Linux remains eligible"
                    ),
                ),
            )

    def test_cli_emits_bounded_outputs_for_manual_event(self) -> None:
        output = subprocess.check_output(
            [
                sys.executable,
                str(Path(__file__).with_name("deep_linux_changes.py")),
                "--event-name",
                "workflow_dispatch",
            ],
            text=True,
        )

        self.assertEqual(
            output,
            "eligible=true\n"
            "reason=workflow_dispatch event has no pull request comparison; "
            "Deep Linux remains eligible\n",
        )

    def run_git(self, root: Path, *args: str) -> str:
        return subprocess.check_output(
            ["git", *args],
            cwd=root,
            stderr=subprocess.PIPE,
            text=True,
        ).strip()


if __name__ == "__main__":
    unittest.main()
