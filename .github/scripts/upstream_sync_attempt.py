"""Prepare and inspect one immutable upstream Synchronization attempt."""

import os
import re
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

from upstream_sync_manifest import (
    MANIFEST_DIRECTORY,
    MANIFEST_SEED_COMMIT,
    ReleaseIdentity,
    SynchronizationManifest,
    manifest_path,
    parse_manifest,
    serialize_manifest,
    validate_chain,
)

SYNC_BRANCH_PREFIX = "automation/upstream-sync/"
_MAX_SYNCHRONIZATION_BRANCHES = 1_000
_NORMALIZATION_MESSAGE = "Normalize Rust workspace version to 0.0.0"
_PR153_RELEASE_COMMIT = MANIFEST_SEED_COMMIT
_PR153_MANIFEST_INTRODUCTION = "1fa5e1fa4167c1bce4060695024d738f8d68956e"
_PR153_MANIFEST_BLOB = "e05e54690ed0dd24891fc9fb41d7199a5ab7d3d2"
_PR153_MANIFEST_INTRODUCTION_MESSAGE = (
    "feat(sync): define strict synchronization manifest schema"
)
_SHA = re.compile(r"[0-9a-f]{40}")


class SyncError(RuntimeError):
    outcome = "failure"


class LegacyAttemptError(SyncError):
    outcome = "legacy-rejected"


@dataclass(frozen=True)
class PreparedAttempt:
    manifest: SynchronizationManifest
    branch: str
    head: str


@dataclass(frozen=True)
class PreparationEvidence:
    """Evidence returned after validating the immutable preparation graph."""

    baseline_reconciliation: str | None


def prepare_attempt(
    repo: Path,
    release: ReleaseIdentity,
    fork_base_sha: str,
    selection_mode: str,
    *,
    seed_commit: str = MANIFEST_SEED_COMMIT,
) -> PreparedAttempt:
    branch = f"{SYNC_BRANCH_PREFIX}{release.commit}"
    if _remote_branch_exists(repo, branch):
        prepared = _remote_attempt(repo, branch, seed_commit=seed_commit)
        if prepared is None:
            raise LegacyAttemptError(
                f"refusing legacy Synchronization branch without manifest {branch}"
            )
        if prepared.manifest.release != release:
            raise SyncError(f"manifest does not own Synchronization branch {branch}")
        _verify_prepared(repo, prepared, seed_commit=seed_commit)
        return prepared

    _, fork_chain, predecessor = _read_chain_at(
        repo, fork_base_sha, seed_commit=seed_commit
    )
    if not _release_lineage_advances(
        repo, predecessor.release.commit, release.commit
    ):
        raise SyncError(
            "selected release does not descend from the manifest predecessor"
        )
    if _manifest_texts_at(repo, release.commit):
        raise SyncError(
            "selected release contains fork-owned Synchronization manifests"
        )

    with tempfile.TemporaryDirectory(prefix="codex-upstream-sync-") as temp_dir:
        worktree = Path(temp_dir)
        _git(repo, "worktree", "add", "--detach", str(worktree), fork_base_sha)
        try:
            returncode, stderr, conflicts = _merge(worktree, release.commit)
            if returncode == 0:
                if _parents(worktree, "HEAD") != [fork_base_sha, release.commit]:
                    raise SyncError(
                        "clean synchronization did not create a two-parent merge"
                    )
                preparation_mode = "clean"
            elif conflicts:
                _git(worktree, "merge", "--abort")
                _git(worktree, "reset", "--hard", release.commit)
                preparation_mode = "conflicting"
            else:
                raise SyncError(f"merge failed without content conflicts: {stderr}")

            if _normalize_workspace_version(worktree):
                _git(worktree, "add", "codex-rs/Cargo.toml")
                _git(worktree, "commit", "-m", _NORMALIZATION_MESSAGE)
            if preparation_mode == "conflicting":
                _git(worktree, "checkout", fork_base_sha, "--", MANIFEST_DIRECTORY)
            manifest = SynchronizationManifest(
                1,
                release,
                fork_base_sha,
                predecessor.release.commit,
                selection_mode,
                preparation_mode,
                conflicts,
            )
            try:
                validate_chain((*fork_chain, manifest), seed_commit=seed_commit)
                text = serialize_manifest(manifest)
            except ValueError as error:
                raise SyncError(str(error)) from error
            target = worktree / manifest_path(release.commit)
            target.parent.mkdir(parents=True, exist_ok=True)
            if target.exists():
                raise SyncError(f"Synchronization manifest already exists: {target}")
            target.write_text(text)
            _git(worktree, "add", "-A", MANIFEST_DIRECTORY)
            _git(
                worktree,
                "commit",
                "-m",
                f"Record Synchronization manifest for {release.tag}",
            )
            _git(worktree, "diff", "--check")
            if _git(worktree, "status", "--porcelain"):
                raise SyncError("prepared synchronization worktree is not clean")
            head = _git(worktree, "rev-parse", "HEAD")
        finally:
            _git(repo, "worktree", "remove", "--force", str(worktree))

    prepared = PreparedAttempt(manifest, branch, head)
    _verify_prepared(repo, prepared, seed_commit=seed_commit)
    create_only_lease = f"--force-with-lease=refs/heads/{branch}:"
    _git(repo, "push", create_only_lease, "origin", f"{head}:refs/heads/{branch}")
    return prepared


