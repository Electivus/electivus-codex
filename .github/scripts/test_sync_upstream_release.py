import subprocess
import sys
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path

from sync_upstream_release import GitHubClient
from sync_upstream_release import PullRequestIntent
from sync_upstream_release import PullRequest
from sync_upstream_release import Release
from sync_upstream_release import SyncConfig
from sync_upstream_release import SyncError
from sync_upstream_release import SyncResult
from sync_upstream_release import synchronize


@dataclass
class FixtureReleases:
    releases: list[Release]

    @classmethod
    def published(cls, *releases: "CreatedRelease") -> "FixtureReleases":
        return cls(
            [
                Release(
                    release.tag,
                    "2026-07-20T00:00:00Z",
                    False,
                    release.url,
                )
                for release in releases
            ]
        )

    def list_releases(self) -> list[Release]:
        return self.releases


class RecordingPullRequests:
    def __init__(self, pull_requests: list[PullRequest] | None = None) -> None:
        self.created: list[PullRequestIntent] = []
        self.pull_requests = pull_requests or []

    def open_synchronization(self):
        return next(
            (
                pull_request
                for pull_request in self.pull_requests
                if pull_request.state == "open"
            ),
            None,
        )

    def for_branch(self, branch: str):
        return next(
            (
                pull_request
                for pull_request in self.pull_requests
                if pull_request.head == branch
            ),
            None,
        )

    def create(self, intent: PullRequestIntent):
        self.created.append(intent)
        return 1, "https://example.test/pull/1"


