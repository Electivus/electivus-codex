import subprocess
import sys
import tempfile
import unittest
from dataclasses import dataclass
from dataclasses import replace
from pathlib import Path

from sync_upstream_release import GitHubClient
from sync_upstream_release import PullRequestIntent
from sync_upstream_release import PullRequest
from sync_upstream_release import Release
from sync_upstream_release import SyncConfig
from sync_upstream_release import SyncError
from sync_upstream_release import SyncResult
from sync_upstream_release import _bounded_diagnostic
from sync_upstream_release import _require_model_visible_budget
from sync_upstream_release import _write_outputs
from sync_upstream_release import _write_summary
from sync_upstream_release import synchronize
from upstream_sync_manifest import MAX_MODEL_VISIBLE_ITEM_BYTES
from upstream_sync_manifest import MAX_RENDERED_DIAGNOSTIC_BYTES
from upstream_sync_manifest import parse_manifest
from upstream_sync_manifest import render_pull_request_body
from upstream_sync_manifest import serialize_manifest

PR153_RELEASE_COMMIT = "b3a6d7f67cf056e18472c2b9ec26d3999ed40b7b"
PR153_RELEASE_COMMIT_OBJECT = """\
tree 58524fd767b96ea166b5700ff9e766c6a85926af
parent 8edb95f274ae70faac9dc35f079ed7b997dd862c
author jif <jif@openai.com> 1787333353 +0100
committer jif <jif@openai.com> 1787333353 +0100

Release 0.150.0-alpha.5
"""


def prepared_metadata(fork_base_sha: str, release_commit: str) -> dict[str, str]:
    return {
        "fork_base_sha": fork_base_sha,
        "manifest_path": f".github/upstream-sync-manifests/{release_commit}.json",
    }


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

    def release_for_tag(self, tag: str) -> Release:
        selected = next((release for release in self.releases if release.tag == tag), None)
        if selected is None:
            raise SyncError(f"{tag!r} is not a published, non-draft Codex CLI release")
        return selected


class ExactReleaseOnly(FixtureReleases):
    def list_releases(self) -> list[Release]:
        raise AssertionError("manual release selection must not list every release")


class NoReleaseLookup:
    def list_releases(self) -> list[Release]:
        raise AssertionError("invalid manual selection must not list releases")

    def release_for_tag(self, tag: str) -> Release:
        raise AssertionError(f"invalid manual selection must not look up {tag}")


def github_release_payload(tag: str) -> dict:
    return {
        "tag_name": tag,
        "published_at": "2026-08-21T18:12:34Z",
        "draft": False,
        "html_url": f"https://example.test/releases/{tag}",
        "prerelease": False,
    }


class PagedReleaseGitHubClient(GitHubClient):
    def __init__(self, pages: list[list[dict]]) -> None:
        super().__init__("token", "Electivus/electivus-codex")
        self.pages = pages

    def _request(
        self,
        path: str,
        *,
        method: str = "GET",
        body: dict | None = None,
    ):
        if not self.pages:
            raise AssertionError(f"unexpected GitHub request: {method} {path} {body}")
        return self.pages.pop(0)


class RecordingPullRequests:
    def __init__(self, pull_requests: list[PullRequest] | None = None) -> None:
        self.created: list[PullRequestIntent] = []
        self.pull_requests = pull_requests or []
        self.branch_batches: list[tuple[str, ...]] = []

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

    def for_branches(self, branches: tuple[str, ...]):
        self.branch_batches.append(branches)
        return {
            branch: pull_request
            for branch in branches
            if (pull_request := self.for_branch(branch)) is not None
        }

    def create(self, intent: PullRequestIntent):
        self.created.append(intent)
        return 1, "https://example.test/pull/1"


