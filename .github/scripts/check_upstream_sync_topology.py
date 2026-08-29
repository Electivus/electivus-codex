#!/usr/bin/env python3
"""Validate the immutable Git topology of an upstream Synchronization PR."""

import argparse
import os
import sys
from dataclasses import dataclass
from pathlib import Path

from sync_upstream_release import _bounded_diagnostic
from upstream_sync_attempt import (
    _SHA,
    SYNC_BRANCH_PREFIX,
    PreparedAttempt,
    SyncError,
    _git,
    _git_paths,
    _is_ancestor,
    _merge_tree,
    _read_chain_at,
    _run_git,
    _verify_prepared,
    synchronization_release_commit,
)
from upstream_sync_manifest import (
    MANIFEST_DIRECTORY,
    MANIFEST_SEED_COMMIT,
    SynchronizationManifest,
)

_NORMALIZATION_MESSAGE = "Normalize Rust workspace version to 0.0.0"
_MANIFEST_COMMIT_PREFIX = "Record Synchronization manifest for "


class TopologyError(RuntimeError):
    """Raised when a Synchronization PR cannot prove its required topology."""


@dataclass(frozen=True)
class TopologyEvidence:
    """Bounded evidence emitted after a Synchronization PR passes validation."""

    head_sha: str
    base_sha: str
    branch: str
    fork_base_sha: str
    release_commit: str
    manifest_introduction: str
    preparation_mode: str
    baseline_reconciliation: str
    catch_up_merge: str | None


def validate_topology(
    repo: Path,
    head_sha: str,
    base_sha: str,
    head_branch: str,
    *,
    seed_commit: str = MANIFEST_SEED_COMMIT,
) -> TopologyEvidence | None:
    """Validate one real PR head/base pair, or return ``None`` if inapplicable.

    The caller must pass the pull request's real head and base SHAs. A GitHub
    synthetic merge ref is deliberately not accepted as a substitute for the
    real head.
    """
    if not head_branch.startswith(SYNC_BRANCH_PREFIX):
        return None

    try:
        return _validate_synchronization_topology(
            repo,
            head_sha,
            base_sha,
            head_branch,
            seed_commit=seed_commit,
        )
    except SyncError as error:
        raise TopologyError(str(error)) from error


