#!/usr/bin/env python3
"""The public repository-owned seam for planning and reporting validation."""

import argparse
from dataclasses import replace
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time

from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_contracts import SCHEMA_VERSION
from validation_contracts import VALIDATION_IMPLEMENTATION
from validation_contracts import ValidationPlan
from validation_contracts import canonical_json
from validation_contracts import fingerprint_to_dict
from validation_contracts import parse_plan
from validation_contracts import serialize_plan
from validation_reports import serialize_report
from validation_reports import render_report
from validation_contracts import SHA1_PATTERN
from validation_result import aggregate
from validation_result import load_manifests
from validation_result import manifest_for_requirement
from validation_result import write_manifest
from validation_plan import build_plan
from validation_plan import normalize_changed_files


PINNED_ACTION = re.compile(r"(?m)^\s*-?\s*uses:\s*(\S+)")
WORKFLOW_SUFFIXES = {".yml", ".yaml"}
FORBIDDEN_PATH_PATTERNS = (
    re.compile(r"(^|/)\.env(?:\.|$)"),
    re.compile(r"\.(?:pem|key|p12)$"),
    re.compile(r"(^|/)(?:id_rsa|credentials(?:\.json)?|secrets?)(?:$|\.)"),
)


def _sha(value: str | None, name: str, *, required: bool = True) -> str | None:
    if value is None or value == "":
        if required:
            raise ContractError(f"{name} is required")
        return None
    if SHA1_PATTERN.fullmatch(value) is None:
        raise ContractError(f"{name} must be a lowercase 40-character SHA")
    return value


def _candidate_from_args(args: argparse.Namespace) -> CandidateIdentity:
    candidate_sha = args.candidate or os.environ.get("GITHUB_SHA")
    if not candidate_sha:
        try:
            candidate_sha = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], text=True, cwd=args.root
            ).strip()
        except (OSError, subprocess.CalledProcessError) as error:
            raise ContractError(f"cannot resolve candidate SHA: {error}") from error
    event_name = args.event_name or os.environ.get("GITHUB_EVENT_NAME", "workflow_dispatch")
    base_sha = args.base or os.environ.get("BASE_SHA")
    head_sha = args.head or os.environ.get("HEAD_SHA")
    kind = args.kind
    if kind == "pull-request" and not base_sha:
        base_sha = os.environ.get("GITHUB_BASE_SHA")
    if kind == "pull-request" and not head_sha:
        head_sha = os.environ.get("GITHUB_HEAD_SHA")
    return CandidateIdentity(
        event_name=event_name,
        repository=args.repository or os.environ.get("GITHUB_REPOSITORY", "local/repository"),
        default_branch=args.default_branch
        or os.environ.get("GITHUB_BASE_REF")
        or "main",
        candidate_sha=candidate_sha,
        base_sha=_sha(base_sha, "base SHA", required=kind == "pull-request"),
        head_sha=_sha(head_sha, "head SHA", required=kind == "pull-request"),
        kind=kind,
        pull_request_number=args.pull_request,
        branch=args.branch or os.environ.get("GITHUB_HEAD_REF", ""),
    )


def _load_input(path: Path) -> tuple[CandidateIdentity, object, dict[str, object]]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ContractError(f"cannot read validation input: {error}") from error
    if not isinstance(payload, dict):
        raise ContractError("validation input must be an object")
    expected = {"candidate", "changedFiles", "metadata"}
    if set(payload) != expected:
        raise ContractError("validation input must contain candidate, changedFiles, and metadata")
    from validation_contracts import candidate_from_dict

    candidate = candidate_from_dict(payload["candidate"])
    metadata = payload["metadata"]
    if not isinstance(metadata, dict):
        raise ContractError("validation input metadata must be an object")
    return candidate, payload["changedFiles"], metadata


def _changed_files_from_git(candidate: CandidateIdentity, root: Path) -> tuple[object, dict[str, object]]:
    if candidate.base_sha is None or candidate.head_sha is None:
        return (), {"comparison_failed": True}
    try:
        output = subprocess.check_output(
            [
                "git",
                "diff",
                "--name-only",
                "--no-renames",
                f"{candidate.base_sha}...{candidate.head_sha}",
            ],
            cwd=root,
            stderr=subprocess.PIPE,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return (), {"comparison_failed": True}
    return output.splitlines(), {"comparison_status": "ok"}


def _repository_files(root: Path, changed_files: tuple[str, ...]) -> tuple[Path, ...]:
    candidates = []
    for path in changed_files:
        if not (
            path.startswith(".github/workflows/")
            or path.startswith(".github/actions/")
        ):
            continue
        absolute = root / path
        if absolute.suffix in WORKFLOW_SUFFIXES and absolute.is_file():
            candidates.append(absolute)
    return tuple(candidates)


def _workflow_policy_errors(root: Path, changed_files: tuple[str, ...]) -> tuple[str, ...]:
    errors: list[str] = []
    for path in changed_files:
        if any(pattern.search(path) for pattern in FORBIDDEN_PATH_PATTERNS):
            errors.append(f"forbidden repository content path: {path}")
    for path in _repository_files(root, changed_files):
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeError) as error:
            errors.append(f"cannot inspect workflow {path}: {error}")
            continue
        if ".github" in path.parts and "actions" in path.parts:
            required = ("name:", "runs:")
        else:
            required = ("name:", "on:", "jobs:")
        missing = [field for field in required if not re.search(rf"(?m)^{re.escape(field)}", text)]
        errors.extend(f"workflow {path} is missing {field}" for field in missing)
        for match in PINNED_ACTION.finditer(text):
            action = match.group(1)
            if action.startswith("./") or action.startswith("docker://"):
                continue
            if "@" not in action or SHA1_PATTERN.fullmatch(action.rsplit("@", 1)[1]) is None:
                errors.append(f"workflow {path} uses an unpinned action: {action}")
    return tuple(dict.fromkeys(errors))