class SyncUpstreamReleaseTest(unittest.TestCase):
    def test_clean_sync_selects_greatest_semantic_version_and_preserves_topology(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            published_later = fixture.release(
                "rust-v0.147.0-alpha.6.6", "0.147.0", "published later"
            )
            selected = fixture.release(
                "rust-v0.148.0-alpha.6", "0.148.0", "selected"
            )
            fork_head = fixture.fork_head
            pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(),
                FixtureReleases(
                    [
                        Release(
                            tag="rust-v100.0.0",
                            published_at="2026-07-25T00:00:00Z",
                            draft=True,
                            url="https://example.test/draft",
                        ),
                        Release(
                            tag=published_later.tag,
                            published_at="2026-08-10T21:52:42Z",
                            draft=False,
                            url=published_later.url,
                        ),
                        Release(
                            tag=selected.tag,
                            published_at="2026-08-10T10:17:08Z",
                            draft=False,
                            url=selected.url,
                            prerelease=True,
                        ),
                        Release(
                            tag="sdk-v99.0.0",
                            published_at="2026-07-26T00:00:00Z",
                            draft=False,
                            url="https://example.test/sdk",
                        ),
                        Release(
                            tag="rust-v-unpublished",
                            published_at=None,
                            draft=False,
                            url="https://example.test/unpublished",
                        ),
                    ]
                ),
                pull_requests,
            )

            branch = f"automation/upstream-sync/{selected.commit}"
            self.assertEqual(
                result,
                SyncResult(
                    outcome="pr-created-clean",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=branch,
                    preparation_mode="clean",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                ),
            )
            branch_head = fixture.git("rev-parse", f"origin/{branch}")
            merge_commit = fixture.git("rev-parse", f"{branch_head}^")
            self.assertEqual(
                fixture.git("show", "-s", "--format=%P", merge_commit).split(),
                [fork_head, selected.commit],
            )
            self.assertIn(
                'version = "0.0.0"',
                fixture.git("show", f"origin/{branch}:codex-rs/Cargo.toml"),
            )
            self.assertEqual(
                fixture.git("show", f"origin/{branch}:package.json"),
                '{"version":"7.7.7"}',
            )
            self.assertEqual(len(pull_requests.created), 1)
            intent = pull_requests.created[0]
            self.assertEqual(intent.head, branch)
            self.assertEqual(intent.base, "main")
            self.assertFalse(intent.draft)
            self.assertIn(selected.tag, intent.title)
            self.assertIn(selected.commit, intent.body)
            self.assertIn(selected.url, intent.body)
            self.assertIn("automatic", intent.body)
            self.assertIn("0.0.0", intent.body)
            self.assertIn("maintainer approval", intent.body)

    def test_automatic_selection_ignores_invalid_semantic_version_tag(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v1.0.0", "1.0.0", "selected")
            invalid = fixture.release("rust-v-channel", "2.0.0", "invalid")
            pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(),
                FixtureReleases(
                    [
                        Release(
                            selected.tag,
                            "2026-07-20T00:00:00Z",
                            False,
                            selected.url,
                        ),
                        Release(
                            invalid.tag,
                            "2026-07-21T00:00:00Z",
                            False,
                            invalid.url,
                        ),
                    ]
                ),
                pull_requests,
            )

            self.assertEqual(
                result,
                SyncResult(
                    outcome="pr-created-clean",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=f"automation/upstream-sync/{selected.commit}",
                    preparation_mode="clean",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                ),
            )

    def test_automatic_selection_uses_semantic_version_prerelease_precedence(
        self,
    ) -> None:
        cases = (
            ("rust-v1.0.0-alpha.9", "rust-v1.0.0-alpha.10"),
            ("rust-v1.0.0-alpha.10", "rust-v1.0.0-alpha.beta"),
            ("rust-v1.0.0-alpha", "rust-v1.0.0"),
        )
        for lower_tag, selected_tag in cases:
            with (
                self.subTest(lower_tag=lower_tag, selected_tag=selected_tag),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                fixture = GitFixture(Path(temp_dir))
                lower = fixture.release(lower_tag, "1.0.0", "lower")
                selected = fixture.release(selected_tag, "1.0.0", "selected")
                pull_requests = RecordingPullRequests()

                result = synchronize(
                    fixture.config(),
                    FixtureReleases.published(lower, selected),
                    pull_requests,
                )

                self.assertEqual(
                    result,
                    SyncResult(
                        outcome="pr-created-clean",
                        tag=selected.tag,
                        release_commit=selected.commit,
                        branch=f"automation/upstream-sync/{selected.commit}",
                        preparation_mode="clean",
                        pr_number=1,
                        pr_url="https://example.test/pull/1",
                    ),
                )

    def test_manual_override_is_validated_before_fetch(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            valid = fixture.release("rust-v1.0.0", "1.0.0", "valid")
            releases = FixtureReleases(
                [
                    Release(valid.tag, "2026-07-20T00:00:00Z", False, valid.url),
                    Release("rust-v-draft", "2026-07-21T00:00:00Z", True, "draft"),
                    Release("rust-v-unpublished", None, False, "unpublished"),
                    Release("sdk-v2", "2026-07-22T00:00:00Z", False, "sdk"),
                ]
            )
            before = fixture.ref_snapshot()

            for tag in (
                "rust-v-draft",
                "rust-v-unpublished",
                "sdk-v2",
                "rust-v-unknown",
            ):
                with self.subTest(tag=tag):
                    with self.assertRaisesRegex(SyncError, "not a published"):
                        synchronize(
                            fixture.config(manual_tag=tag),
                            releases,
                            RecordingPullRequests(),
                        )
                    self.assertEqual(fixture.ref_snapshot(), before)

            pull_requests = RecordingPullRequests()
            result = synchronize(
                fixture.config(manual_tag=valid.tag), releases, pull_requests
            )
            self.assertEqual(
                result,
                SyncResult(
                    outcome="pr-created-clean",
                    tag=valid.tag,
                    release_commit=valid.commit,
                    branch=f"automation/upstream-sync/{valid.commit}",
                    preparation_mode="clean",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                ),
            )
            self.assertIn("Selection: manual", pull_requests.created[0].body)

    def test_manual_override_accepts_exact_published_non_semver_tag(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v-channel", "1.0.0", "selected")
            pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(manual_tag=selected.tag),
                FixtureReleases.published(selected),
                pull_requests,
            )

            self.assertEqual(
                result,
                SyncResult(
                    outcome="pr-created-clean",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=f"automation/upstream-sync/{selected.commit}",
                    preparation_mode="clean",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                ),
            )
            self.assertIn("Selection: manual", pull_requests.created[0].body)

    def test_already_integrated_release_is_an_idempotent_no_op(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v1.0.0", "1.0.0", "integrated")
            fixture.integrate(selected)
            pull_requests = RecordingPullRequests()

            for _ in range(2):
                self.assertEqual(
                    synchronize(
                        fixture.config(),
                        FixtureReleases.published(selected),
                        pull_requests,
                    ),
                    SyncResult(
                        outcome="already-integrated",
                        tag=selected.tag,
                        release_commit=selected.commit,
                    ),
                )
            self.assertEqual(pull_requests.created, [])

    def test_conflict_creates_normalized_release_branch_and_draft_intent(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fixture.fork_change("shared.txt", "fork version\n")
            selected = fixture.release(
                "rust-v3.0.0", "3.0.0", "upstream version", path="shared.txt"
            )
            pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(),
                FixtureReleases.published(selected),
                pull_requests,
            )

            branch = f"automation/upstream-sync/{selected.commit}"
            self.assertEqual(
                result,
                SyncResult(
                    outcome="draft-pr-created-conflicts",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=branch,
                    preparation_mode="conflicting",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                    conflict_count=1,
                    conflicts=("shared.txt",),
                ),
            )
            branch_head = fixture.git("rev-parse", f"origin/{branch}")
            self.assertEqual(
                fixture.git("merge-base", selected.commit, branch_head),
                selected.commit,
            )
            self.assertTrue(pull_requests.created[0].draft)
            self.assertIn("1 total", pull_requests.created[0].body)
            self.assertIn("`shared.txt`", pull_requests.created[0].body)

    def test_conflict_reporting_is_bounded_but_retains_total(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.many_conflicts(25)
            pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(),
                FixtureReleases.published(selected),
                pull_requests,
            )

            self.assertEqual(
                result,
                SyncResult(
                    outcome="draft-pr-created-conflicts",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=f"automation/upstream-sync/{selected.commit}",
                    preparation_mode="conflicting",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                    conflict_count=25,
                    conflicts=tuple(f"conflict-{index:02}.txt" for index in range(20)),
                ),
            )
            self.assertIn("25 total; showing up to 20", pull_requests.created[0].body)
            self.assertNotIn("`conflict-24.txt`", pull_requests.created[0].body)

    def test_open_pr_freezes_its_baseline_without_release_discovery(self) -> None:
        baseline = "a" * 40
        pull_request = PullRequest(
            number=17,
            url="https://example.test/pull/17",
            state="open",
            merged=False,
            head=f"automation/upstream-sync/{baseline}",
            head_sha="b" * 40,
            title="Synchronize openai/codex rust-v1.0.0",
            body=f"- Immutable commit: `{baseline}`",
            head_repository="Electivus/electivus-codex",
        )

        result = synchronize(
            SyncConfig(Path("/unused"), "/unused", "main"),
            FailingReleases(),
            RecordingPullRequests([pull_request]),
        )

        self.assertEqual(
            result,
            SyncResult(
                outcome="open-pr-frozen",
                tag="rust-v1.0.0",
                release_commit=baseline,
                branch=pull_request.head,
                pr_number=17,
                pr_url=pull_request.url,
            ),
        )

    def test_closed_pr_does_not_block_a_newer_release(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            abandoned = fixture.release("rust-v4.0.0", "4.0.0", "abandoned")
            abandoned_branch = f"automation/upstream-sync/{abandoned.commit}"
            fixture.git(
                "push", "origin", f"{fixture.fork_head}:refs/heads/{abandoned_branch}"
            )
            abandoned_head = fixture.remote_branch_head(abandoned_branch)
            pull_requests = RecordingPullRequests(
                [
                    closed_pull_request(
                        23, abandoned_branch, abandoned_head, abandoned
                    )
                ]
            )
            selected = fixture.release("rust-v4.1.0", "4.1.0", "selected")

            result = synchronize(
                fixture.config(),
                FixtureReleases.published(abandoned, selected),
                pull_requests,
            )

            branch = f"automation/upstream-sync/{selected.commit}"
            self.assertEqual(
                result,
                SyncResult(
                    outcome="pr-created-clean",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=branch,
                    preparation_mode="clean",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                ),
            )
            self.assertEqual(
                fixture.remote_branch_head(abandoned_branch), abandoned_head
            )
            self.assertEqual(len(pull_requests.created), 1)
            self.assertIn(selected.tag, pull_requests.created[0].title)

    def test_closed_pr_for_selected_release_is_abandoned(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v5.0.0", "5.0.0", "release")
            branch = f"automation/upstream-sync/{selected.commit}"
            pull_requests = RecordingPullRequests(
                [closed_pull_request(29, branch, "c" * 40, selected)]
            )

            result = synchronize(
                fixture.config(),
                FixtureReleases.published(selected),
                pull_requests,
            )

            self.assertEqual(
                result,
                SyncResult(
                    outcome="closed-pr-abandoned",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=branch,
                    pr_number=29,
                    pr_url="https://example.test/pull/29",
                ),
            )
            self.assertEqual(pull_requests.created, [])
            self.assertIsNone(fixture.remote_branch_head(branch))

    def test_merged_baseline_allows_a_later_release_on_a_distinct_branch(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            first = fixture.release("rust-v1.0.0", "1.0.0", "first")
            pull_requests = RecordingPullRequests()
            first_result = synchronize(
                fixture.config(),
                FixtureReleases.published(first),
                pull_requests,
            )
            fixture.integrate_branch(first_result.branch)
            pull_requests.pull_requests.append(
                PullRequest(
                    number=1,
                    url=first_result.pr_url,
                    state="closed",
                    merged=True,
                    head=first_result.branch,
                    head_sha=fixture.remote_branch_head(first_result.branch) or "",
                    title=f"Synchronize openai/codex {first.tag}",
                    body=f"- Immutable commit: `{first.commit}`",
                    head_repository="Electivus/electivus-codex",
                )
            )
            later = fixture.release("rust-v2.0.0", "2.0.0", "later")

            later_result = synchronize(
                fixture.config(),
                FixtureReleases(
                    [
                        Release(first.tag, "2026-07-20T00:00:00Z", False, first.url),
                        Release(later.tag, "2026-07-21T00:00:00Z", False, later.url),
                    ]
                ),
                pull_requests,
            )

            self.assertEqual(
                later_result,
                SyncResult(
                    outcome="draft-pr-created-conflicts",
                    tag=later.tag,
                    release_commit=later.commit,
                    branch=f"automation/upstream-sync/{later.commit}",
                    preparation_mode="conflicting",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                    conflict_count=1,
                    conflicts=("codex-rs/Cargo.toml",),
                ),
            )
            self.assertNotEqual(later_result.branch, first_result.branch)

    def test_push_failure_does_not_mutate_default_branch_or_request_a_pr(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v7.0.0", "7.0.0", "release")
            default_head = fixture.remote_branch_head("main")
            fixture.git(
                "remote",
                "set-url",
                "--push",
                "origin",
                str(fixture.root / "missing.git"),
            )
            pull_requests = RecordingPullRequests()

            with self.assertRaisesRegex(SyncError, "git push"):
                synchronize(
                    fixture.config(),
                    FixtureReleases.published(selected),
                    pull_requests,
                )

            self.assertEqual(fixture.remote_branch_head("main"), default_head)
            self.assertEqual(pull_requests.created, [])

    def test_pr_creation_failure_reuses_valid_prepared_branch_on_retry(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v7.1.0", "7.1.0", "release")
            releases = FixtureReleases.published(selected)
            branch = f"automation/upstream-sync/{selected.commit}"

            with self.assertRaisesRegex(SyncError, "PR API unavailable"):
                synchronize(
                    fixture.config(),
                    releases,
                    CreateFailurePullRequests(),
                )
            prepared_head = fixture.remote_branch_head(branch)
            retry_pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(),
                releases,
                retry_pull_requests,
            )

            self.assertEqual(
                result,
                SyncResult(
                    outcome="pr-created-clean",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=branch,
                    preparation_mode="clean",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                ),
            )
            self.assertEqual(fixture.remote_branch_head(branch), prepared_head)
            self.assertEqual(len(retry_pull_requests.created), 1)

    def test_pr_creation_retry_preserves_conflict_context_and_draft_state(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fixture.fork_change("shared.txt", "fork version\n")
            selected = fixture.release(
                "rust-v7.2.0", "7.2.0", "upstream version", path="shared.txt"
            )
            releases = FixtureReleases.published(selected)

            with self.assertRaisesRegex(SyncError, "PR API unavailable"):
                synchronize(
                    fixture.config(),
                    releases,
                    CreateFailurePullRequests(),
                )
            retry_pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(),
                releases,
                retry_pull_requests,
            )

            self.assertEqual(
                result,
                SyncResult(
                    outcome="draft-pr-created-conflicts",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=f"automation/upstream-sync/{selected.commit}",
                    preparation_mode="conflicting",
                    pr_number=1,
                    pr_url="https://example.test/pull/1",
                    conflict_count=1,
                    conflicts=("shared.txt",),
                ),
            )
            self.assertTrue(retry_pull_requests.created[0].draft)
            self.assertIn("`shared.txt`", retry_pull_requests.created[0].body)

    def test_discovery_and_fetch_failures_stop_before_branch_or_pr_mutation(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v8.0.0", "8.0.0", "release")
            pull_requests = RecordingPullRequests()
            before = fixture.ref_snapshot()

            with self.assertRaisesRegex(SyncError, "API unavailable"):
                synchronize(fixture.config(), ApiFailureReleases(), pull_requests)
            with self.assertRaisesRegex(SyncError, "git fetch"):
                synchronize(
                    SyncConfig(
                        fixture.fork,
                        str(fixture.root / "missing-upstream.git"),
                        "main",
                    ),
                    FixtureReleases.published(selected),
                    pull_requests,
                )

            self.assertEqual(fixture.ref_snapshot(), before)
            self.assertEqual(pull_requests.created, [])

    def test_non_conflict_merge_failure_stops_before_push_or_pr(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v9.0.0", "9.0.0", "release")
            fixture.git("config", "commit.gpgsign", "true")
            fixture.git("config", "gpg.program", sys.executable)
            pull_requests = RecordingPullRequests()

            with self.assertRaisesRegex(SyncError, "without content conflicts"):
                synchronize(
                    fixture.config(),
                    FixtureReleases.published(selected),
                    pull_requests,
                )

            self.assertIsNone(
                fixture.remote_branch_head(
                    f"automation/upstream-sync/{selected.commit}"
                )
            )
            self.assertEqual(pull_requests.created, [])

    def test_github_client_ignores_external_sync_branch_names(self) -> None:
        branch = "automation/upstream-sync/" + "a" * 40
        external = PullRequest(
            number=41,
            url="https://example.test/pull/41",
            state="open",
            merged=False,
            head=branch,
            head_sha="b" * 40,
            title="Synchronize openai/codex rust-v-external",
            body="",
            head_repository="someone/codex",
        )
        owned = PullRequest(
            number=43,
            url="https://example.test/pull/43",
            state="open",
            merged=False,
            head=branch,
            head_sha="c" * 40,
            title="Synchronize openai/codex rust-v-owned",
            body="",
            head_repository="Electivus/electivus-codex",
        )
        client = FixtureGitHubClient([external, owned])

        self.assertEqual(client.open_synchronization(), owned)
        self.assertEqual(client.for_branch(branch), owned)

    def test_workflow_is_a_safe_thin_adapter_for_the_sync_contract(self) -> None:
        workflow = (
            Path(__file__).parents[1] / "workflows" / "upstream-release-sync.yml"
        ).read_text()

        for contract in (
            'cron: "17 6 * * *"',
            "workflow_dispatch:",
            "release_tag:",
            "group: upstream-release-sync",
            "cancel-in-progress: false",
            "github.repository == 'Electivus/electivus-codex'",
            "RELEASE_TAG: ${{ inputs.release_tag || '' }}",
            '--release-tag "$RELEASE_TAG"',
        ):
            self.assertIn(contract, workflow)
        permissions = workflow.split("permissions:\n", 1)[1].split("\nconcurrency:", 1)[
            0
        ]
        self.assertEqual(
            permissions,
            "  contents: write\n  pull-requests: write\n",
        )
        run = workflow.split("        run: |\n", 1)[1]
        self.assertEqual(run.count("sync_upstream_release.py"), 1)
        self.assertNotIn("${{ inputs.release_tag", run)

    def test_ambiguous_workspace_version_fails_before_push_or_pr(self) -> None:
        manifests = {
            "missing": "[workspace]\nmembers = []\n",
            "duplicate": (
                '[workspace.package]\nversion = "1.0.0"\n'
                '[workspace.package]\nversion = "2.0.0"\n'
            ),
            "structural": '[workspace.package]\nversion = { value = "1.0.0" }\n',
        }
        for name, manifest in manifests.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temp_dir:
                fixture = GitFixture(Path(temp_dir))
                selected = fixture.release_with_manifest(
                    f"rust-v7.0.0-{name}", manifest
                )
                branch = f"automation/upstream-sync/{selected.commit}"
                pull_requests = RecordingPullRequests()

                with self.assertRaisesRegex(SyncError, "workspace"):
                    synchronize(
                        fixture.config(),
                        FixtureReleases.published(selected),
                        pull_requests,
                    )

                self.assertIsNone(fixture.remote_branch_head(branch))
                self.assertEqual(pull_requests.created, [])


class FailingReleases:
    def list_releases(self) -> list[Release]:
        raise AssertionError("release discovery must not run while a PR is open")


class ApiFailureReleases:
    def list_releases(self) -> list[Release]:
        raise SyncError("API unavailable")


class CreateFailurePullRequests(RecordingPullRequests):
    def create(self, intent: PullRequestIntent):
        raise SyncError("PR API unavailable")


class FixtureGitHubClient(GitHubClient):
    def __init__(self, pull_requests: list[PullRequest]) -> None:
        super().__init__("token", "Electivus/electivus-codex")
        self.pull_requests = pull_requests

    def _pull_requests(self, state: str) -> list[PullRequest]:
        return [
            pull_request
            for pull_request in self.pull_requests
            if state == "all" or pull_request.state == state
        ]


def closed_pull_request(
    number: int,
    branch: str,
    head_sha: str | None,
    release: "CreatedRelease",
) -> PullRequest:
    return PullRequest(
        number=number,
        url=f"https://example.test/pull/{number}",
        state="closed",
        merged=False,
        head=branch,
        head_sha=head_sha or "",
        title=f"Synchronize openai/codex {release.tag}",
        body=f"- Immutable commit: `{release.commit}`",
        head_repository="Electivus/electivus-codex",
    )


@dataclass(frozen=True)
class CreatedRelease:
    tag: str
    commit: str
    url: str


class GitFixture:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.upstream = root / "upstream"
        self.origin = root / "fork.git"
        self.fork = root / "fork"
        self.git_at(root, "init", "--initial-branch=main", str(self.upstream))
        self.configure(self.upstream)
        self.write_workspace(self.upstream, "0.0.0")
        (self.upstream / "package.json").write_text('{"version":"7.7.7"}\n')
        (self.upstream / "shared.txt").write_text("shared\n")
        for index in range(25):
            (self.upstream / f"conflict-{index:02}.txt").write_text("shared\n")
        self.commit(self.upstream, "shared base")
        self.git_at(root, "clone", "--bare", str(self.upstream), str(self.origin))
        self.git_at(root, "clone", str(self.origin), str(self.fork))
        self.configure(self.fork)
        (self.fork / "fork.txt").write_text("fork\n")
        self.commit(self.fork, "fork work")
        self.git_at(self.fork, "push", "origin", "main")
        self.fork_head = self.git("rev-parse", "HEAD")

    def config(self, *, manual_tag: str | None = None) -> SyncConfig:
        return SyncConfig(
            repo_root=self.fork,
            upstream_url=str(self.upstream),
            default_branch="main",
            manual_tag=manual_tag,
        )

    def release(
        self, tag: str, version: str, contents: str, *, path: str = "upstream.txt"
    ) -> CreatedRelease:
        self.write_workspace(self.upstream, version)
        (self.upstream / path).write_text(f"{contents}\n")
        self.commit(self.upstream, f"release {tag}")
        self.git_at(self.upstream, "tag", tag)
        return CreatedRelease(
            tag=tag,
            commit=self.git_at(self.upstream, "rev-parse", "HEAD"),
            url=f"https://example.test/releases/{tag}",
        )

    def release_with_manifest(self, tag: str, manifest: str) -> CreatedRelease:
        (self.upstream / "codex-rs" / "Cargo.toml").write_text(manifest)
        self.commit(self.upstream, f"release {tag}")
        self.git_at(self.upstream, "tag", tag)
        return CreatedRelease(
            tag=tag,
            commit=self.git_at(self.upstream, "rev-parse", "HEAD"),
            url=f"https://example.test/releases/{tag}",
        )

    def fork_change(self, path: str, contents: str) -> None:
        (self.fork / path).write_text(contents)
        self.commit(self.fork, f"fork changes {path}")
        self.git("push", "origin", "main")
        self.fork_head = self.git("rev-parse", "HEAD")

    def many_conflicts(self, count: int) -> CreatedRelease:
        for index in range(count):
            (self.fork / f"conflict-{index:02}.txt").write_text("fork\n")
        self.commit(self.fork, "fork conflict changes")
        self.git("push", "origin", "main")
        self.fork_head = self.git("rev-parse", "HEAD")
        for index in range(count):
            (self.upstream / f"conflict-{index:02}.txt").write_text("upstream\n")
        return self.release("rust-v6.0.0", "6.0.0", "release")

    def integrate(self, release: CreatedRelease) -> None:
        self.git("fetch", str(self.upstream), f"refs/tags/{release.tag}")
        self.git("merge", "--no-ff", "-m", f"integrate {release.tag}", release.commit)
        self.git("push", "origin", "main")

    def integrate_branch(self, branch: str) -> None:
        self.git("fetch", "origin", branch)
        self.git("merge", "--ff-only", "FETCH_HEAD")
        self.git("push", "origin", "main")

    def remote_branch_head(self, branch: str) -> str | None:
        output = self.git("ls-remote", "--heads", "origin", f"refs/heads/{branch}")
        return output.split()[0] if output else None

    def ref_snapshot(self) -> str:
        return self.git("show-ref")

    def git(self, *args: str) -> str:
        return self.git_at(self.fork, *args)

    @staticmethod
    def git_at(root: Path, *args: str) -> str:
        return subprocess.check_output(
            ["git", *args],
            cwd=root,
            stderr=subprocess.PIPE,
            text=True,
        ).strip()

    @classmethod
    def configure(cls, root: Path) -> None:
        cls.git_at(root, "config", "user.name", "Sync Test")
        cls.git_at(root, "config", "user.email", "sync@example.com")

    @classmethod
    def commit(cls, root: Path, message: str) -> None:
        cls.git_at(root, "add", ".")
        cls.git_at(root, "commit", "-m", message)

    @staticmethod
    def write_workspace(root: Path, version: str) -> None:
        manifest = root / "codex-rs" / "Cargo.toml"
        manifest.parent.mkdir(exist_ok=True)
        manifest.write_text(
            f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{version}"\n'
        )


if __name__ == "__main__":
    unittest.main()