def inspect_open_attempt(
    repo: Path,
    branch: str,
    expected_head: str,
    *,
    seed_commit: str = MANIFEST_SEED_COMMIT,
) -> PreparedAttempt | None:
    prepared = _remote_attempt(repo, branch, expected_head, seed_commit=seed_commit)
    if (
        prepared is not None
        and prepared.manifest.release.commit != synchronization_release_commit(branch)
    ):
        raise SyncError(f"manifest does not own Synchronization branch {branch}")
    if prepared is not None:
        path = manifest_path(prepared.manifest.release.commit)
        manifest_head = _git(
            repo, "log", "-1", "--format=%H", prepared.head, "--", path
        )
        if _manifest_texts_at(repo, prepared.head) != _manifest_texts_at(
            repo, manifest_head
        ):
            raise SyncError(
                "open Synchronization manifest directory changed after introduction"
            )
        changes = _git(
            repo,
            "log",
            "--first-parent",
            "--format=%H",
            f"{manifest_head}..{prepared.head}",
            "--",
            MANIFEST_DIRECTORY,
        )
        if changes:
            raise SyncError(
                "open Synchronization manifest history changed after introduction"
            )
        _verify_prepared(
            repo,
            PreparedAttempt(prepared.manifest, branch, manifest_head),
            seed_commit=seed_commit,
        )
    return prepared


def synchronization_branches(repo: Path) -> tuple[tuple[str, str], ...]:
    output = _git(
        repo,
        "ls-remote",
        "--heads",
        "origin",
        f"refs/heads/{SYNC_BRANCH_PREFIX}*",
    )
    branches = []
    for line in output.splitlines():
        head, separator, ref = line.partition("\t")
        prefix = "refs/heads/"
        if (
            separator != "\t"
            or _SHA.fullmatch(head) is None
            or not ref.startswith(prefix)
        ):
            raise SyncError("invalid Synchronization branch listing")
        branch = ref.removeprefix(prefix)
        synchronization_release_commit(branch)
        branches.append((branch, head))
    if len(branches) > _MAX_SYNCHRONIZATION_BRANCHES:
        raise SyncError("Synchronization branch listing exceeds its record limit")
    return tuple(sorted(branches))


def inspect_retry_attempt(
    repo: Path,
    branch: str,
    expected_head: str,
    *,
    seed_commit: str = MANIFEST_SEED_COMMIT,
) -> PreparedAttempt:
    prepared = _remote_attempt(repo, branch, expected_head, seed_commit=seed_commit)
    if prepared is None:
        raise LegacyAttemptError(
            f"refusing legacy Synchronization branch without manifest {branch}"
        )
    _verify_prepared(repo, prepared, seed_commit=seed_commit)
    return prepared