class SyncUpstreamReleaseTest(unittest.TestCase):
    def test_github_release_listing_accepts_more_than_1000_records(self) -> None:
        pages = [
            [
                github_release_payload(f"rust-v{page}.{item}.0")
                for item in range(100)
            ]
            for page in range(10)
        ]
        pages.append([github_release_payload("rust-v10.0.0")])

        releases = PagedReleaseGitHubClient(pages).list_releases()

        self.assertEqual(
            (len(releases), releases[0], releases[-1]),
            (
                1001,
                Release(
                    tag="rust-v0.0.0",
                    published_at="2026-08-21T18:12:34Z",
                    draft=False,
                    url="https://example.test/releases/rust-v0.0.0",
                ),
                Release(
                    tag="rust-v10.0.0",
                    published_at="2026-08-21T18:12:34Z",
                    draft=False,
                    url="https://example.test/releases/rust-v10.0.0",
                ),
            ),
        )

    def test_clean_sync_selects_greatest_semantic_version_and_preserves_topology(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            published_later = fixture.release(
                "rust-v0.147.0", "0.147.0", "published later"
            )
            selected = fixture.release("rust-v0.148.0", "0.148.0", "selected")
            newer_prerelease = fixture.release(
                "rust-v0.149.0-alpha.1", "0.149.0", "not automatic"
            )
            unpublished = fixture.release(
                "rust-v99.0.0", "99.0.0", "not published"
            )
            fork_head = fixture.fork_head
            default_head = fixture.remote_branch_head("main")
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
                            tag=newer_prerelease.tag,
                            published_at="2026-08-10T10:17:08Z",
                            draft=False,
                            url=newer_prerelease.url,
                            prerelease=True,
                        ),
                        Release(
                            tag=selected.tag,
                            published_at="2026-08-09T10:17:08Z",
                            draft=False,
                            url=selected.url,
                        ),
                        Release(
                            tag="sdk-v99.0.0",
                            published_at="2026-07-26T00:00:00Z",
                            draft=False,
                            url="https://example.test/sdk",
                        ),
                        Release(
                            tag=unpublished.tag,
                            published_at=None,
                            draft=False,
                            url=unpublished.url,
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
                    **prepared_metadata(fork_head, selected.commit),
                ),
            )
            branch_head = fixture.git("rev-parse", f"origin/{branch}")
            merge_commit = fixture.git("rev-parse", f"{branch_head}^^")
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
            manifest = parse_manifest(
                fixture.git("show", f"origin/{branch}:{result.manifest_path}") + "\n"
            )
            self.assertEqual(intent.head, branch)
            self.assertEqual(intent.base, "main")
            self.assertFalse(intent.draft)
            self.assertEqual(intent.body, render_pull_request_body(manifest))
            self.assertEqual(manifest.fork_base_sha, fork_head)
            self.assertEqual(fixture.remote_branch_head("main"), default_head)

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
                    **prepared_metadata(fixture.fork_head, selected.commit),
                ),
            )

    def test_automatic_selection_uses_stable_semantic_version_precedence(self) -> None:
        cases = (
            ("rust-v1.9.0", "rust-v1.10.0"),
            ("rust-v1.10.9", "rust-v1.11.0"),
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
                        **prepared_metadata(fixture.fork_head, selected.commit),
                    ),
                )

    def test_automatic_selection_rejects_only_published_prereleases(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            older = fixture.release("rust-v2.0.0-alpha.1", "2.0.0", "older")
            newer = fixture.release("rust-v2.0.0-rc.1", "2.0.0", "newer")
            pull_requests = RecordingPullRequests()
            before = fixture.ref_snapshot()

            with self.assertRaisesRegex(
                SyncError, "^no published stable Codex CLI release is available$"
            ):
                synchronize(
                    fixture.config(),
                    FixtureReleases.published(older, newer),
                    pull_requests,
                )

            self.assertEqual(fixture.ref_snapshot(), before)
            self.assertEqual(pull_requests.created, [])

    def test_automatic_selection_rejects_build_metadata_precedence_tie(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            first = fixture.release("rust-v2.0.0+one", "2.0.0", "first")
            second = fixture.release("rust-v2.0.0+two", "2.0.0", "second")
            before = fixture.ref_snapshot()

            with self.assertRaisesRegex(SyncError, "ambiguous"):
                synchronize(
                    fixture.config(),
                    FixtureReleases.published(first, second),
                    RecordingPullRequests(),
                )

            self.assertEqual(fixture.ref_snapshot(), before)

    def test_manual_override_is_validated_before_fetch(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            valid = fixture.release("rust-v1.0.0", "1.0.0", "valid")
            releases = FixtureReleases(
                [
                    Release(valid.tag, "2026-07-20T00:00:00Z", False, valid.url),
                    Release("rust-v2.0.0", "2026-07-21T00:00:00Z", True, "draft"),
                    Release("rust-v2.1.0", None, False, "unpublished"),
                ]
            )
            before = fixture.ref_snapshot()

            for tag in (
                "rust-v2.0.0",
                "rust-v2.1.0",
                "rust-v9.9.9",
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
                    **prepared_metadata(fixture.fork_head, valid.commit),
                ),
            )
            self.assertIn("Selection (`selectionMode`): `manual`", pull_requests.created[0].body)

    def test_manual_override_does_not_require_release_listing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v1.0.0-rc.1", "1.0.0", "selected")
            pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(manual_tag=selected.tag),
                ExactReleaseOnly.published(selected),
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
                    **prepared_metadata(fixture.fork_head, selected.commit),
                ),
            )
            self.assertIn("Selection (`selectionMode`): `manual`", pull_requests.created[0].body)

    def test_manual_override_rejects_invalid_tag_before_release_lookup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            before = fixture.ref_snapshot()

            for tag in ("rust-v-channel", "sdk-v1.2.3", "rust-v1.2.03"):
                with self.subTest(tag=tag), self.assertRaisesRegex(
                    SyncError, "exact rust-v<SemVer>"
                ):
                    synchronize(
                        fixture.config(manual_tag=tag),
                        NoReleaseLookup(),
                        RecordingPullRequests(),
                    )

            self.assertEqual(fixture.ref_snapshot(), before)

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
            default_head = fixture.remote_branch_head("main")
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
                    **prepared_metadata(fixture.fork_head, selected.commit),
                ),
            )
            branch_head = fixture.git("rev-parse", f"origin/{branch}")
            self.assertEqual(
                fixture.git("merge-base", selected.commit, branch_head),
                selected.commit,
            )
            ancestry = subprocess.run(
                ["git", "merge-base", "--is-ancestor", fixture.fork_head, branch_head],
                cwd=fixture.fork,
                check=False,
            )
            self.assertEqual(ancestry.returncode, 1)
            manifest = parse_manifest(
                fixture.git("show", f"origin/{branch}:{result.manifest_path}") + "\n"
            )
            self.assertTrue(pull_requests.created[0].draft)
            self.assertEqual(
                pull_requests.created[0].body, render_pull_request_body(manifest)
            )
            self.assertEqual(manifest.conflict_paths, ("shared.txt",))
            self.assertEqual(fixture.remote_branch_head("main"), default_head)

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
                    **prepared_metadata(fixture.fork_head, selected.commit),
                ),
            )
            self.assertIn("25 total; showing 20", pull_requests.created[0].body)
            self.assertNotIn('"conflict-24.txt"', pull_requests.created[0].body)

    def test_oversized_complete_conflict_evidence_fails_before_push_or_pr(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            conflict_directory = "large-conflicts"
            for index in range(100):
                path = f"{conflict_directory}/{index:03}-{'x' * 100}.txt"
                fork_path = fixture.fork / path
                fork_path.parent.mkdir(exist_ok=True)
                fork_path.write_text("fork\n")
            fixture.commit(fixture.fork, "add large fork conflict set")
            fixture.git("push", "origin", "main")
            fixture.fork_head = fixture.git("rev-parse", "HEAD")
            for index in range(100):
                path = f"{conflict_directory}/{index:03}-{'x' * 100}.txt"
                upstream_path = fixture.upstream / path
                upstream_path.parent.mkdir(exist_ok=True)
                upstream_path.write_text("upstream\n")
            selected = fixture.release("rust-v6.1.0", "6.1.0", "release")
            branch = f"automation/upstream-sync/{selected.commit}"
            pull_requests = RecordingPullRequests()

            with self.assertRaisesRegex(SyncError, "manifest exceeds its byte budget"):
                synchronize(
                    fixture.config(),
                    FixtureReleases.published(selected),
                    pull_requests,
                )

            self.assertIsNone(fixture.remote_branch_head(branch))
            self.assertEqual(pull_requests.created, [])

    def test_summary_json_encodes_conflict_paths_on_one_line(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            summary_path = Path(temp_dir) / "summary.md"
            conflict = "line one\n`injected`\nconflit-ç.txt"
            _write_summary(
                str(summary_path),
                SyncResult(
                    outcome="draft-pr-created-conflicts",
                    tag="rust-v1.0.0",
                    release_commit="a" * 40,
                    conflict_count=1,
                    conflicts=(conflict,),
                ),
            )

            summary = summary_path.read_text()
            self.assertIn(
                '  - "line one\\n`injected`\\nconflit-\\u00e7.txt"', summary
            )
            self.assertNotIn("\n`injected`\n", summary)

    def test_rendered_workflow_surfaces_share_a_model_visible_byte_budget(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            output_path = root / "output"
            summary_path = root / "summary.md"
            result = SyncResult(
                outcome="draft-pr-created-conflicts",
                tag="rust-v1.0.0",
                release_commit="a" * 40,
                branch=f"automation/upstream-sync/{'a' * 40}",
                preparation_mode="conflicting",
                pr_number=1,
                pr_url="https://example.test/pull/1",
                conflict_count=100,
                conflicts=tuple(f"{index:03}-{'x' * 100}.txt" for index in range(100)),
                fork_base_sha="b" * 40,
                manifest_path=f".github/upstream-sync-manifests/{'a' * 40}.json",
            )

            _write_outputs(str(output_path), result)
            _write_summary(str(summary_path), result)

            for path in (output_path, summary_path):
                self.assertLessEqual(
                    len(path.read_bytes()), MAX_MODEL_VISIBLE_ITEM_BYTES
                )

        at_budget = "é" * (MAX_MODEL_VISIBLE_ITEM_BYTES // 2)
        self.assertEqual(
            _require_model_visible_budget(at_budget, "test surface"), at_budget
        )
        with self.assertRaisesRegex(SyncError, "model-visible byte budget"):
            _require_model_visible_budget(f"{at_budget}x", "test surface")

        oversized_error = ("é" * MAX_RENDERED_DIAGNOSTIC_BYTES) + "\ntrailing"
        diagnostic = _bounded_diagnostic(oversized_error)
        self.assertLessEqual(
            len(diagnostic.encode("utf-8")), MAX_RENDERED_DIAGNOSTIC_BYTES
        )
        self.assertTrue(diagnostic.endswith(" ... [diagnostic truncated]"))
        self.assertNotIn("\n", diagnostic)

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            output_path = root / "failure-output"
            summary_path = root / "failure-summary.md"
            _write_outputs(str(output_path), None, oversized_error)
            _write_summary(str(summary_path), None, oversized_error)

            output = output_path.read_text()
            summary = summary_path.read_text()
            self.assertIn("outcome=failure", output)
            self.assertIn("error=", output)
            self.assertIn("- Outcome: failure", summary)
            self.assertIn("- Error: ", summary)
            for content in (output, summary):
                self.assertIn(" ... [diagnostic truncated]", content)
                self.assertLessEqual(
                    len(content.encode("utf-8")), MAX_MODEL_VISIBLE_ITEM_BYTES
                )

    def test_open_pr_freezes_its_baseline_without_release_discovery(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            release = fixture.release("rust-v1.0.0", "1.0.0", "legacy")
            branch = f"automation/upstream-sync/{release.commit}"
            fixture.git("fetch", str(fixture.upstream), release.commit)
            fixture.git("switch", "--detach", fixture.fork_head)
            fixture.git(
                "merge",
                "--no-ff",
                "-m",
                f"Merge openai/codex release {release.commit}",
                release.commit,
            )
            legacy_head = fixture.git("rev-parse", "HEAD")
            fixture.git("push", "origin", f"HEAD:refs/heads/{branch}")
            fixture.git("switch", "main")
            pull_request = PullRequest(
                17,
                "https://example.test/pull/17",
                "open",
                False,
                branch,
                legacy_head,
                f"Synchronize openai/codex {release.tag}",
                f"- Immutable commit: `{release.commit}`",
                "Electivus/electivus-codex",
            )

            result = synchronize(
                fixture.config(), FailingReleases(), RecordingPullRequests([pull_request])
            )

            self.assertEqual(
                result,
                SyncResult(
                    outcome="open-pr-frozen",
                    tag=release.tag,
                    release_commit=release.commit,
                    branch=branch,
                    pr_number=17,
                    pr_url=pull_request.url,
                ),
            )

            ambiguous_attempts = (
                replace(pull_request, body="missing immutable identity"),
                replace(pull_request, body=f"{pull_request.body}\n{pull_request.body}"),
                replace(
                    pull_request,
                    body=f"{pull_request.body}\nImmutable commit: `{'A' * 40}`",
                ),
                replace(pull_request, title="Synchronize openai/codex rust-v01.0.0"),
            )
            for ambiguous in ambiguous_attempts:
                with self.subTest(ambiguous=ambiguous):
                    with self.assertRaises(SyncError) as raised:
                        synchronize(
                            fixture.config(),
                            FailingReleases(),
                            RecordingPullRequests([ambiguous]),
                        )
                    self.assertEqual(raised.exception.outcome, "legacy-rejected")

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
                    **prepared_metadata(fixture.fork_head, selected.commit),
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

    def test_later_release_freezes_manifest_chain_and_rejects_predecessor_drift(self) -> None:
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
            later_fork_base = fixture.remote_branch_head("main") or ""

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
                    **prepared_metadata(later_fork_base, later.commit),
                ),
            )
            self.assertNotEqual(later_result.branch, first_result.branch)
            manifest_text = fixture.git(
                "show", f"origin/{later_result.branch}:{later_result.manifest_path}"
            )
            later_manifest = parse_manifest(f"{manifest_text}\n")
            self.assertEqual(later_manifest.previous_release_commit, first.commit)

            fixture.git("fetch", "origin", later_result.branch)
            fixture.git("switch", "--detach", "FETCH_HEAD")
            fixture.git(
                "merge", "--no-ff", "-s", "ours", "-m",
                "semantic reconciliation", later_fork_base,
            )
            reconciled_head = fixture.git("rev-parse", "HEAD")
            fixture.git("push", "origin", f"HEAD:refs/heads/{later_result.branch}")
            frozen = PullRequest(
                2, "https://example.test/pull/2", "open", False,
                later_result.branch, reconciled_head,
                "ignored title", "ignored body", "Electivus/electivus-codex",
            )
            frozen_result = synchronize(
                fixture.config(), FailingReleases(), RecordingPullRequests([frozen])
            )
            self.assertEqual(frozen_result.outcome, "open-pr-frozen")
            predecessor_path = fixture.fork / first_result.manifest_path
            predecessor = parse_manifest(predecessor_path.read_text())
            predecessor_path.write_text(
                serialize_manifest(replace(predecessor, selection_mode="manual"))
            )
            fixture.commit(fixture.fork, "alter predecessor manifest")
            fixture.git("push", "origin", f"HEAD:refs/heads/{later_result.branch}")
            drift_head = fixture.git("rev-parse", "HEAD")
            fixture.git("switch", "main")
            frozen_pull_requests = RecordingPullRequests([replace(frozen, head_sha=drift_head)])
            with self.assertRaisesRegex(SyncError, "history changed"):
                synchronize(
                    fixture.config(), FailingReleases(), frozen_pull_requests
                )
            self.assertEqual(
                fixture.remote_branch_head(later_result.branch), drift_head
            )
            self.assertEqual(frozen_pull_requests.created, [])

            fixture.git("switch", "--detach", drift_head)
            fixture.git("checkout", "HEAD^", "--", first_result.manifest_path)
            fixture.commit(fixture.fork, "revert predecessor manifest")
            reverted_head = fixture.git("rev-parse", "HEAD")
            fixture.git("push", "origin", f"HEAD:refs/heads/{later_result.branch}")
            fixture.git("switch", "main")
            reverted = replace(frozen, head_sha=reverted_head)
            with self.assertRaisesRegex(SyncError, "history changed"):
                synchronize(
                    fixture.config(), FailingReleases(), RecordingPullRequests([reverted])
                )

    def test_integrated_manifest_history_and_mode_are_immutable(self) -> None:
        for tamper, error in (
            ("rewrite", "history changed after introduction"),
            ("rewrite-and-revert", "history changed after introduction"),
            ("symlink", "regular blobs"),
        ):
            with self.subTest(tamper=tamper), tempfile.TemporaryDirectory() as temp_dir:
                fixture = GitFixture(Path(temp_dir))
                first = fixture.release("rust-v1.0.0", "1.0.0", "first")
                first_result = synchronize(
                    fixture.config(),
                    FixtureReleases.published(first),
                    RecordingPullRequests(),
                )
                fixture.integrate_branch(first_result.branch)
                path = fixture.fork / first_result.manifest_path
                original = path.read_text()

                if tamper == "symlink":
                    path.unlink()
                    blob = fixture.hash_object(
                        fixture.fork, "blob", "../immutable-manifest.json"
                    )
                    fixture.git(
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        f"120000,{blob},{first_result.manifest_path}",
                    )
                    fixture.git("commit", "-m", "replace manifest with symlink")
                else:
                    manifest = parse_manifest(original)
                    path.write_text(
                        serialize_manifest(replace(manifest, selection_mode="manual"))
                    )
                    fixture.commit(fixture.fork, "rewrite integrated manifest")
                    if tamper == "rewrite-and-revert":
                        path.write_text(original)
                        fixture.commit(fixture.fork, "revert integrated manifest")
                fixture.git("push", "origin", "main")
                later = fixture.release("rust-v2.0.0", "2.0.0", "later")
                first_pull_request = replace(
                    closed_pull_request(
                        1,
                        first_result.branch,
                        fixture.remote_branch_head(first_result.branch),
                        first,
                    ),
                    merged=True,
                )
                pull_requests = RecordingPullRequests([first_pull_request])

                with self.assertRaisesRegex(SyncError, error):
                    synchronize(
                        fixture.config(),
                        FixtureReleases.published(first, later),
                        pull_requests,
                    )

                self.assertIsNone(
                    fixture.remote_branch_head(
                        f"automation/upstream-sync/{later.commit}"
                    )
                )
                self.assertEqual(pull_requests.created, [])

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
            fork_base = fixture.fork_head

            with self.assertRaisesRegex(SyncError, "PR API unavailable"):
                synchronize(
                    fixture.config(),
                    releases,
                    CreateFailurePullRequests(),
                )
            prepared_head = fixture.remote_branch_head(branch)
            frozen = PullRequest(
                9,
                "https://example.test/pull/9",
                "open",
                False,
                branch,
                prepared_head or "",
                "malicious rust-v9.9.9",
                f"- Immutable commit: `{'f' * 40}`",
                "Electivus/electivus-codex",
            )
            frozen_pull_requests = RecordingPullRequests([frozen])
            frozen_result = synchronize(
                fixture.config(), FailingReleases(), frozen_pull_requests
            )
            self.assertEqual(
                frozen_result,
                SyncResult(
                    outcome="open-pr-frozen",
                    tag=selected.tag,
                    release_commit=selected.commit,
                    branch=branch,
                    preparation_mode="clean",
                    pr_number=frozen.number,
                    pr_url=frozen.url,
                    **prepared_metadata(fork_base, selected.commit),
                ),
            )
            self.assertEqual(frozen_pull_requests.created, [])
            newer = fixture.release("rust-v7.2.0", "7.2.0", "newer release")
            releases = FixtureReleases.published(selected, newer)
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
                    **prepared_metadata(fork_base, selected.commit),
                ),
            )
            self.assertEqual(fixture.remote_branch_head(branch), prepared_head)
            self.assertIsNone(
                fixture.remote_branch_head(
                    f"automation/upstream-sync/{newer.commit}"
                )
            )
            self.assertEqual(len(retry_pull_requests.created), 1)
            self.assertEqual(retry_pull_requests.branch_batches, [(branch,)])

            prepared_head = prepared_head or ""
            for tamper, error in (
                ("normalization", "exact workspace version edit"),
                ("merge-tree", "unexpected Baseline reconciliation"),
                ("preparation-mode", "conflict evidence differs"),
            ):
                with self.subTest(tamper=tamper):
                    merge = (
                        selected.commit
                        if tamper == "preparation-mode"
                        else fixture.git("rev-parse", f"{prepared_head}^^")
                    )
                    fixture.git("switch", "--detach", merge)
                    if tamper == "merge-tree":
                        (fixture.fork / "unexpected.txt").write_text("unexpected\n")
                        fixture.git("add", "unexpected.txt")
                        tree = fixture.git("write-tree")
                        parents = fixture.git("show", "-s", "--format=%P", merge).split()
                        merge = fixture.git(
                            "commit-tree",
                            tree,
                            "-p",
                            parents[0],
                            "-p",
                            parents[1],
                            "-m",
                            "altered Baseline reconciliation",
                        )
                        fixture.git("reset", "--hard", merge)
                    fixture.write_workspace(fixture.fork, "0.0.0")
                    if tamper == "normalization":
                        cargo_manifest = fixture.fork / "codex-rs/Cargo.toml"
                        cargo_manifest.write_text(
                            f"{cargo_manifest.read_text()}edition = \"2099\"\n"
                        )
                    fixture.commit(
                        fixture.fork, "Normalize Rust workspace version to 0.0.0"
                    )
                    if tamper == "preparation-mode":
                        fixture.git(
                            "checkout",
                            fork_base,
                            "--",
                            ".github/upstream-sync-manifests",
                        )
                    fixture.git("checkout", prepared_head, "--", result.manifest_path)
                    if tamper == "preparation-mode":
                        path = fixture.fork / result.manifest_path
                        manifest = parse_manifest(path.read_text())
                        path.write_text(
                            serialize_manifest(
                                replace(
                                    manifest,
                                    preparation_mode="conflicting",
                                    conflict_paths=("forged.txt",),
                                )
                            )
                        )
                    fixture.commit(
                        fixture.fork,
                        f"Record Synchronization manifest for {selected.tag}",
                    )
                    fixture.git("push", "--force", "origin", f"HEAD:refs/heads/{branch}")
                    fixture.git("switch", "main")
                    with self.assertRaisesRegex(SyncError, error):
                        synchronize(fixture.config(), releases, RecordingPullRequests())

    def test_pr_creation_retry_ignores_a_moved_release_tag(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v7.1.1", "7.1.1", "original release")
            branch = f"automation/upstream-sync/{selected.commit}"

            with self.assertRaisesRegex(SyncError, "PR API unavailable"):
                synchronize(
                    fixture.config(manual_tag=selected.tag),
                    FixtureReleases.published(selected),
                    CreateFailurePullRequests(),
                )
            prepared_head = fixture.remote_branch_head(branch)
            before_invalid = fixture.ref_snapshot()
            with self.assertRaisesRegex(SyncError, "exact rust-v<SemVer>"):
                synchronize(
                    fixture.config(manual_tag="rust-v-invalid"),
                    NoReleaseLookup(),
                    RecordingPullRequests(),
                )
            self.assertEqual(fixture.ref_snapshot(), before_invalid)
            with self.assertRaises(SyncError) as mismatch:
                synchronize(
                    fixture.config(manual_tag="rust-v7.1.2"),
                    FailingReleases(),
                    RecordingPullRequests(),
                )
            self.assertEqual(mismatch.exception.outcome, "pending-attempt")
            self.assertIn("does not match requested release", str(mismatch.exception))
            fixture.write_workspace(fixture.upstream, "7.1.2")
            (fixture.upstream / "retargeted.txt").write_text("retargeted\n")
            fixture.commit(fixture.upstream, "retarget release tag")
            moved_commit = fixture.git_at(fixture.upstream, "rev-parse", "HEAD")
            fixture.git_at(
                fixture.upstream,
                "tag",
                "--force",
                selected.tag,
                moved_commit,
            )
            pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(manual_tag=selected.tag),
                FailingReleases(),
                pull_requests,
            )

            self.assertEqual(
                (result.tag, result.release_commit, result.branch),
                (selected.tag, selected.commit, branch),
            )
            self.assertEqual(fixture.remote_branch_head(branch), prepared_head)
            self.assertIsNone(
                fixture.remote_branch_head(
                    f"automation/upstream-sync/{moved_commit}"
                )
            )
            self.assertEqual(len(pull_requests.created), 1)

    def test_orphaned_legacy_branch_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v7.1.1", "7.1.1", "legacy")
            branch = f"automation/upstream-sync/{selected.commit}"
            fixture.git("fetch", str(fixture.upstream), selected.commit)
            fixture.git("push", "origin", f"FETCH_HEAD:refs/heads/{branch}")
            pull_requests = RecordingPullRequests()
            with self.assertRaises(SyncError) as raised:
                synchronize(
                    fixture.config(),
                    FixtureReleases.published(selected),
                    pull_requests,
                )
            self.assertEqual(raised.exception.outcome, "legacy-rejected")
            self.assertEqual(pull_requests.created, [])

    def test_manifest_backed_branch_rejects_mismatched_ownership(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v7.1.2", "7.1.2", "release")
            branch = f"automation/upstream-sync/{selected.commit}"

            with self.assertRaisesRegex(SyncError, "PR API unavailable"):
                synchronize(
                    fixture.config(),
                    FixtureReleases.published(selected),
                    CreateFailurePullRequests(),
                )
            prepared_head = fixture.remote_branch_head(branch)
            fixture.git("fetch", "origin", branch)
            fixture.git("switch", "--detach", "FETCH_HEAD")
            mismatched_commit = "f" * 40
            mismatched_branch = f"automation/upstream-sync/{mismatched_commit}"
            mismatched_manifest = (
                f".github/upstream-sync-manifests/{mismatched_commit}.json"
            )
            fixture.git(
                "mv",
                f".github/upstream-sync-manifests/{selected.commit}.json",
                mismatched_manifest,
            )
            fixture.commit(fixture.fork, "move manifest away from its owning branch")
            mismatched_head = fixture.git("rev-parse", "HEAD")
            fixture.git("push", "origin", f":refs/heads/{branch}")
            fixture.git(
                "push", "origin", f"HEAD:refs/heads/{mismatched_branch}"
            )
            fixture.git("switch", "main")
            default_head = fixture.remote_branch_head("main")
            pull_requests = RecordingPullRequests()

            with self.assertRaisesRegex(
                SyncError, "Synchronization manifest filename does not match"
            ):
                synchronize(
                    fixture.config(), FailingReleases(), pull_requests
                )

            self.assertEqual(fixture.remote_branch_head("main"), default_head)
            self.assertEqual(
                fixture.remote_branch_head(mismatched_branch), mismatched_head
            )
            self.assertIsNotNone(prepared_head)
            self.assertIsNone(fixture.remote_branch_head(branch))
            self.assertEqual(pull_requests.created, [])

    def test_removed_active_manifest_is_not_misclassified_as_legacy(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            selected = fixture.release("rust-v7.1.2", "7.1.2", "release")
            result = synchronize(
                fixture.config(),
                FixtureReleases.published(selected),
                RecordingPullRequests(),
            )
            fixture.git("fetch", "origin", result.branch)
            fixture.git("switch", "--detach", "FETCH_HEAD")
            fixture.git("rm", result.manifest_path)
            fixture.commit(fixture.fork, "remove active manifest")
            fixture.git("push", "origin", f"HEAD:refs/heads/{result.branch}")
            fixture.git("switch", "main")

            with self.assertRaisesRegex(SyncError, "removed from branch history"):
                synchronize(
                    fixture.config(), FailingReleases(), RecordingPullRequests()
                )

    def test_pr_creation_retry_preserves_conflict_context_and_draft_state(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            fixture.fork_change("shared.txt", "fork version\n")
            fixture.write_workspace(fixture.upstream, "0.0.0")
            (fixture.upstream / "shared.txt").write_text("upstream version\n")
            fixture.commit(
                fixture.upstream, "Normalize Rust workspace version to 0.0.0"
            )
            tag = "rust-v7.2.0"
            fixture.git_at(fixture.upstream, "tag", tag)
            selected = CreatedRelease(
                tag,
                fixture.git_at(fixture.upstream, "rev-parse", "HEAD"),
                f"https://example.test/releases/{tag}",
            )
            releases = FixtureReleases.published(selected)
            fork_base = fixture.fork_head

            with self.assertRaisesRegex(SyncError, "PR API unavailable"):
                synchronize(
                    fixture.config(),
                    releases,
                    CreateFailurePullRequests(),
                )
            prepared_head = fixture.remote_branch_head(
                f"automation/upstream-sync/{selected.commit}"
            )
            fixture.fork_change("later.txt", "later fork work\n")
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
                    **prepared_metadata(fork_base, selected.commit),
                ),
            )
            self.assertTrue(retry_pull_requests.created[0].draft)
            self.assertIn('"shared.txt"', retry_pull_requests.created[0].body)
            self.assertEqual(
                fixture.remote_branch_head(result.branch), prepared_head
            )

    @unittest.skipIf(
        sys.platform == "win32",
        "Windows cannot materialize a filename containing a newline",
    )
    def test_conflicting_retry_preserves_hostile_path_and_default_branch(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = GitFixture(Path(temp_dir))
            conflict = "line one\n`injected`\nconflit-ç.txt"
            fixture.fork_change(conflict, "fork version\n")
            selected = fixture.release(
                "rust-v7.3.0", "7.3.0", "upstream version", path=conflict
            )
            releases = FixtureReleases.published(selected)
            fork_base = fixture.fork_head
            default_head = fixture.remote_branch_head("main")
            branch = f"automation/upstream-sync/{selected.commit}"
            failed_pull_requests = CreateFailurePullRequests()

            with self.assertRaisesRegex(SyncError, "PR API unavailable"):
                synchronize(fixture.config(), releases, failed_pull_requests)

            prepared_head = fixture.remote_branch_head(branch)
            manifest_path = (
                f".github/upstream-sync-manifests/{selected.commit}.json"
            )
            manifest = parse_manifest(
                fixture.git("show", f"origin/{branch}:{manifest_path}") + "\n"
            )
            intent = PullRequestIntent(
                title=f"Synchronize openai/codex {selected.tag}",
                head=branch,
                base="main",
                body=render_pull_request_body(manifest),
                draft=True,
            )
            self.assertEqual(manifest.conflict_paths, (conflict,))
            self.assertEqual(failed_pull_requests.created, [intent])
            self.assertEqual(fixture.remote_branch_head("main"), default_head)

            fixture.fork_change("later.txt", "later fork work\n")
            retry_default_head = fixture.remote_branch_head("main")
            retry_pull_requests = RecordingPullRequests()

            result = synchronize(
                fixture.config(), FailingReleases(), retry_pull_requests
            )

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
                    conflicts=(conflict,),
                    **prepared_metadata(fork_base, selected.commit),
                ),
            )
            self.assertEqual(retry_pull_requests.created, [intent])
            self.assertEqual(fixture.remote_branch_head(branch), prepared_head)
            self.assertEqual(
                fixture.remote_branch_head("main"), retry_default_head
            )

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

    def test_github_client_indexes_pull_requests_once_for_many_branches(self) -> None:
        branch = "automation/upstream-sync/" + "a" * 40
        owned = PullRequest(
            number=43,
            url="https://example.test/pull/43",
            state="closed",
            merged=False,
            head=branch,
            head_sha="c" * 40,
            title="Synchronize openai/codex rust-v-owned",
            body="",
            head_repository="Electivus/electivus-codex",
        )
        client = FixtureGitHubClient([owned])
        branches = (branch,) + tuple(
            f"automation/upstream-sync/{index:040x}" for index in range(1, 600)
        )

        self.assertEqual(client.for_branches(branches), {branch: owned})
        self.assertEqual(client.pull_request_queries, ["all"])

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
                    f"rust-v7.0.0+{name}", manifest
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
        self.created.append(intent)
        raise SyncError("PR API unavailable")


class FixtureGitHubClient(GitHubClient):
    def __init__(self, pull_requests: list[PullRequest]) -> None:
        super().__init__("token", "Electivus/electivus-codex")
        self.pull_requests = pull_requests
        self.pull_request_queries: list[str] = []

    def _pull_requests(self, state: str) -> list[PullRequest]:
        self.pull_request_queries.append(state)
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
        source = Path(__file__).parents[2]
        self.git_at(root, "init", "--initial-branch=main", str(self.upstream))
        self.configure(self.upstream)
        self.write_workspace(self.upstream, "0.0.0")
        (self.upstream / "package.json").write_text('{"version":"7.7.7"}\n')
        (self.upstream / "shared.txt").write_text("shared\n")
        for index in range(25):
            (self.upstream / f"conflict-{index:02}.txt").write_text("shared\n")
        self.commit(self.upstream, "shared base")
        fixture_seed = self.git_at(self.upstream, "rev-parse", "HEAD")
        release_seed = self.hash_object(
            self.upstream,
            "commit",
            PR153_RELEASE_COMMIT_OBJECT.removesuffix("\n"),
        )
        if release_seed != PR153_RELEASE_COMMIT:
            raise AssertionError("PR #153 fixture commit object has an unexpected ID")
        self.git_at(self.upstream, "branch", "fixture-seed", fixture_seed)
        self.git_at(self.upstream, "replace", release_seed, fixture_seed)
        tree = self.git_at(self.upstream, "rev-parse", "HEAD^{tree}")
        bridge = self.git_at(
            self.upstream,
            "commit-tree",
            tree,
            "-p",
            release_seed,
            "-m",
            "historical seed bridge",
        )
        self.git_at(self.upstream, "reset", "--hard", bridge)
        self.git_at(self.upstream, "branch", "-M", "main")
        self.git_at(root, "clone", "--bare", str(self.upstream), str(self.origin))
        self.git_at(self.origin, "replace", release_seed, fixture_seed)
        self.git_at(root, "clone", str(self.origin), str(self.fork))
        self.git_at(self.fork, "replace", release_seed, fixture_seed)
        self.configure(self.fork)
        (self.fork / "fork.txt").write_text("fork\n")
        seed = source / ".github/upstream-sync-manifests" / f"{release_seed}.json"
        destination = self.fork / ".github/upstream-sync-manifests" / seed.name
        destination.parent.mkdir(parents=True)
        destination.write_bytes(seed.read_bytes())
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

    @staticmethod
    def hash_object(root: Path, object_type: str, content: str) -> str:
        return subprocess.run(
            ["git", "hash-object", "-t", object_type, "-w", "--stdin"],
            cwd=root,
            input=content,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()

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
