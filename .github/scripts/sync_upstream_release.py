#!/usr/bin/env python3
"""Prepare one reviewable synchronization from a published Codex CLI release."""

import argparse
import json
import os
import re
import subprocess
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol


SYNC_BRANCH_PREFIX = "automation/upstream-sync/"
MAX_CONFLICTS_SHOWN = 20


@dataclass(frozen=True)
class Release:
    tag: str
    published_at: str | None
    draft: bool
    url: str
    prerelease: bool = False


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


class ReleaseClient(Protocol):
    def list_releases(self) -> list[Release]: ...


class PullRequestService(Protocol):
    def open_synchronization(self) -> PullRequest | None: ...

    def closed_synchronization(self) -> PullRequest | None: ...

    def for_branch(self, branch: str) -> PullRequest | None: ...

    def create(self, intent: PullRequestIntent) -> tuple[int, str]: ...

    def reopen(self, number: int) -> tuple[int, str]: ...


class SyncError(RuntimeError):
    pass


class GitHubClient:
    def __init__(self, token: str, repository: str) -> None:
        self.token = token
        self.repository = repository

    def list_releases(self) -> list[Release]:
        releases = []
        for item in self._get_pages("/repos/openai/codex/releases"):
            releases.append(
                Release(
                    tag=item["tag_name"],
                    published_at=item.get("published_at"),
                    draft=item["draft"],
                    url=item["html_url"],
                    prerelease=item["prerelease"],
                )
            )
        return releases

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

    def closed_synchronization(self) -> PullRequest | None:
        matches = [
            pull_request
            for pull_request in self._pull_requests("closed")
            if not pull_request.merged
            and pull_request.head_repository == self.repository
            and pull_request.head.startswith(SYNC_BRANCH_PREFIX)
        ]
        return self._only_pull_request(matches, "closed Synchronization")

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

    def reopen(self, number: int) -> tuple[int, str]:
        result = self._request(
            f"/repos/{self.repository}/pulls/{number}",
            method="PATCH",
            body={"state": "open"},
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

    def _get_pages(self, path: str, query: dict[str, str] | None = None) -> list[dict]:
        items = []
        for page in range(1, 11):
            page_query = {**(query or {}), "per_page": "100", "page": str(page)}
            result = self._request(f"{path}?{urllib.parse.urlencode(page_query)}")
            if not isinstance(result, list):
                raise SyncError(f"GitHub returned a non-list response for {path}")
            items.extend(result)
            if len(result) < 100:
                return items
        raise SyncError(f"GitHub pagination exceeded 1000 records for {path}")

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
        return SyncResult(
            outcome="open-pr-frozen",
            tag=_tag_from_title(frozen.title),
            release_commit=_commit_from_body(frozen.body) or frozen.head_sha,
            branch=frozen.head,
            pr_number=frozen.number,
            pr_url=frozen.url,
        )

    closed = pull_requests.closed_synchronization()
    if closed is not None:
        if config.manual_tag is not None:
            manual_release, _ = _select_release(
                releases.list_releases(), config.manual_tag
            )
            if manual_release.tag != _tag_from_title(closed.title):
                raise SyncError(
                    "manual release tag does not match the closed Synchronization PR"
                )
        return _reopen_closed(config, closed, pull_requests)

    release, selection_mode = _select_release(
        releases.list_releases(), config.manual_tag
    )
    release_commit = _fetch_release(config, release.tag)
    branch = f"{SYNC_BRANCH_PREFIX}{release_commit}"
    fork_head = _default_head(config)

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
        return SyncResult(
            outcome="open-pr-frozen",
            tag=release.tag,
            release_commit=release_commit,
            branch=branch,
            pr_number=existing.number,
            pr_url=existing.url,
        )
    if existing is not None:
        return _reopen_closed(config, existing, pull_requests)

    if _remote_branch_exists(config.repo_root, branch):
        prepared = _inspect_prepared_branch(config, fork_head, release_commit, branch)
    else:
        prepared = _prepare_branch(config, fork_head, release_commit, branch)
    intent = PullRequestIntent(
        title=f"Synchronize openai/codex {release.tag}",
        head=branch,
        base=config.default_branch,
        body=_pull_request_body(
            release,
            release_commit,
            selection_mode,
            prepared.mode,
            prepared.conflicts,
        ),
        draft=prepared.mode == "conflicting",
    )
    number, url = pull_requests.create(intent)
    outcome = (
        "pr-created-clean" if prepared.mode == "clean" else "draft-pr-created-conflicts"
    )
    return SyncResult(
        outcome=outcome,
        tag=release.tag,
        release_commit=release_commit,
        branch=branch,
        preparation_mode=prepared.mode,
        pr_number=number,
        pr_url=url,
        conflict_count=len(prepared.conflicts),
        conflicts=prepared.conflicts[:MAX_CONFLICTS_SHOWN],
    )


@dataclass(frozen=True)
class _PreparedBranch:
    mode: str
    conflicts: tuple[str, ...]


@dataclass(frozen=True)
class _MergeProbe:
    returncode: int
    stderr: str
    conflicts: tuple[str, ...]


def _reopen_closed(
    config: SyncConfig,
    pull_request: PullRequest,
    pull_requests: PullRequestService,
) -> SyncResult:
    tag = _tag_from_title(pull_request.title)
    release_commit = _commit_from_body(pull_request.body)
    branch_commit = pull_request.head.removeprefix(SYNC_BRANCH_PREFIX)
    if not tag or not re.fullmatch(r"[0-9a-f]{40}", release_commit):
        raise SyncError("closed Synchronization PR has invalid baseline metadata")
    if release_commit != branch_commit:
        raise SyncError("closed Synchronization PR branch does not match its baseline")

    fork_head = _default_head(config)
    if _commit_exists(config.repo_root, release_commit) and _is_ancestor(
        config.repo_root, release_commit, fork_head
    ):
        return SyncResult(
            outcome="already-integrated",
            tag=tag,
            release_commit=release_commit,
        )

    if _remote_branch_exists(config.repo_root, pull_request.head):
        prepared = _PreparedBranch(mode="", conflicts=())
    else:
        fetched_commit = _fetch_release(config, tag)
        if fetched_commit != release_commit:
            raise SyncError(f"release tag {tag} no longer resolves to {release_commit}")
        prepared = _prepare_branch(
            config,
            fork_head,
            release_commit,
            pull_request.head,
        )
    number, url = pull_requests.reopen(pull_request.number)
    return SyncResult(
        outcome="closed-pr-reopened",
        tag=tag,
        release_commit=release_commit,
        branch=pull_request.head,
        preparation_mode=prepared.mode,
        pr_number=number,
        pr_url=url,
        conflict_count=len(prepared.conflicts),
        conflicts=prepared.conflicts[:MAX_CONFLICTS_SHOWN],
    )


def _commit_exists(repo: Path, commit: str) -> bool:
    return (
        _run_git(repo, "rev-parse", "--verify", f"{commit}^{{commit}}").returncode == 0
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
            (release for release in eligible if release.tag == manual_tag), None
        )
        if selected is None:
            raise SyncError(
                f"{manual_tag!r} is not a published, non-draft Codex CLI release"
            )
        return selected, "manual"
    if not eligible:
        raise SyncError("no published Codex CLI release is available")
    return max(eligible, key=lambda release: release.published_at or ""), "automatic"


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


def _remote_branch_exists(repo: Path, branch: str) -> bool:
    process = _run_git(
        repo,
        "ls-remote",
        "--exit-code",
        "--heads",
        "origin",
        f"refs/heads/{branch}",
    )
    if process.returncode not in (0, 2):
        raise SyncError(process.stderr.strip())
    return process.returncode == 0


def _prepare_branch(
    config: SyncConfig,
    fork_head: str,
    release_commit: str,
    branch: str,
) -> _PreparedBranch:
    with tempfile.TemporaryDirectory(prefix="codex-upstream-sync-") as temp_dir:
        worktree = Path(temp_dir)
        _git(config.repo_root, "worktree", "add", "--detach", str(worktree), fork_head)
        try:
            probe = _probe_merge(worktree, release_commit)
            if probe.returncode == 0:
                parents = _git(worktree, "show", "-s", "--format=%P", "HEAD").split()
                if parents != [fork_head, release_commit]:
                    raise SyncError(
                        "clean synchronization did not create a two-parent merge"
                    )
                mode = "clean"
            elif probe.conflicts:
                _git(worktree, "merge", "--abort")
                _git(worktree, "reset", "--hard", release_commit)
                mode = "conflicting"
            else:
                raise SyncError(
                    f"merge failed without content conflicts: {probe.stderr}"
                )

            if _normalize_workspace_version(worktree):
                _git(worktree, "add", "codex-rs/Cargo.toml")
                _git(
                    worktree,
                    "commit",
                    "-m",
                    "Normalize Rust workspace version to 0.0.0",
                )
            _git(worktree, "diff", "--check")
            if _git(worktree, "status", "--porcelain"):
                raise SyncError("prepared synchronization worktree is not clean")
            _git(worktree, "push", "origin", f"HEAD:refs/heads/{branch}")
            return _PreparedBranch(mode=mode, conflicts=probe.conflicts)
        finally:
            _run_git(config.repo_root, "worktree", "remove", "--force", str(worktree))


def _inspect_prepared_branch(
    config: SyncConfig,
    fork_head: str,
    release_commit: str,
    branch: str,
) -> _PreparedBranch:
    _git(
        config.repo_root,
        "fetch",
        "--no-tags",
        "origin",
        f"refs/heads/{branch}",
    )
    head = _git(config.repo_root, "rev-parse", "FETCH_HEAD^{commit}")
    manifest = _git(config.repo_root, "show", f"{head}:codex-rs/Cargo.toml")
    if _workspace_version(manifest)[2]["version"] != "0.0.0":
        raise SyncError(f"refusing to use unnormalized branch {branch}")

    normalization_parent = _normalization_parent(config.repo_root, head)
    if head == release_commit or normalization_parent == release_commit:
        mode = "conflicting"
        conflicts = _conflicts_between(config, fork_head, release_commit)
    else:
        merge_commit = normalization_parent or head
        parents = _commit_parents(config.repo_root, merge_commit)
        if len(parents) != 2 or parents[1] != release_commit:
            raise SyncError(f"refusing to overwrite unowned branch {branch}")
        mode = "clean"
        conflicts = ()
    return _PreparedBranch(mode=mode, conflicts=conflicts)


def _normalization_parent(repo: Path, commit: str) -> str | None:
    parents = _commit_parents(repo, commit)
    if (
        len(parents) != 1
        or _git(repo, "show", "-s", "--format=%s", commit)
        != "Normalize Rust workspace version to 0.0.0"
        or _git(repo, "diff", "--name-only", parents[0], commit)
        != "codex-rs/Cargo.toml"
    ):
        return None
    return parents[0]


def _commit_parents(repo: Path, commit: str) -> list[str]:
    return _git(repo, "show", "-s", "--format=%P", commit).split()


def _conflicts_between(
    config: SyncConfig,
    fork_head: str,
    release_commit: str,
) -> tuple[str, ...]:
    with tempfile.TemporaryDirectory(prefix="codex-upstream-conflicts-") as temp_dir:
        worktree = Path(temp_dir)
        _git(config.repo_root, "worktree", "add", "--detach", str(worktree), fork_head)
        try:
            probe = _probe_merge(worktree, release_commit)
            if probe.returncode != 0 and not probe.conflicts:
                raise SyncError(
                    f"merge failed without content conflicts: {probe.stderr}"
                )
            return probe.conflicts
        finally:
            _run_git(config.repo_root, "worktree", "remove", "--force", str(worktree))


def _probe_merge(worktree: Path, release_commit: str) -> _MergeProbe:
    merge = _run_git(
        worktree,
        "merge",
        "--no-ff",
        "-m",
        f"Merge openai/codex release {release_commit}",
        release_commit,
    )
    conflicts = tuple(
        path
        for path in _git(
            worktree, "diff", "--name-only", "--diff-filter=U"
        ).splitlines()
        if path
    )
    return _MergeProbe(
        returncode=merge.returncode,
        stderr=merge.stderr.strip(),
        conflicts=conflicts,
    )


def _normalize_workspace_version(worktree: Path) -> bool:
    manifest = worktree / "codex-rs" / "Cargo.toml"
    try:
        text = manifest.read_text()
    except OSError as error:
        raise SyncError(f"cannot read Rust workspace version: {error}") from error
    lines, index, match = _workspace_version(text)
    if match["version"] == "0.0.0":
        return False
    lines[index] = f'{match["prefix"]}"0.0.0"{match["suffix"]}'
    manifest.write_text("".join(lines))
    if _workspace_version(manifest.read_text())[2]["version"] != "0.0.0":
        raise SyncError("failed to normalize Rust workspace package version")
    return True


def _workspace_version(text: str) -> tuple[list[str], int, re.Match[str]]:
    lines = text.splitlines(keepends=True)
    table_starts = [
        index
        for index, line in enumerate(lines)
        if re.fullmatch(r"\s*\[workspace\.package]\s*(?:#.*)?\n?", line)
    ]
    if len(table_starts) != 1:
        raise SyncError("Rust workspace package table is missing or ambiguous")
    start = table_starts[0] + 1
    end = next(
        (
            index
            for index in range(start, len(lines))
            if re.match(r"\s*\[", lines[index])
        ),
        len(lines),
    )
    version_lines = [
        index
        for index in range(start, end)
        if re.match(r"\s*version\s*=", lines[index])
    ]
    if len(version_lines) != 1:
        raise SyncError("Rust workspace package version is missing or ambiguous")
    index = version_lines[0]
    match = re.fullmatch(
        r'(?P<prefix>\s*version\s*=\s*)"(?P<version>[^"]+)"'
        r"(?P<suffix>\s*(?:#.*)?\n?)",
        lines[index],
    )
    if match is None:
        raise SyncError("Rust workspace package version is not a string literal")
    return lines, index, match


def _pull_request_body(
    release: Release,
    release_commit: str,
    selection_mode: str,
    preparation_mode: str,
    conflicts: tuple[str, ...],
) -> str:
    if conflicts:
        shown = "\n".join(f"- `{path}`" for path in conflicts[:MAX_CONFLICTS_SHOWN])
        conflict_context = (
            f"\n\nConflicts ({len(conflicts)} total; showing up to "
            f"{MAX_CONFLICTS_SHOWN}):\n{shown}"
        )
        next_action = (
            "Merge the current fork default branch, resolve every conflict, "
            "then mark this PR ready for review."
        )
    else:
        conflict_context = ""
        next_action = (
            "Review the synchronization, update it against the current default "
            "branch, and approve its workflow runs."
        )
    return f"""\
Synchronizes the published Codex CLI release `{release.tag}`.

- Upstream release: {release.url}
- Immutable commit: `{release_commit}`
- Selection: {selection_mode}
- Preparation: {preparation_mode}
- Rust workspace version: normalized to `0.0.0`

CI triggered by this `GITHUB_TOKEN`-created PR requires maintainer approval.

Next action: {next_action}{conflict_context}
"""


def _tag_from_title(title: str) -> str:
    match = re.search(r"\brust-v\S+", title)
    return match.group(0) if match else ""


def _commit_from_body(body: str) -> str:
    match = re.search(r"Immutable commit: `([0-9a-f]{40})`", body)
    return match.group(1) if match else ""


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
    path: str | None, result: SyncResult | None, error: str = ""
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
            "conflicts": json.dumps(result.conflicts),
        }
        if result is not None
        else {"outcome": "failure", "error": error.replace("\n", " ")}
    )
    with Path(path).open("a") as output:
        for key, value in values.items():
            print(f"{key}={value}", file=output)


def _write_summary(
    path: str | None, result: SyncResult | None, error: str = ""
) -> None:
    if not path:
        return
    lines = ["## Upstream synchronization", ""]
    if result is None:
        lines.extend(["- Outcome: failure", f"- Error: {error}"])
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
        if result.conflict_count:
            lines.append(f"- Conflicts: {result.conflict_count} total")
            lines.extend(f"  - `{path}`" for path in result.conflicts)
    with Path(path).open("a") as summary:
        summary.write("\n".join(lines) + "\n")


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
        _write_outputs(args.output, None, message)
        _write_summary(args.summary, None, message)
        print(message, file=os.sys.stderr)
        return 1
    _write_outputs(args.output, result)
    _write_summary(args.summary, result)
    print(json.dumps(result.__dict__, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