def _remote_attempt(
    repo: Path,
    branch: str,
    expected_head: str | None = None,
    *,
    seed_commit: str = MANIFEST_SEED_COMMIT,
) -> PreparedAttempt | None:
    release_commit = synchronization_release_commit(branch)
    _git(repo, "fetch", "--no-tags", "origin", f"refs/heads/{branch}")
    head = _git(repo, "rev-parse", "FETCH_HEAD^{commit}")
    if expected_head is not None and head != expected_head:
        raise SyncError("open Synchronization PR head changed during inspection")
    active = manifest_path(release_commit)
    if active not in _manifest_texts_at(repo, head):
        history = _git(repo, "log", "--format=%H", head, "--", active)
        if history:
            raise SyncError(
                "active Synchronization manifest was removed from branch history"
            )
        return None
    _, _, manifest = _read_chain_at(repo, head, seed_commit=seed_commit)
    return PreparedAttempt(manifest, branch, head)


def _verify_prepared(
    repo: Path,
    prepared: PreparedAttempt,
    *,
    seed_commit: str = MANIFEST_SEED_COMMIT,
) -> PreparationEvidence:
    manifest = prepared.manifest
    if manifest.release.commit != synchronization_release_commit(prepared.branch):
        raise SyncError(
            f"manifest does not own Synchronization branch {prepared.branch}"
        )
    head_texts, _, tip = _read_chain_at(repo, prepared.head, seed_commit=seed_commit)
    fork_texts, _, fork_tip = _read_chain_at(
        repo, manifest.fork_base_sha, seed_commit=seed_commit
    )
    active = manifest_path(manifest.release.commit)
    if (
        tip != manifest
        or manifest.previous_release_commit != fork_tip.release.commit
        or head_texts != {**fork_texts, active: head_texts.get(active, "")}
    ):
        raise SyncError("prepared branch Synchronization manifest chain drift")

    parents = _parents(repo, prepared.head)
    if len(parents) != 1 or _git(repo, "show", "-s", "--format=%s", prepared.head) != (
        f"Record Synchronization manifest for {manifest.release.tag}"
    ):
        raise SyncError("prepared branch has an unexpected manifest commit")
    manifest_parent = parents[0]
    parent_texts = fork_texts if manifest.preparation_mode == "clean" else {}
    if _manifest_texts_at(repo, manifest_parent) != parent_texts:
        raise SyncError("prepared branch manifest commit contains chain drift")
    changed = tuple(sorted(_git_paths(repo, "diff", manifest_parent, prepared.head)))
    expected = tuple(sorted(path for path in head_texts if path not in parent_texts))
    if changed != expected:
        raise SyncError("prepared branch manifest commit changed unexpected paths")

    cargo = _git(repo, "show", f"{prepared.head}:codex-rs/Cargo.toml", strip=False)
    if _workspace_version(cargo)["version"] != "0.0.0":
        raise SyncError(f"refusing to use unnormalized branch {prepared.branch}")
    prepared_parent = manifest_parent
    if (
        manifest.preparation_mode == "clean"
        or manifest_parent != manifest.release.commit
    ):
        prepared_parent = (
            _normalization_parent(repo, manifest_parent) or manifest_parent
        )
    returncode, tree, conflicts = _merge_tree(
        repo, manifest.fork_base_sha, manifest.release.commit
    )
    if conflicts != manifest.conflict_paths:
        raise SyncError(
            "prepared branch conflict evidence differs from frozen baselines"
        )
    if manifest.preparation_mode == "clean":
        if (
            returncode != 0
            or _parents(repo, prepared_parent)
            != [manifest.fork_base_sha, manifest.release.commit]
            or _git(repo, "show", "-s", "--format=%T", prepared_parent) != tree
        ):
            raise SyncError("clean branch has an unexpected Baseline reconciliation")
        return PreparationEvidence(prepared_parent)
    elif prepared_parent != manifest.release.commit or _is_ancestor(
        repo, manifest.fork_base_sha, prepared.head
    ):
        raise SyncError("conflicting branch has unexpected Fork ancestry")
    return PreparationEvidence(None)