def _validate_synchronization_topology(
    repo: Path,
    head_sha: str,
    base_sha: str,
    head_branch: str,
    *,
    seed_commit: str,
) -> TopologyEvidence:
    _require_sha(head_sha, "real PR head")
    _require_sha(base_sha, "real PR base")
    if head_sha == base_sha:
        raise TopologyError("real PR head and base must be different commits")
    _ensure_complete_history(repo)
    _ensure_no_replace_refs(repo)
    _require_commit(repo, head_sha, "real PR head")
    _require_commit(repo, base_sha, "real PR base")

    try:
        release_commit = synchronization_release_commit(head_branch)
    except SyncError as error:
        raise TopologyError(str(error)) from error
    if release_commit != release_commit.lower():
        raise TopologyError("Synchronization branch ownership must use lowercase SHA")
    _require_commit(repo, release_commit, "Synchronization release")

    try:
        head_texts, head_manifests, manifest = _read_chain_at(
            repo, head_sha, seed_commit=seed_commit
        )
    except SyncError as error:
        raise TopologyError(f"manifest chain is invalid: {error}") from error
    active_path = f"{MANIFEST_DIRECTORY}/{release_commit}.json"
    if manifest.release.commit != release_commit:
        raise TopologyError(
            "Synchronization branch does not point at the manifest chain tip "
            f"for {release_commit}"
        )
    if active_path not in head_texts:
        raise TopologyError(
            "active Synchronization manifest is missing from real PR head"
        )

    for candidate in head_manifests:
        _require_commit(repo, candidate.release.commit, "manifest release")
        if candidate.previous_release_commit is not None:
            _require_commit(
                repo, candidate.previous_release_commit, "manifest predecessor"
            )
            if not _is_ancestor(
                repo, candidate.previous_release_commit, candidate.release.commit
            ):
                raise TopologyError(
                    "manifest predecessor is not an ancestor of its release commit"
                )

    try:
        _, fork_manifests, fork_tip = _read_chain_at(
            repo, manifest.fork_base_sha, seed_commit=seed_commit
        )
    except SyncError as error:
        raise TopologyError(
            f"fork baseline manifest chain is invalid: {error}"
        ) from error
    for candidate in fork_manifests:
        _require_commit(repo, candidate.release.commit, "fork manifest release")
        if candidate.previous_release_commit is not None:
            _require_commit(
                repo,
                candidate.previous_release_commit,
                "fork manifest predecessor",
            )
            if not _is_ancestor(
                repo, candidate.previous_release_commit, candidate.release.commit
            ):
                raise TopologyError(
                    "fork manifest predecessor is not an ancestor of its release commit"
                )

    try:
        _, _, base_tip = _read_chain_at(repo, base_sha, seed_commit=seed_commit)
    except SyncError as error:
        raise TopologyError(
            f"real PR base manifest chain is invalid: {error}"
        ) from error
    if base_tip.release.commit != manifest.previous_release_commit:
        raise TopologyError(
            "real PR base manifest chain tip does not match the immutable predecessor"
        )
    if fork_tip.release.commit != manifest.previous_release_commit:
        raise TopologyError(
            "active Synchronization manifest does not bind to the fork chain tip"
        )
    _require_commit(repo, manifest.fork_base_sha, "frozen Fork baseline")
    if not _is_ancestor(repo, manifest.fork_base_sha, base_sha):
        raise TopologyError(
            "real PR base does not descend from the immutable Fork baseline"
        )
    if not _is_ancestor(repo, base_sha, head_sha):
        raise TopologyError(
            "real PR head is stale: it does not contain the real PR base"
        )
    if not _is_ancestor(repo, manifest.release.commit, head_sha):
        raise TopologyError(
            "real PR head does not contain the immutable Synchronization release"
        )

    manifest_introduction = _manifest_introduction(repo, head_sha, active_path)
    branch = head_branch
    try:
        preparation = _verify_prepared(
            repo,
            PreparedAttempt(manifest, branch, manifest_introduction),
            seed_commit=seed_commit,
        )
    except SyncError as error:
        raise TopologyError(
            f"immutable preparation graph is invalid: {error}"
        ) from error

    changes = _git(
        repo,
        "log",
        "--first-parent",
        "--format=%H",
        f"{manifest_introduction}..{head_sha}",
        "--",
        MANIFEST_DIRECTORY,
    )
    if changes:
        raise TopologyError(
            "Synchronization manifest history changed after its introduction"
        )

    history = _first_parent_history(repo, manifest_introduction, head_sha)
    merges = _validate_new_commits(repo, history)
    if manifest.preparation_mode == "clean":
        baseline_reconciliation = preparation.baseline_reconciliation
        if baseline_reconciliation is None:
            raise TopologyError(
                "clean preparation did not return a Baseline reconciliation"
            )
        catch_up = _select_clean_catch_up(
            repo, manifest, baseline_reconciliation, merges, base_sha
        )
    elif manifest.preparation_mode == "conflicting":
        baseline_reconciliation, catch_up = _select_conflicting_reconciliations(
            repo, manifest, manifest_introduction, merges, base_sha
        )
    else:
        raise TopologyError("Synchronization preparation mode is unsupported")

    return TopologyEvidence(
        head_sha=head_sha,
        base_sha=base_sha,
        branch=branch,
        fork_base_sha=manifest.fork_base_sha,
        release_commit=manifest.release.commit,
        manifest_introduction=manifest_introduction,
        preparation_mode=manifest.preparation_mode,
        baseline_reconciliation=baseline_reconciliation,
        catch_up_merge=catch_up,
    )