def _write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _write_failure(output_dir: Path, errors: tuple[str, ...]) -> None:
    bounded_errors = tuple(error[:4_096] for error in errors[:32])
    payload = {
        "schemaVersion": SCHEMA_VERSION,
        "validationImplementation": VALIDATION_IMPLEMENTATION,
        "outcome": "indeterminate",
        "admissionAllowed": False,
        "errors": list(bounded_errors),
        "nextActions": ["repair the input and rerun Preflight"],
    }
    text = json.dumps(payload, indent=2, ensure_ascii=False) + "\n"
    _write_text(output_dir / "validation-failure.json", text)
    _write_text(
        output_dir / "validation-report.md",
        "# Validation report\n\n- Outcome: `indeterminate`\n- Admission allowed: `false`\n\n"
        + "\n".join(f"- {error}" for error in bounded_errors)
        + "\n",
    )


def _github_outputs(plan: ValidationPlan, report_path: Path) -> dict[str, str]:
    outputs = {
        "plan_path": str(plan_path_for_output(plan, report_path)),
        "report_path": str(report_path),
        "profile": plan.profile,
        "change_surfaces": canonical_json(plan.surfaces),
        "risk_modifiers": canonical_json(plan.risk_modifiers),
        "codeql_languages": canonical_json(plan.codeql_languages),
        "selected_evidence": canonical_json(
            tuple(item.family for item in plan.requirements if item.selected)
        ),
        "admission_allowed": str(False).lower(),
    }
    if any("\n" in value or "\r" in value or len(value) > 4_096 for value in outputs.values()):
        raise ContractError("GitHub outputs must be bounded single-line values")
    return outputs


def _enrich_metadata(
    root: Path, candidate: CandidateIdentity, changed_files: tuple[str, ...], metadata: dict[str, object]
) -> dict[str, object]:
    enriched = dict(metadata)
    manifest_paths = tuple(
        root / path
        for path in changed_files
        if path.startswith(".github/upstream-sync-manifests/")
    )
    if candidate.kind == "synchronization" and not manifest_paths:
        enriched["unknown_policy_state"] = True
    if manifest_paths:
        digests = []
        sync_metadata = []
        for path in manifest_paths:
            try:
                relative = str(path.relative_to(root))
                contents = path.read_bytes()
                digests.append((relative, hashlib.sha256(contents).hexdigest()))
                from upstream_sync_manifest import parse_manifest

                manifest = parse_manifest(contents.decode("utf-8"))
                sync_metadata.append(
                    {
                        "sync_release_baseline": manifest.release.commit,
                        "sync_fork_baseline": manifest.fork_base_sha,
                        "sync_predecessor": manifest.previous_release_commit or "",
                        "sync_release_tag": manifest.release.tag,
                        "sync_selection_mode": manifest.selection_mode,
                        "sync_preparation_mode": manifest.preparation_mode,
                    }
                )
            except (OSError, UnicodeError, ValueError) as error:
                enriched["unknown_policy_state"] = True
                try:
                    relative = str(path.relative_to(root))
                except ValueError:
                    relative = str(path)
                digests.append((relative, f"unreadable:{str(error)[:256]}"))
        enriched["manifest_digest"] = hashlib.sha256(
            canonical_json(tuple(digests)).encode()
        ).hexdigest()
        if len(sync_metadata) == 1 and len(manifest_paths) == 1:
            enriched.update(sync_metadata[0])
        else:
            enriched["unknown_policy_state"] = True
    return enriched


def plan_path_for_output(plan: ValidationPlan, report_path: Path) -> Path:
    return report_path.parent / "validation-plan.json"


def _append_github_outputs(outputs: dict[str, str]) -> None:
    output_path = os.environ.get("GITHUB_OUTPUT")
    if output_path:
        with Path(output_path).open("a", encoding="utf-8") as stream:
            stream.write("".join(f"{key}={value}\n" for key, value in outputs.items()))