def _read_chain_at(
    repo: Path,
    commit: str,
    *,
    seed_commit: str = MANIFEST_SEED_COMMIT,
):
    texts = _manifest_texts_at(repo, commit)
    manifests = []
    for path, text in sorted(texts.items()):
        _verify_manifest_history(repo, commit, path, seed_commit=seed_commit)
        try:
            manifest = parse_manifest(text)
        except ValueError as error:
            raise SyncError(
                f"invalid Synchronization manifest {path}: {error}"
            ) from error
        if path != manifest_path(manifest.release.commit):
            raise SyncError(f"Synchronization manifest filename does not match {path}")
        manifests.append(manifest)
    try:
        tip = validate_chain(tuple(manifests), seed_commit=seed_commit)
    except ValueError as error:
        raise SyncError(str(error)) from error
    return texts, tuple(manifests), tip


def _manifest_texts_at(repo: Path, commit: str) -> dict[str, str]:
    output = _git(
        repo,
        "ls-tree",
        "-r",
        "-z",
        commit,
        "--",
        MANIFEST_DIRECTORY,
        strip=False,
    )
    if output and not output.endswith("\0"):
        raise SyncError("Git tree entries are not NUL terminated")
    texts = {}
    for record in output[:-1].split("\0") if output else ():
        metadata, separator, path = record.partition("\t")
        fields = metadata.split()
        if separator != "\t" or len(fields) != 3:
            raise SyncError("invalid Synchronization manifest tree entry")
        mode, kind, object_id = fields
        if mode != "100644" or kind != "blob" or _SHA.fullmatch(object_id) is None:
            raise SyncError("Synchronization manifests must be regular blobs")
        texts[path] = _git(repo, "cat-file", "blob", object_id, strip=False)
    return texts


def _verify_manifest_history(
    repo: Path,
    commit: str,
    path: str,
    *,
    seed_commit: str = MANIFEST_SEED_COMMIT,
) -> None:
    if (
        path == manifest_path(_PR153_RELEASE_COMMIT)
        and seed_commit == MANIFEST_SEED_COMMIT
    ):
        _verify_pr153_manifest_history(repo, commit, path)
        return
    changes = _git(
        repo,
        "log",
        "--first-parent",
        "--reverse",
        "--format=%H",
        commit,
        "--",
        path,
    ).splitlines()
    if (
        path == manifest_path(seed_commit)
        and len(changes) >= 2
        and _git(repo, "show", "-s", "--format=%s", changes[-1])
        == _PR153_MANIFEST_INTRODUCTION_MESSAGE
        and _tree_entry(repo, commit, path) == _tree_entry(repo, changes[-1], path)
    ):
        return
    if len(changes) != 1:
        raise SyncError(
            f"Synchronization manifest history changed after introduction: {path}"
        )
    if _tree_entry(repo, commit, path) != _tree_entry(repo, changes[0], path):
        raise SyncError(
            f"Synchronization manifest differs from its introduction: {path}"
        )