def _require_sha(value: str, label: str) -> None:
    if _SHA.fullmatch(value) is None:
        raise TopologyError(f"{label} must be a complete 40-character lowercase SHA")


def _require_commit(repo: Path, commit: str, label: str) -> None:
    _require_sha(commit, label)
    process = _run_git(repo, "cat-file", "-e", f"{commit}^{{commit}}")
    if process.returncode != 0:
        raise TopologyError(
            f"{label} {commit} is unavailable; complete Git history is required"
        )


def _ensure_complete_history(repo: Path) -> None:
    process = _run_git(repo, "rev-parse", "--is-shallow-repository")
    if process.returncode != 0 or process.stdout.strip() != "false":
        raise TopologyError(
            "complete Git history is required for Synchronization topology validation"
        )


def _ensure_no_replace_refs(repo: Path) -> None:
    replacements = _git(repo, "replace", "-l")
    if replacements:
        raise TopologyError(
            "Git replacement refs are not allowed during Synchronization topology validation"
        )


def _manifest_introduction(repo: Path, head: str, path: str) -> str:
    changes = _git(
        repo,
        "log",
        "--first-parent",
        "--reverse",
        "--format=%H",
        head,
        "--",
        path,
    ).splitlines()
    if len(changes) != 1:
        raise TopologyError(
            "active Synchronization manifest must have exactly one introduction"
        )
    return changes[0]


def _first_parent_history(
    repo: Path,
    introduction: str,
    head: str,
) -> tuple[tuple[str, tuple[str, ...]], ...]:
    output = _git(
        repo,
        "rev-list",
        "--first-parent",
        "--reverse",
        "--parents",
        f"{introduction}..{head}",
    )
    history = []
    for line in output.splitlines():
        fields = line.split()
        if not fields or any(_SHA.fullmatch(field) is None for field in fields):
            raise TopologyError("Git history contains an invalid commit record")
        history.append((fields[0], tuple(fields[1:])))
    return tuple(history)


def _validate_new_commits(
    repo: Path,
    history: tuple[tuple[str, tuple[str, ...]], ...],
) -> tuple[tuple[int, str, tuple[str, ...]], ...]:
    merges = []
    for index, (commit, parents) in enumerate(history):
        if len(parents) > 2:
            raise TopologyError(
                f"unsupported octopus merge after manifest introduction: {commit}"
            )
        subject = _git(repo, "show", "-s", "--format=%s", commit)
        if subject == _NORMALIZATION_MESSAGE or subject.startswith(
            _MANIFEST_COMMIT_PREFIX
        ):
            raise TopologyError(
                "deterministic manifest/version commits cannot replace a reconciliation"
            )
        if len(parents) != 2:
            raise TopologyError(
                f"unsupported single-parent commit after manifest introduction: {commit}"
            )
        merges.append((index, commit, parents))
    return tuple(merges)


def _select_clean_catch_up(
    repo: Path,
    manifest: SynchronizationManifest,
    baseline: str,
    merges: tuple[tuple[int, str, tuple[str, ...]], ...],
    base_sha: str,
) -> str | None:
    if base_sha == manifest.fork_base_sha:
        if merges:
            raise TopologyError(
                "Fork-first Synchronization cannot contain an extra reconciliation"
            )
        return None
    if len(merges) != 1:
        raise TopologyError(
            "advanced Fork baseline requires exactly one attributable Catch-up merge"
        )
    _, commit, parents = merges[0]
    if parents[1] != base_sha:
        raise TopologyError(
            "Catch-up merge must use the real current PR base as its second parent"
        )
    if not _is_ancestor(repo, baseline, parents[0]):
        raise TopologyError(
            "Catch-up merge does not descend from Baseline reconciliation"
        )
    _validate_reconciliation_tree(repo, commit, parents, "Catch-up")
    return commit