def run(args: argparse.Namespace) -> int:
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    if args.now > 0:
        _write_text(output_dir / "validation-started-at", str(args.now) + "\n")
    try:
        if args.input:
            candidate, changed_files_input, metadata = _load_input(args.input)
        else:
            candidate = _candidate_from_args(args)
            if args.changed_file:
                changed_files_input = args.changed_file
                metadata = {"comparison_status": "ok"}
            else:
                changed_files_input, metadata = _changed_files_from_git(candidate, args.root)
        if args.plan_version != SCHEMA_VERSION:
            raise ContractError(f"unsupported Validation plan version: {args.plan_version}")
        changed_files = normalize_changed_files(changed_files_input)
        policy_errors = _workflow_policy_errors(args.root, changed_files)
        metadata = _enrich_metadata(args.root, candidate, changed_files, metadata)
        plan = build_plan(candidate, changed_files, metadata=metadata)
        all_policy_errors = tuple(dict.fromkeys((*plan.policy_errors, *policy_errors)))
        plan = replace(
            plan,
            policy_errors=all_policy_errors,
            fingerprint=replace(
                plan.fingerprint,
                parameters=tuple(
                    (
                        "policyErrors",
                        "\n".join(all_policy_errors),
                    )
                    if name == "policyErrors"
                    else (name, value)
                    for name, value in plan.fingerprint.parameters
                ),
            ),
        )
        # Re-serialize through the parser so the CLI itself exercises the same
        # strict contract consumed by later workflow jobs.
        plan = parse_plan(serialize_plan(plan))
        preflight_requirement = next(
            requirement
            for requirement in plan.requirements
            if requirement.family == "repository-hygiene"
        )
        preflight = manifest_for_requirement(
            plan,
            preflight_requirement,
            outcome="product-failure" if plan.policy_errors else "passed",
            producer="preflight",
            reason=(
                "; ".join(plan.policy_errors)
                if plan.policy_errors
                else "identity, plan schema, workflow policy, and repository hygiene passed"
            ),
            duration_seconds=time.monotonic() - started,
            critical_path_seconds=time.monotonic() - started,
            created_at=args.now,
        )
        evidence_dir = args.evidence_dir or output_dir / "evidence"
        if args.evidence_dir:
            manifests = load_manifests(args.evidence_dir)
        else:
            manifests = (preflight,)
        manifests = tuple(
            manifest for manifest in manifests if manifest.family != preflight.family
        ) + (preflight,)
        result = aggregate(
            plan,
            manifests,
            current_candidate=candidate,
            current_base_sha=candidate.base_sha,
            now=args.now or None,
            cache_fallback=args.cache_fallback,
        )
        plan_path = output_dir / "validation-plan.json"
        report_path = output_dir / "validation-report.json"
        _write_text(plan_path, serialize_plan(plan))
        _write_text(
            output_dir / "validation-fingerprint.json",
            json.dumps(fingerprint_to_dict(plan.fingerprint), indent=2, ensure_ascii=False) + "\n",
        )
        write_manifest(preflight, evidence_dir / "preflight.json")
        _write_text(report_path, serialize_report(result.report))
        _write_text(output_dir / "validation-report.md", render_report(result.report))
        outputs = _github_outputs(plan, report_path)
        outputs["plan_path"] = str(plan_path)
        outputs["admission_allowed"] = str(result.report.admission_allowed).lower()
        _append_github_outputs(outputs)
        summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary_path:
            with Path(summary_path).open("a", encoding="utf-8") as stream:
                stream.write(render_report(result.report))
        if args.preflight_only:
            return 0 if not plan.policy_errors else 1
        return 0 if result.report.admission_allowed else 1
    except (ContractError, OSError, ValueError) as error:
        _write_failure(output_dir, (str(error),))
        return 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--input", type=Path)
    command.add_argument("--output-dir", type=Path, default=Path("validation-output"))
    command.add_argument("--evidence-dir", type=Path)
    command.add_argument("--root", type=Path, default=Path.cwd())
    command.add_argument("--event-name")
    command.add_argument("--repository")
    command.add_argument("--default-branch")
    command.add_argument("--candidate")
    command.add_argument("--base")
    command.add_argument("--head")
    command.add_argument("--branch")
    command.add_argument("--pull-request", type=int)
    command.add_argument(
        "--kind",
        choices=("pull-request", "integrated", "release", "surveillance", "synchronization"),
        default="pull-request",
    )
    command.add_argument("--changed-file", action="append")
    command.add_argument("--plan-version", type=int, default=SCHEMA_VERSION)
    command.add_argument("--now", type=int, default=0)
    command.add_argument(
        "--preflight-only",
        action="store_true",
        help="emit the complete plan/report but fail only on Preflight policy errors",
    )
    command.add_argument(
        "--cache-fallback",
        choices=("not-applicable", "not-used", "disabled-reconstruction"),
        default="not-applicable",
    )
    return command


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    args.root = args.root.resolve()
    if args.input:
        args.input = args.input.resolve()
    if args.output_dir:
        args.output_dir = args.output_dir.resolve()
    if args.evidence_dir:
        args.evidence_dir = args.evidence_dir.resolve()
    return run(args)


if __name__ == "__main__":
    raise SystemExit(main())
