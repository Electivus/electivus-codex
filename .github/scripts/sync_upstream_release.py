#!/usr/bin/env python3
"""Prepare one reviewable synchronization from a published Codex CLI release."""

import argparse
import json
import os
import re
import subprocess
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from upstream_sync_attempt import LegacyAttemptError
from upstream_sync_attempt import PreparedAttempt
from upstream_sync_attempt import SYNC_BRANCH_PREFIX
from upstream_sync_attempt import SyncError
from upstream_sync_attempt import inspect_open_attempt
from upstream_sync_attempt import inspect_retry_attempt
from upstream_sync_attempt import prepare_attempt
from upstream_sync_attempt import synchronization_branches
from upstream_sync_manifest import ReleaseIdentity
from upstream_sync_manifest import MAX_MODEL_VISIBLE_ITEM_BYTES
from upstream_sync_manifest import bounded_conflict_paths
from upstream_sync_manifest import canonical_release_url
from upstream_sync_manifest import manifest_path
from upstream_sync_manifest import render_pull_request_body

GITHUB_PAGE_SIZE = 100
DEFAULT_GITHUB_RECORD_LIMIT = 1_000
UPSTREAM_RELEASE_RECORD_LIMIT = 10_000
RELEASE_TAG_PATTERN = re.compile(
    r"^rust-v(?P<major>0|[1-9]\d*)\."
    r"(?P<minor>0|[1-9]\d*)\."
    r"(?P<patch>0|[1-9]\d*)"
    r"(?:-(?P<prerelease>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)


@dataclass(frozen=True)
class Release:
    tag: str
    published_at: str | None
    draft: bool
    url: str
    prerelease: bool = False


@dataclass(frozen=True, order=True)
class _PrereleaseIdentifier:
    is_non_numeric: bool
    value: int | str


@dataclass(frozen=True, order=True)
class _SemanticVersion:
    major: int
    minor: int
    patch: int
    is_stable: bool
    prerelease_identifiers: tuple[_PrereleaseIdentifier, ...]


@dataclass(frozen=True)
class _VersionedRelease:
    release: Release
    version: _SemanticVersion


@dataclass(frozen=True)
class PullRequest:
    number: int
    url: str
    state: str
    merged: bool
    head: str
    head_sha: str
    title: str
    body: str
    head_repository: str


@dataclass(frozen=True)
class PullRequestIntent:
    title: str
    head: str
    base: str
    body: str
    draft: bool


@dataclass(frozen=True)
class SyncConfig:
    repo_root: Path
    upstream_url: str
    default_branch: str
    manual_tag: str | None = None


@dataclass(frozen=True)
class SyncResult:
    outcome: str
    tag: str
    release_commit: str
    branch: str = ""
    preparation_mode: str = ""
    pr_number: int | None = None
    pr_url: str = ""
    conflict_count: int = 0
    conflicts: tuple[str, ...] = ()
    fork_base_sha: str = ""
    manifest_path: str = ""


class ReleaseClient(Protocol):
    def list_releases(self) -> list[Release]: ...

    def release_for_tag(self, tag: str) -> Release: ...


class PullRequestService(Protocol):
    def open_synchronization(self) -> PullRequest | None: ...

    def for_branch(self, branch: str) -> PullRequest | None: ...

    def create(self, intent: PullRequestIntent) -> tuple[int, str]: ...


class PendingAttemptError(SyncError):
    outcome = "pending-attempt"


class GitHubClient:
    def __init__(self, token: str, repository: str) -> None:
        self.token = token
        self.repository = repository

    def list_releases(self) -> list[Release]:
        return [
            self._release(item)
            for item in self._get_pages(
                "/repos/openai/codex/releases",
                record_limit=UPSTREAM_RELEASE_RECORD_LIMIT,
            )
        ]

    def release_for_tag(self, tag: str) -> Release:
        encoded_tag = urllib.parse.quote(tag, safe="")
        result = self._request(f"/repos/openai/codex/releases/tags/{encoded_tag}")
        if not isinstance(result, dict):
            raise SyncError(f"GitHub returned a non-object response for release {tag}")
        return self._release(result)

    @staticmethod
    def _release(item: dict) -> Release:
        return Release(
            tag=item["tag_name"],
            published_at=item.get("published_at"),
            draft=item["draft"],
            url=item["html_url"],
            prerelease=item["prerelease"],
        )

    def open_synchronization(self) -> PullRequest | None:
        matches = [
            pull_request
            for pull_request in self._pull_requests("open")
            if pull_request.head_repository == self.repository
            and pull_request.head.startswith(SYNC_BRANCH_PREFIX)
        ]
        return self._only_pull_request(matches, "open Synchronization")

    def for_branch(self, branch: str) -> PullRequest | None:
        matches = [
            pull_request
            for pull_request in self._pull_requests("all")
            if pull_request.head_repository == self.repository
            and pull_request.head == branch
        ]
        return self._only_pull_request(matches, branch)

    def create(self, intent: PullRequestIntent) -> tuple[int, str]:
        result = self._request(
            f"/repos/{self.repository}/pulls",
            method="POST",
            body={
                "title": intent.title,
                "head": intent.head,
                "base": intent.base,
                "body": intent.body,
                "draft": intent.draft,
            },
        )
        return result["number"], result["html_url"]

    def _pull_requests(self, state: str) -> list[PullRequest]:
        return [
            PullRequest(
                number=item["number"],
                url=item["html_url"],
                state=item["state"],
                merged=item.get("merged_at") is not None,
                head=item["head"]["ref"],
                head_sha=item["head"]["sha"],
                title=item["title"],
                body=item.get("body") or "",
                head_repository=(item["head"].get("repo") or {}).get("full_name", ""),
            )
            for item in self._get_pages(
                f"/repos/{self.repository}/pulls",
                {"state": state},
            )
        ]

    @staticmethod
    def _only_pull_request(
        matches: list[PullRequest], description: str
    ) -> PullRequest | None:
        if len(matches) > 1:
            raise SyncError(f"found multiple PRs for {description}")
        return matches[0] if matches else None

    def _get_pages(
        self,
        path: str,
        query: dict[str, str] | None = None,
        *,
        record_limit: int = DEFAULT_GITHUB_RECORD_LIMIT,
    ) -> list[dict]:
        items = []
        page_limit = record_limit // GITHUB_PAGE_SIZE
        for page in range(1, page_limit + 1):
            page_query = {
                **(query or {}),
                "per_page": str(GITHUB_PAGE_SIZE),
                "page": str(page),
            }
            result = self._request(f"{path}?{urllib.parse.urlencode(page_query)}")
            if not isinstance(result, list):
                raise SyncError(f"GitHub returned a non-list response for {path}")
            items.extend(result)
            if len(result) < GITHUB_PAGE_SIZE:
                return items
        raise SyncError(f"GitHub pagination exceeded {record_limit} records for {path}")

    def _request(
        self,
        path: str,
        *,
        method: str = "GET",
        body: dict | None = None,
    ):
        request = urllib.request.Request(
            f"https://api.github.com{path}",
            data=json.dumps(body).encode() if body is not None else None,
            method=method,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self.token}",
                "Content-Type": "application/json",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except (OSError, urllib.error.HTTPError, json.JSONDecodeError) as error:
            raise SyncError(f"GitHub API {method} {path} failed: {error}") from error


def synchronize(
    config: SyncConfig,
    releases: ReleaseClient,
    pull_requests: PullRequestService,
) -> SyncResult:
    frozen = pull_requests.open_synchronization()
    if frozen is not None:
        return _frozen_result(config.repo_root, frozen)

    if config.manual_tag and _semantic_version(config.manual_tag) is None:
        raise SyncError(
            f"{config.manual_tag!r} is not an exact rust-v<SemVer> release tag"
        )
    retry = _orphaned_attempt(config.repo_root, pull_requests, config.manual_tag)
    if retry is not None:
        return _create_pull_request(config, pull_requests, retry)

    candidates = (
        [releases.release_for_tag(config.manual_tag)]
        if config.manual_tag
        else releases.list_releases()
    )
    release, selection_mode = _select_release(candidates, config.manual_tag)
    release_commit = _fetch_release(config, release.tag)
    branch = f"{SYNC_BRANCH_PREFIX}{release_commit}"
    fork_head = _default_head(config)
    release_identity = ReleaseIdentity(
        tag=release.tag,
        commit=release_commit,
        url=canonical_release_url(release.tag),
    )

    if _is_ancestor(config.repo_root, release_commit, fork_head):
        return SyncResult(
            outcome="already-integrated",
            tag=release.tag,
            release_commit=release_commit,
        )

    existing = pull_requests.for_branch(branch)
    if existing is not None and existing.merged:
        return SyncResult(
            outcome="already-integrated",
            tag=release.tag,
            release_commit=release_commit,
            branch=branch,
            pr_number=existing.number,
            pr_url=existing.url,
        )
    if existing is not None and existing.state == "open":
        return _frozen_result(config.repo_root, existing)
    if existing is not None:
        return SyncResult(
            outcome="closed-pr-abandoned",
            tag=release.tag,
            release_commit=release_commit,
            branch=branch,
            pr_number=existing.number,
            pr_url=existing.url,
        )

    prepared = prepare_attempt(
        config.repo_root,
        release_identity,
        fork_head,
        selection_mode,
    )
    return _create_pull_request(config, pull_requests, prepared)


def _orphaned_attempt(
    repo_root: Path,
    pull_requests: PullRequestService,
    requested_tag: str | None,
) -> PreparedAttempt | None:
    attempts = [
        inspect_retry_attempt(repo_root, branch, head)
        for branch, head in synchronization_branches(repo_root)
        if pull_requests.for_branch(branch) is None
    ]
    if len(attempts) > 1:
        raise SyncError("found multiple orphaned Synchronization attempts")
    if not attempts:
        return None
    attempt = attempts[0]
    if requested_tag is not None and attempt.manifest.release.tag != requested_tag:
        raise PendingAttemptError(
            f"pending Synchronization attempt for {attempt.manifest.release.tag} "
            f"does not match requested release {requested_tag}"
        )
    return attempt


def _create_pull_request(
    config: SyncConfig,
    pull_requests: PullRequestService,
    prepared: PreparedAttempt,
) -> SyncResult:
    manifest = prepared.manifest
    intent = PullRequestIntent(
        title=f"Synchronize openai/codex {manifest.release.tag}",
        head=prepared.branch,
        base=config.default_branch,
        body=render_pull_request_body(manifest),
        draft=manifest.preparation_mode == "conflicting",
    )
    number, url = pull_requests.create(intent)
    outcome = (
        "pr-created-clean"
        if manifest.preparation_mode == "clean"
        else "draft-pr-created-conflicts"
    )
    return SyncResult(
        outcome=outcome,
        tag=manifest.release.tag,
        release_commit=manifest.release.commit,
        branch=prepared.branch,
        preparation_mode=manifest.preparation_mode,
        pr_number=number,
        pr_url=url,
        conflict_count=len(manifest.conflict_paths),
        conflicts=bounded_conflict_paths(manifest.conflict_paths),
        fork_base_sha=manifest.fork_base_sha,
        manifest_path=manifest_path(manifest.release.commit),
    )


def _frozen_result(repo_root: Path, frozen: PullRequest) -> SyncResult:
    prepared = inspect_open_attempt(repo_root, frozen.head, frozen.head_sha)
    if prepared is None:
        tag = _tag_from_title(frozen.title)
        branch_commit = frozen.head.removeprefix(SYNC_BRANCH_PREFIX)
        body_commit = _commit_from_body(frozen.body)
        if (
            not tag
            or not re.fullmatch(r"[0-9a-f]{40}", branch_commit)
            or body_commit != branch_commit
        ):
            raise LegacyAttemptError(
                "open legacy Synchronization PR identity is ambiguous"
            )
        return SyncResult(
            outcome="open-pr-frozen",
            tag=tag,
            release_commit=branch_commit,
            branch=frozen.head,
            pr_number=frozen.number,
            pr_url=frozen.url,
        )
    manifest = prepared.manifest
    return SyncResult(
        outcome="open-pr-frozen",
        tag=manifest.release.tag,
        release_commit=manifest.release.commit,
        branch=prepared.branch,
        preparation_mode=manifest.preparation_mode,
        pr_number=frozen.number,
        pr_url=frozen.url,
        conflict_count=len(manifest.conflict_paths),
        conflicts=bounded_conflict_paths(manifest.conflict_paths),
        fork_base_sha=manifest.fork_base_sha,
        manifest_path=manifest_path(manifest.release.commit),
    )


def _select_release(
    releases: list[Release], manual_tag: str | None
) -> tuple[Release, str]:
    eligible = [
        release
        for release in releases
        if not release.draft
        and release.published_at is not None
        and release.tag.startswith("rust-v")
    ]
    if manual_tag:
        selected = next(
            (
                release
                for release in eligible
                if release.tag == manual_tag
                and _semantic_version(release.tag) is not None
            ),
            None,
        )
        if selected is None:
            raise SyncError(
                f"{manual_tag!r} is not a published, non-draft Codex CLI release"
            )
        return selected, "manual"
    automatic = [
        _VersionedRelease(release, version)
        for release in eligible
        if (version := _semantic_version(release.tag)) is not None and version.is_stable
    ]
    if not automatic:
        raise SyncError("no published stable Codex CLI release is available")
    greatest = max(candidate.version for candidate in automatic)
    selected = [
        candidate.release for candidate in automatic if candidate.version == greatest
    ]
    if len(selected) != 1:
        raise SyncError("greatest stable Codex CLI Semantic Version is ambiguous")
    return selected[0], "automatic"


def _semantic_version(
    tag: str,
) -> _SemanticVersion | None:
    match = RELEASE_TAG_PATTERN.fullmatch(tag)
    if match is None:
        return None
    prerelease = match.group("prerelease")
    if prerelease is not None and any(
        identifier.isdigit() and len(identifier) > 1 and identifier.startswith("0")
        for identifier in prerelease.split(".")
    ):
        return None
    identifiers = tuple(
        _PrereleaseIdentifier(
            is_non_numeric=not identifier.isdigit(),
            value=identifier if not identifier.isdigit() else int(identifier),
        )
        for identifier in prerelease.split(".")
    ) if prerelease is not None else ()
    return _SemanticVersion(
        major=int(match.group("major")),
        minor=int(match.group("minor")),
        patch=int(match.group("patch")),
        is_stable=prerelease is None,
        prerelease_identifiers=identifiers,
    )


def _fetch_release(config: SyncConfig, tag: str) -> str:
    _git(
        config.repo_root,
        "fetch",
        "--no-tags",
        config.upstream_url,
        f"refs/tags/{tag}",
    )
    return _git(config.repo_root, "rev-parse", "FETCH_HEAD^{commit}")


def _default_head(config: SyncConfig) -> str:
    for ref in (
        f"refs/remotes/origin/{config.default_branch}",
        f"refs/heads/{config.default_branch}",
    ):
        process = _run_git(config.repo_root, "rev-parse", "--verify", ref)
        if process.returncode == 0:
            return process.stdout.strip()
    raise SyncError(f"cannot resolve default branch {config.default_branch}")


def _is_ancestor(repo: Path, ancestor: str, descendant: str) -> bool:
    process = _run_git(repo, "merge-base", "--is-ancestor", ancestor, descendant)
    if process.returncode not in (0, 1):
        raise SyncError(process.stderr.strip())
    return process.returncode == 0


def _tag_from_title(title: str) -> str:
    match = re.fullmatch(r"Synchronize openai/codex (rust-v\S+)", title)
    if match is None or _semantic_version(match.group(1)) is None:
        return ""
    return match.group(1)


def _commit_from_body(body: str) -> str:
    if body.count("Immutable commit:") != 1:
        return ""
    match = re.search(r"Immutable commit: `([^`\r\n]*)`", body)
    if match is None or re.fullmatch(r"[0-9a-f]{40}", match.group(1)) is None:
        return ""
    return match.group(1)


def _run_git(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.setdefault("GIT_AUTHOR_NAME", "github-actions[bot]")
    env.setdefault(
        "GIT_AUTHOR_EMAIL", "41898282+github-actions[bot]@users.noreply.github.com"
    )
    env.setdefault("GIT_COMMITTER_NAME", env["GIT_AUTHOR_NAME"])
    env.setdefault("GIT_COMMITTER_EMAIL", env["GIT_AUTHOR_EMAIL"])
    return subprocess.run(
        ["git", *args],
        cwd=repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )


def _git(repo: Path, *args: str) -> str:
    process = _run_git(repo, *args)
    if process.returncode != 0:
        raise SyncError(
            f"git {' '.join(args)} failed ({process.returncode}): "
            f"{process.stderr.strip()}"
        )
    return process.stdout.strip()


def _write_outputs(
    path: str | None,
    result: SyncResult | None,
    error: str = "",
    outcome: str = "failure",
) -> None:
    if not path:
        return
    values = (
        {
            "outcome": result.outcome,
            "release_tag": result.tag,
            "release_commit": result.release_commit,
            "branch": result.branch,
            "preparation_mode": result.preparation_mode,
            "pr_number": result.pr_number or "",
            "pr_url": result.pr_url,
            "conflict_count": result.conflict_count,
            "conflicts": json.dumps(bounded_conflict_paths(result.conflicts)),
            "fork_base_sha": result.fork_base_sha,
            "manifest_path": result.manifest_path,
        }
        if result is not None
        else {"outcome": outcome, "error": error.replace("\n", " ")}
    )
    content = "".join(f"{key}={value}\n" for key, value in values.items())
    with Path(path).open("a") as output:
        output.write(_require_model_visible_budget(content, "GitHub output"))


def _write_summary(
    path: str | None,
    result: SyncResult | None,
    error: str = "",
    outcome: str = "failure",
) -> None:
    if not path:
        return
    lines = ["## Upstream synchronization", ""]
    if result is None:
        lines.extend([f"- Outcome: {outcome}", f"- Error: {error}"])
    else:
        lines.extend(
            [
                f"- Outcome: {result.outcome}",
                f"- Release: `{result.tag}`",
                f"- Commit: `{result.release_commit}`",
            ]
        )
        if result.pr_url:
            lines.append(f"- Pull request: {result.pr_url}")
        if result.fork_base_sha:
            lines.append(f"- Fork baseline: `{result.fork_base_sha}`")
        if result.manifest_path:
            lines.append(f"- Manifest: `{result.manifest_path}`")
        if result.conflict_count:
            lines.append(f"- Conflicts: {result.conflict_count} total")
            lines.extend(
                f"  - {json.dumps(conflict, ensure_ascii=True)}"
                for conflict in bounded_conflict_paths(result.conflicts)
            )
    content = "\n".join(lines) + "\n"
    with Path(path).open("a") as summary:
        summary.write(_require_model_visible_budget(content, "GitHub summary"))


def _require_model_visible_budget(content: str, surface: str) -> str:
    if len(content.encode("utf-8")) > MAX_MODEL_VISIBLE_ITEM_BYTES:
        raise SyncError(f"{surface} exceeds its model-visible byte budget")
    return content


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    parser.add_argument("--upstream-url", default="https://github.com/openai/codex.git")
    parser.add_argument("--default-branch", default="main")
    parser.add_argument("--release-tag")
    parser.add_argument("--output", default=os.environ.get("GITHUB_OUTPUT"))
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY"))
    args = parser.parse_args()

    try:
        token = os.environ.get("GITHUB_TOKEN")
        repository = os.environ.get("GITHUB_REPOSITORY")
        if not token or not repository:
            raise SyncError("GITHUB_TOKEN and GITHUB_REPOSITORY are required")
        client = GitHubClient(token, repository)
        result = synchronize(
            SyncConfig(
                repo_root=args.repo_root.resolve(),
                upstream_url=args.upstream_url,
                default_branch=args.default_branch,
                manual_tag=args.release_tag or None,
            ),
            client,
            client,
        )
    except Exception as error:
        message = str(error)
        outcome = getattr(error, "outcome", "failure")
        _write_outputs(args.output, None, message, outcome)
        _write_summary(args.summary, None, message, outcome)
        print(message, file=os.sys.stderr)
        return 1
    _write_outputs(args.output, result)
    _write_summary(args.summary, result)
    payload = {
        **result.__dict__,
        "conflicts": bounded_conflict_paths(result.conflicts),
    }
    rendered_payload = json.dumps(payload, sort_keys=True)
    print(_require_model_visible_budget(rendered_payload, "standard output"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