def _select_conflicting_reconciliations(
    repo: Path,
    manifest: SynchronizationManifest,
    manifest_introduction: str,
    merges: tuple[tuple[int, str, tuple[str, ...]], ...],
    base_sha: str,
) -> tuple[str, str | None]:
    baseline_matches = [
        merge for merge in merges if merge[2][1] == manifest.fork_base_sha
    ]
    if len(baseline_matches) != 1:
        raise TopologyError(
            "release-first Synchronization requires exactly one Fork-second Baseline reconciliation"
        )
    baseline_index, baseline, baseline_parents = baseline_matches[0]
    catch_up_matches = [merge for merge in merges if merge[2][1] == base_sha]
    if base_sha != manifest.fork_base_sha:
        if len(catch_up_matches) != 1:
            raise TopologyError(
                "advanced release-first Synchronization requires exactly one Catch-up merge"
            )
        catch_up_index, catch_up, catch_up_parents = catch_up_matches[0]
        if catch_up_index <= baseline_index:
            raise TopologyError("Catch-up merge must follow Baseline reconciliation")
    else:
        catch_up = None
        catch_up_parents = ()
    if not _is_ancestor(repo, manifest_introduction, baseline_parents[0]):
        raise TopologyError(
            "Baseline reconciliation does not descend from the manifest introduction"
        )
    if not _is_ancestor(repo, baseline_parents[0], baseline):
        raise TopologyError("Baseline reconciliation has an invalid first parent")
    if _is_ancestor(repo, manifest.fork_base_sha, baseline_parents[0]):
        raise TopologyError(
            "Baseline reconciliation was already contaminated by Fork ancestry"
        )
    _validate_reconciliation_tree(
        repo,
        baseline,
        baseline_parents,
        "Baseline",
    )

    if base_sha == manifest.fork_base_sha:
        if len(merges) != 1:
            raise TopologyError(
                "Fork-second Baseline reconciliation is duplicated or misplaced"
            )
        return baseline, None

    if not _is_ancestor(repo, baseline, catch_up_parents[0]):
        raise TopologyError(
            "Catch-up merge does not descend from Baseline reconciliation"
        )
    _validate_reconciliation_tree(repo, catch_up, catch_up_parents, "Catch-up")
    if len(merges) != 2:
        raise TopologyError("Synchronization contains an unsupported extra merge")
    return baseline, catch_up


def _validate_reconciliation_tree(
    repo: Path,
    commit: str,
    parents: tuple[str, ...],
    label: str,
) -> None:
    returncode, automatic_tree, conflict_paths = _merge_tree(
        repo,
        parents[0],
        parents[1],
    )
    actual_tree = _git(repo, "show", "-s", "--format=%T", commit)
    if returncode == 0:
        if actual_tree != automatic_tree:
            raise TopologyError(
                f"{label} merge tree does not match Git's conflict-free result"
            )
        return

    changed_paths = set(_git_paths(repo, "diff", automatic_tree, actual_tree))
    unexpected_paths = sorted(changed_paths.difference(conflict_paths))
    if unexpected_paths:
        raise TopologyError(
            f"{label} conflicted resolution changed non-conflicted path "
            f"{unexpected_paths[0]}"
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--head", default=os.environ.get("PR_HEAD_SHA", ""))
    parser.add_argument("--base", default=os.environ.get("PR_BASE_SHA", ""))
    parser.add_argument(
        "--head-branch",
        default=os.environ.get("PR_HEAD_BRANCH", ""),
    )
    args = parser.parse_args(argv)
    try:
        evidence = validate_topology(
            args.repo.resolve(), args.head, args.base, args.head_branch
        )
    except (OSError, SyncError, TopologyError) as error:
        print(
            _bounded_diagnostic(f"Synchronization topology failed: {error}"),
            file=sys.stderr,
        )
        return 1
    if evidence is None:
        print(
            "Synchronization topology not applicable: pull request is not a "
            "Synchronization branch"
        )
        return 0
    catch_up = evidence.catch_up_merge or "none"
    print(
        "Synchronization topology passed: "
        f"real head={evidence.head_sha} real base={evidence.base_sha} "
        f"baseline={evidence.baseline_reconciliation} catch-up={catch_up}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
