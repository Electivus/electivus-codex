import subprocess
import tempfile
import unittest
from pathlib import Path

from check_upstream_sync_topology import (
    TopologyError,
    TopologyEvidence,
    validate_topology,
)
from sync_upstream_release import synchronize
from test_sync_upstream_release import (
    CreatedRelease,
    FixtureReleases,
    GitFixture,
    RecordingPullRequests,
)
from upstream_sync_attempt import PreparedAttempt, prepare_attempt
from upstream_sync_manifest import ReleaseIdentity, canonical_release_url


class UpstreamSyncTopologyTests(unittest.TestCase):
    def test_non_synchronization_branch_is_not_applicable(self) -> None:
        self.assertIsNone(validate_topology(Path.cwd(), "", "", "feature/example"))

    def test_clean_fork_first_topology_passes_without_catch_up(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fork_base = fixture.fork_head
            release, branch, head = self._prepare_clean(fixture)

            evidence = validate_topology(
                fixture.fork,
                head,
                fork_base,
                branch,
                seed_commit=fixture.seed_commit,
            )

            self.assertEqual(
                evidence,
                TopologyEvidence(
                    head_sha=head,
                    base_sha=fork_base,
                    branch=branch,
                    fork_base_sha=fork_base,
                    release_commit=release.commit,
                    manifest_introduction=fixture.git(
                        "log",
                        "--first-parent",
                        "--reverse",
                        "--format=%H",
                        head,
                        "--",
                        f".github/upstream-sync-manifests/{release.commit}.json",
                    ),
                    preparation_mode="clean",
                    baseline_reconciliation=fixture.git("rev-parse", f"{head}^^"),
                    catch_up_merge=None,
                ),
            )

    def test_clean_advanced_base_requires_and_accepts_catch_up(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            _, branch, prepared_head = self._prepare_clean(fixture)
            fixture.git("switch", "main")
            fixture.fork_change("later.txt", "later fork work\n")
            advanced_base = fixture.fork_head

            with self.assertRaisesRegex(TopologyError, "real PR head is stale"):
                validate_topology(
                    fixture.fork,
                    prepared_head,
                    advanced_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

            head = self._merge_current_main(fixture, branch)
            evidence = validate_topology(
                fixture.fork,
                head,
                advanced_base,
                branch,
                seed_commit=fixture.seed_commit,
            )

            self.assertIsNotNone(evidence)
            self.assertEqual(evidence.catch_up_merge, head)

    def test_clean_catch_up_cannot_discard_unrelated_base_change(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            _, branch, prepared_head = self._prepare_clean(fixture)
            fixture.git("switch", "main")
            fixture.fork_change("later.txt", "later fork work\n")
            advanced_base = fixture.fork_head
            invalid_catch_up = self._commit_with_parents(
                fixture,
                prepared_head,
                (prepared_head, advanced_base),
                "Catch up while discarding unrelated base work",
            )
            self._push_branch(fixture, branch, invalid_catch_up)

            with self.assertRaisesRegex(
                TopologyError, "merge tree does not match Git's conflict-free result"
            ):
                validate_topology(
                    fixture.fork,
                    invalid_catch_up,
                    advanced_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_conflicted_catch_up_may_resolve_conflicted_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            _, branch, _ = self._prepare_clean(fixture)
            fixture.git("switch", "main")
            fixture.fork_change("upstream.txt", "later fork version\n")
            fixture.fork_change("later.txt", "later fork work\n")
            advanced_base = fixture.fork_head
            head = self._merge_current_main_with_conflict(fixture, branch)

            evidence = validate_topology(
                fixture.fork,
                head,
                advanced_base,
                branch,
                seed_commit=fixture.seed_commit,
            )

            self.assertIsNotNone(evidence)
            self.assertEqual(evidence.catch_up_merge, head)

    def test_conflicted_catch_up_cannot_change_non_conflicted_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            _, branch, _ = self._prepare_clean(fixture)
            fixture.git("switch", "main")
            fixture.fork_change("upstream.txt", "later fork version\n")
            fixture.fork_change("later.txt", "later fork work\n")
            advanced_base = fixture.fork_head
            head = self._merge_current_main_with_conflict(
                fixture,
                branch,
                discard_path="later.txt",
            )

            with self.assertRaisesRegex(
                TopologyError,
                "conflicted resolution changed non-conflicted path later.txt",
            ):
                validate_topology(
                    fixture.fork,
                    head,
                    advanced_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_catch_up_cannot_discard_newer_base_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            first_release = fixture.release("rust-v1.0.0", "0.0.0", "first release")
            first_prepared = self._prepare_release(
                fixture,
                first_release,
                fixture.remote_branch_head("main") or "",
            )
            fixture.integrate_branch(first_prepared.branch)

            active_release = fixture.release("rust-v1.1.0", "0.0.0", "active release")
            active_prepared = self._prepare_release(
                fixture,
                active_release,
                fixture.remote_branch_head("main") or "",
            )
            active_head = fixture.remote_branch_head(active_prepared.branch)
            self.assertIsNotNone(active_head)

            later_release = fixture.release("rust-v1.2.0", "0.0.0", "later release")
            later_prepared = self._prepare_release(
                fixture,
                later_release,
                fixture.remote_branch_head("main") or "",
            )
            fixture.integrate_branch(later_prepared.branch)
            advanced_base = fixture.remote_branch_head("main")
            self.assertIsNotNone(advanced_base)

            invalid_catch_up = self._commit_with_parents(
                fixture,
                active_head or "",
                (active_head or "", advanced_base or ""),
                "Catch up while discarding newer base manifest",
            )
            self._push_branch(fixture, active_prepared.branch, invalid_catch_up)

            with self.assertRaisesRegex(
                TopologyError, "base manifest chain tip does not match"
            ):
                validate_topology(
                    fixture.fork,
                    invalid_catch_up,
                    advanced_base or "",
                    active_prepared.branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_conflicting_release_first_topology_passes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fixture.fork_change("shared.txt", "fork version\n")
            fork_base = fixture.fork_head
            release, branch, prepared_head = self._prepare_conflicting(fixture)
            head = self._merge_fork_baseline(fixture, branch, fork_base)

            evidence = validate_topology(
                fixture.fork,
                head,
                fork_base,
                branch,
                seed_commit=fixture.seed_commit,
            )

            self.assertEqual(evidence.preparation_mode, "conflicting")
            self.assertEqual(evidence.release_commit, release.commit)
            self.assertEqual(evidence.baseline_reconciliation, head)
            self.assertIsNone(evidence.catch_up_merge)
            self.assertEqual(
                fixture.git("show", "-s", "--format=%P", head).split(),
                [prepared_head, fork_base],
            )

    def test_conflicting_advanced_base_requires_catch_up_after_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fixture.fork_change("shared.txt", "fork version\n")
            fork_base = fixture.fork_head
            _, branch, _ = self._prepare_conflicting(fixture)
            baseline = self._merge_fork_baseline(fixture, branch, fork_base)
            fixture.git("switch", "main")
            fixture.fork_change("later.txt", "later fork work\n")
            advanced_base = fixture.fork_head
            head = self._merge_current_main(fixture, branch)

            evidence = validate_topology(
                fixture.fork,
                head,
                advanced_base,
                branch,
                seed_commit=fixture.seed_commit,
            )

            self.assertEqual(evidence.baseline_reconciliation, baseline)
            self.assertEqual(evidence.catch_up_merge, head)

    def test_conflicting_topology_without_baseline_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fixture.fork_change("shared.txt", "fork version\n")
            fork_base = fixture.fork_head
            _, branch, head = self._prepare_conflicting(fixture)

            with self.assertRaisesRegex(TopologyError, "real PR head is stale"):
                validate_topology(
                    fixture.fork,
                    head,
                    fork_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_duplicate_baseline_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fixture.fork_change("shared.txt", "fork version\n")
            fork_base = fixture.fork_head
            _, branch, _ = self._prepare_conflicting(fixture)
            baseline = self._merge_fork_baseline(fixture, branch, fork_base)
            duplicate = self._commit_with_parents(
                fixture,
                baseline,
                (baseline, fork_base),
                "duplicate Baseline reconciliation",
            )
            self._push_branch(fixture, branch, duplicate)

            with self.assertRaisesRegex(
                TopologyError, "exactly one Fork-second Baseline reconciliation"
            ):
                validate_topology(
                    fixture.fork,
                    duplicate,
                    fork_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_extra_clean_merge_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fork_base = fixture.fork_head
            _, branch, prepared_head = self._prepare_clean(fixture)
            extra = self._commit_with_parents(
                fixture,
                prepared_head,
                (prepared_head, fork_base),
                "duplicate clean reconciliation",
            )
            self._push_branch(fixture, branch, extra)

            with self.assertRaisesRegex(TopologyError, "extra reconciliation"):
                validate_topology(
                    fixture.fork,
                    extra,
                    fork_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_deterministic_commit_cannot_replace_reconciliation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fork_base = fixture.fork_head
            _, branch, _prepared_head = self._prepare_clean(fixture)
            fixture.git("fetch", "origin", branch)
            fixture.git("switch", "--detach", "FETCH_HEAD")
            fixture.git(
                "commit",
                "--allow-empty",
                "-m",
                "Normalize Rust workspace version to 0.0.0",
            )
            deterministic = fixture.git("rev-parse", "HEAD")
            self._push_branch(fixture, branch, deterministic)

            with self.assertRaisesRegex(TopologyError, "deterministic"):
                validate_topology(
                    fixture.fork,
                    deterministic,
                    fork_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_catch_up_before_baseline_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fixture.fork_change("shared.txt", "fork version\n")
            fork_base = fixture.fork_head
            _, branch, prepared_head = self._prepare_conflicting(fixture)
            fixture.git("switch", "main")
            fixture.fork_change("later.txt", "later fork work\n")
            advanced_base = fixture.fork_head
            catch_up = self._commit_with_parents(
                fixture,
                prepared_head,
                (prepared_head, advanced_base),
                "early Catch-up merge",
            )
            baseline = self._commit_with_parents(
                fixture,
                catch_up,
                (catch_up, fork_base),
                "late Baseline reconciliation",
            )
            self._push_branch(fixture, branch, baseline)

            with self.assertRaisesRegex(TopologyError, "must follow Baseline"):
                validate_topology(
                    fixture.fork,
                    baseline,
                    advanced_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_reversed_catch_up_parent_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            _, branch, _ = self._prepare_clean(fixture)
            fixture.git("switch", "main")
            fixture.fork_change("later.txt", "later fork work\n")
            advanced_base = fixture.fork_head
            fixture.git("fetch", "origin", branch)
            fixture.git("switch", "--detach", "FETCH_HEAD")
            reversed_merge = self._commit_with_parents(
                fixture,
                fixture.git("rev-parse", "HEAD"),
                (advanced_base, fixture.git("rev-parse", "HEAD")),
                "reversed Catch-up merge",
            )
            self._push_branch(fixture, branch, reversed_merge)

            with self.assertRaisesRegex(TopologyError, "immutable preparation graph"):
                validate_topology(
                    fixture.fork,
                    reversed_merge,
                    advanced_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_stale_head_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            _, branch, head = self._prepare_clean(fixture)
            fixture.git("switch", "main")
            fixture.fork_change("later.txt", "later fork work\n")

            with self.assertRaisesRegex(TopologyError, "real PR head is stale"):
                validate_topology(
                    fixture.fork,
                    head,
                    fixture.fork_head,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_manifest_tampering_after_introduction_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fork_base = fixture.fork_head
            release, branch, _ = self._prepare_clean(fixture)
            fixture.git("fetch", "origin", branch)
            fixture.git("switch", "--detach", "FETCH_HEAD")
            path = (
                fixture.fork / f".github/upstream-sync-manifests/{release.commit}.json"
            )
            path.write_text(
                path.read_text().replace(
                    '"selectionMode": "automatic"', '"selectionMode": "manual"'
                )
            )
            fixture.git("add", str(path.relative_to(fixture.fork)))
            fixture.git("commit", "-m", "tamper Synchronization manifest")
            tampered = fixture.git("rev-parse", "HEAD")
            self._push_branch(fixture, branch, tampered)

            with self.assertRaisesRegex(TopologyError, "manifest chain is invalid"):
                validate_topology(
                    fixture.fork,
                    tampered,
                    fork_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_branch_release_substitution_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fork_base = fixture.fork_head
            _, _branch, head = self._prepare_clean(fixture)
            substituted = f"automation/upstream-sync/{'f' * 40}"

            with self.assertRaisesRegex(TopologyError, "Synchronization release"):
                validate_topology(
                    fixture.fork,
                    head,
                    fork_base,
                    substituted,
                    seed_commit=fixture.seed_commit,
                )

    def test_github_synthetic_merge_is_not_accepted_as_real_head(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fork_base = fixture.fork_head
            _, branch, real_head = self._prepare_clean(fixture)
            tree = fixture.git("rev-parse", f"{real_head}^{{tree}}")
            synthetic = fixture.git(
                "commit-tree",
                tree,
                "-p",
                fork_base,
                "-p",
                real_head,
                "-m",
                "GitHub synthetic merge",
            )

            with self.assertRaisesRegex(TopologyError, "immutable preparation graph"):
                validate_topology(
                    fixture.fork,
                    synthetic,
                    fork_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_shallow_repository_fails_before_ancestry_inference(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fork_base = fixture.fork_head
            _, branch, head = self._prepare_clean(fixture)
            shallow = Path(temp_dir) / "shallow"
            fixture.git_at(
                fixture.root,
                "clone",
                "--no-local",
                "--depth",
                "1",
                "--branch",
                branch,
                str(fixture.origin),
                str(shallow),
            )

            with self.assertRaisesRegex(TopologyError, "complete Git history"):
                validate_topology(
                    shallow,
                    head,
                    fork_base,
                    branch,
                    seed_commit=fixture.seed_commit,
                )

    def test_replacement_refs_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fork_base = fixture.fork_head
            _, branch, head = self._prepare_clean(fixture)
            fixture.git("replace", head, fork_base)
            try:
                with self.assertRaisesRegex(TopologyError, "replacement refs"):
                    validate_topology(
                        fixture.fork,
                        head,
                        fork_base,
                        branch,
                        seed_commit=fixture.seed_commit,
                    )
            finally:
                fixture.git("replace", "-d", head)

    def _prepare_clean(self, fixture: GitFixture) -> tuple[CreatedRelease, str, str]:
        release = fixture.release("rust-v1.0.0", "1.0.0", "clean release")
        result = synchronize(
            fixture.config(),
            FixtureReleases.published(release),
            RecordingPullRequests(),
        )
        head = fixture.remote_branch_head(result.branch)
        self.assertIsNotNone(head)
        return release, result.branch, head or ""

    @staticmethod
    def _prepare_release(
        fixture: GitFixture, release: CreatedRelease, fork_base: str
    ) -> PreparedAttempt:
        fixture.git("fetch", str(fixture.upstream), f"refs/tags/{release.tag}")
        return prepare_attempt(
            fixture.fork,
            ReleaseIdentity(
                release.tag,
                release.commit,
                canonical_release_url(release.tag),
            ),
            fork_base,
            "automatic",
            seed_commit=fixture.seed_commit,
        )

    def _prepare_conflicting(
        self, fixture: GitFixture
    ) -> tuple[CreatedRelease, str, str]:
        release = fixture.release(
            "rust-v1.0.0", "0.0.0", "upstream version", path="shared.txt"
        )
        result = synchronize(
            fixture.config(),
            FixtureReleases.published(release),
            RecordingPullRequests(),
        )
        head = fixture.remote_branch_head(result.branch)
        self.assertIsNotNone(head)
        return release, result.branch, head or ""

    def _merge_fork_baseline(
        self, fixture: GitFixture, branch: str, fork_base: str
    ) -> str:
        fixture.git("fetch", "origin", branch)
        fixture.git("switch", "--detach", "FETCH_HEAD")
        process = subprocess.run(
            [
                "git",
                "merge",
                "--no-ff",
                "-m",
                "Reconcile Fork baseline",
                fork_base,
            ],
            cwd=fixture.fork,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(process.returncode, 0)
        (fixture.fork / "shared.txt").write_text("resolved version\n")
        fixture.git("add", "shared.txt")
        fixture.git("commit", "-m", "Reconcile Fork baseline")
        head = fixture.git("rev-parse", "HEAD")
        self._push_branch(fixture, branch, head)
        return head

    def _merge_current_main(self, fixture: GitFixture, branch: str) -> str:
        fixture.git("fetch", "origin", "main")
        fixture.git("fetch", "origin", branch)
        fixture.git("switch", "--detach", "FETCH_HEAD")
        fixture.git(
            "merge", "--no-ff", "-m", "Catch up Synchronization branch", "origin/main"
        )
        head = fixture.git("rev-parse", "HEAD")
        self._push_branch(fixture, branch, head)
        return head

    def _merge_current_main_with_conflict(
        self,
        fixture: GitFixture,
        branch: str,
        *,
        discard_path: str | None = None,
    ) -> str:
        fixture.git("fetch", "origin", "main")
        fixture.git("fetch", "origin", branch)
        fixture.git("switch", "--detach", "FETCH_HEAD")
        process = subprocess.run(
            [
                "git",
                "merge",
                "--no-ff",
                "-m",
                "Catch up Synchronization branch",
                "origin/main",
            ],
            cwd=fixture.fork,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(process.returncode, 0)
        (fixture.fork / "upstream.txt").write_text("resolved version\n")
        paths = ["upstream.txt"]
        if discard_path is not None:
            (fixture.fork / discard_path).unlink()
            paths.append(discard_path)
        fixture.git("add", "-A", "--", *paths)
        fixture.git("commit", "-m", "Catch up Synchronization branch")
        head = fixture.git("rev-parse", "HEAD")
        self._push_branch(fixture, branch, head)
        return head

    @staticmethod
    def _commit_with_parents(
        fixture: GitFixture,
        tree_source: str,
        parents: tuple[str, str],
        message: str,
    ) -> str:
        tree = fixture.git("rev-parse", f"{tree_source}^{{tree}}")
        return fixture.git(
            "commit-tree",
            tree,
            "-p",
            parents[0],
            "-p",
            parents[1],
            "-m",
            message,
        )

    @staticmethod
    def _push_branch(fixture: GitFixture, branch: str, head: str) -> None:
        fixture.git("push", "origin", f"{head}:refs/heads/{branch}")


if __name__ == "__main__":
    unittest.main()