def _verify_pr153_manifest_history(repo: Path, commit: str, path: str) -> None:
    expected_entry = ("100644", "blob", _PR153_MANIFEST_BLOB)
    if _tree_entry(repo, commit, path) != expected_entry:
        raise SyncError("PR #153 seed manifest differs from its canonical blob")

    canonical_commit = _run_git(
        repo, "cat-file", "-e", f"{_PR153_MANIFEST_INTRODUCTION}^{{commit}}"
    )
    if canonical_commit.returncode == 0 and _is_ancestor(
        repo, _PR153_MANIFEST_INTRODUCTION, commit
    ):
        introduction = _PR153_MANIFEST_INTRODUCTION
        if _tree_entry(repo, introduction, path) != expected_entry:
            raise SyncError(
                "PR #153 seed manifest canonical introduction differs from its blob"
            )
    else:
        changes = _git(
            repo,
            "log",
            "--first-parent",
            "--reverse",
            "--format=%H",
            commit,
            "--",
            path,
        ).splitlines()
        canonical_introductions = [
            change
            for change in changes
            if _git(repo, "show", "-s", "--format=%s", change)
            == _PR153_MANIFEST_INTRODUCTION_MESSAGE
            and _tree_entry(repo, change, path) == expected_entry
        ]
        if len(canonical_introductions) == 1:
            introduction = canonical_introductions[0]
        elif (
            len(changes) == 1
            and _tree_entry(repo, changes[0], path) == expected_entry
            and _git(repo, "show", "-s", "--format=%s", changes[0]).startswith(
                "Record Synchronization manifest for "
            )
        ):
            introduction = changes[0]
        else:
            raise SyncError("PR #153 seed manifest history is not anchored")

    changes = _git(
        repo,
        "log",
        "--first-parent",
        "--format=%H",
        f"{introduction}..{commit}",
        "--",
        path,
    ).splitlines()
    for change in changes:
        if _tree_entry(repo, change, path) != expected_entry:
            raise SyncError("PR #153 seed manifest history changed after introduction")


def _tree_entry(repo: Path, commit: str, path: str) -> tuple[str, str, str] | None:
    output = _git(repo, "ls-tree", "-z", commit, "--", path, strip=False)
    if not output:
        return None
    records = output.removesuffix("\0").split("\0")
    if not output.endswith("\0") or len(records) != 1:
        raise SyncError(f"invalid Git tree entry for {path}")
    metadata, separator, actual_path = records[0].partition("\t")
    fields = metadata.split()
    if separator != "\t" or actual_path != path or len(fields) != 3:
        raise SyncError(f"invalid Git tree entry for {path}")
    mode, kind, object_id = fields
    return mode, kind, object_id


def _merge_tree(repo: Path, fork_base_sha: str, release_commit: str):
    process = _run_git(
        repo,
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        "-z",
        fork_base_sha,
        release_commit,
    )
    if process.returncode not in (0, 1):
        raise SyncError(process.stderr.strip())
    fields = process.stdout.split("\0")
    return process.returncode, fields[0], tuple(sorted(set(filter(None, fields[1:]))))


def _merge(worktree: Path, release_commit: str) -> tuple[int, str, tuple[str, ...]]:
    merge = _run_git(
        worktree,
        "merge",
        "--no-ff",
        "-m",
        f"Merge openai/codex release {release_commit}",
        release_commit,
    )
    conflicts = tuple(sorted(set(_git_paths(worktree, "diff", "--diff-filter=U"))))
    return merge.returncode, merge.stderr.strip(), conflicts


def _normalization_parent(repo: Path, commit: str) -> str | None:
    parents = _parents(repo, commit)
    if (
        len(parents) != 1
        or _git(repo, "show", "-s", "--format=%s", commit) != _NORMALIZATION_MESSAGE
    ):
        return None
    if _git_paths(repo, "diff", parents[0], commit) != ("codex-rs/Cargo.toml",):
        raise SyncError("normalization commit changed unexpected paths")
    before = _git(repo, "show", f"{parents[0]}:codex-rs/Cargo.toml", strip=False)
    after = _git(repo, "show", f"{commit}:codex-rs/Cargo.toml", strip=False)
    before_kind = _git(
        repo, "ls-tree", parents[0], "--", "codex-rs/Cargo.toml"
    ).split()[:2]
    after_kind = _git(repo, "ls-tree", commit, "--", "codex-rs/Cargo.toml").split()[:2]
    if (
        before == after
        or _normalized_workspace_version(before) != after
        or before_kind != after_kind
    ):
        raise SyncError("normalization commit is not the exact workspace version edit")
    return parents[0]


def _normalize_workspace_version(worktree: Path) -> bool:
    path = worktree / "codex-rs/Cargo.toml"
    text = path.read_text()
    normalized = _normalized_workspace_version(text)
    if normalized == text:
        return False
    path.write_text(normalized)
    return True


def _normalized_workspace_version(text: str) -> str:
    match = _workspace_version(text)
    if match["version"] != "0.0.0":
        return text[: match.start("version")] + "0.0.0" + text[match.end("version") :]
    return text


def _workspace_version(text: str) -> re.Match[str]:
    tables = list(
        re.finditer(
            r"(?ms)^\s*\[workspace\.package]\s*(?:#.*)?$\n(?P<body>.*?)(?=^\s*\[|\Z)",
            text,
        )
    )
    if len(tables) != 1:
        raise SyncError("Rust workspace package table is missing or ambiguous")
    pattern = re.compile(
        r'^(?P<prefix>\s*version\s*=\s*)"(?P<version>[^"]+)"(?P<suffix>\s*(?:#.*)?)$',
        re.MULTILINE,
    )
    versions = list(
        pattern.finditer(text, tables[0].start("body"), tables[0].end("body"))
    )
    if len(versions) != 1:
        raise SyncError("Rust workspace package version is missing or ambiguous")
    return versions[0]


def synchronization_release_commit(branch: str) -> str:
    """Return the immutable release SHA encoded by a Synchronization branch."""
    commit = branch.removeprefix(SYNC_BRANCH_PREFIX)
    if not branch.startswith(SYNC_BRANCH_PREFIX) or _SHA.fullmatch(commit) is None:
        raise SyncError("Synchronization branch has invalid ownership")
    return commit


def _parents(repo: Path, commit: str) -> list[str]:
    return _git(repo, "show", "-s", "--format=%P", commit).split()


def _release_lineage_advances(repo: Path, predecessor: str, release: str) -> bool:
    if _is_ancestor(repo, predecessor, release):
        return True
    predecessor_parents = _parents(repo, predecessor)
    release_parents = _parents(repo, release)
    return (
        len(predecessor_parents) == 1
        and len(release_parents) == 1
        and _is_ancestor(repo, predecessor_parents[0], release_parents[0])
    )


def _is_ancestor(repo: Path, ancestor: str, descendant: str) -> bool:
    process = _run_git(repo, "merge-base", "--is-ancestor", ancestor, descendant)
    if process.returncode not in (0, 1):
        raise SyncError(process.stderr.strip())
    return process.returncode == 0


def _remote_branch_exists(repo: Path, branch: str) -> bool:
    process = _run_git(
        repo, "ls-remote", "--exit-code", "--heads", "origin", f"refs/heads/{branch}"
    )
    if process.returncode not in (0, 2):
        raise SyncError(process.stderr.strip())
    return process.returncode == 0


def _git_paths(repo: Path, command: str, *args: str) -> tuple[str, ...]:
    output = _git(repo, command, "--name-only", "-z", *args, strip=False)
    if output and not output.endswith("\0"):
        raise SyncError("Git paths are not NUL terminated")
    return tuple(output[:-1].split("\0")) if output else ()


def _run_git(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    email = "41898282+github-actions[bot]@users.noreply.github.com"
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "github-actions[bot]",
        "GIT_AUTHOR_EMAIL": email,
        "GIT_COMMITTER_NAME": "github-actions[bot]",
        "GIT_COMMITTER_EMAIL": email,
    }
    return subprocess.run(
        ["git", *args],
        cwd=repo,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )


def _git(repo: Path, *args: str, strip: bool = True) -> str:
    process = _run_git(repo, *args)
    if process.returncode != 0:
        raise SyncError(
            f"git {' '.join(args)} failed ({process.returncode}): {process.stderr.strip()}"
        )
    return process.stdout.strip() if strip else process.stdout
