#!/usr/bin/env python3
"""Aggregate parallel Shadow CodeQL language results into one manifest."""

from dataclasses import dataclass
import argparse
import hashlib
import json
import math
from pathlib import Path

from validation_contracts import ContractError
from validation_contracts import OUTCOMES
from validation_contracts import SHA1_PATTERN
from validation_contracts import parse_plan
from validation_contracts import serialize_manifest
from validation_contracts import EvidenceManifest
from validation_contracts import canonical_json
from validation_plan import SUPPORTED_CODEQL_LANGUAGES
from validation_result import manifest_for_requirement


@dataclass(frozen=True)
class CodeqlLanguageResult:
    language: str
    candidate_sha: str
    outcome: str
    duration_seconds: float


def _language_result(payload: object, path: Path) -> CodeqlLanguageResult:
    if not isinstance(payload, dict) or set(payload) != {
        "language",
        "candidateSha",
        "outcome",
        "durationSeconds",
    }:
        raise ContractError(f"CodeQL result {path} has invalid fields")
    language = payload["language"]
    candidate_sha = payload["candidateSha"]
    outcome = payload["outcome"]
    duration = payload["durationSeconds"]
    if not isinstance(language, str) or language not in SUPPORTED_CODEQL_LANGUAGES:
        raise ContractError(f"CodeQL result {path} has an unsupported language")
    if not isinstance(candidate_sha, str) or SHA1_PATTERN.fullmatch(candidate_sha) is None:
        raise ContractError(f"CodeQL result {path} has a malformed candidate SHA")
    if not isinstance(outcome, str) or outcome not in OUTCOMES:
        raise ContractError(f"CodeQL result {path} has an unsupported outcome")
    if (
        isinstance(duration, bool)
        or not isinstance(duration, (int, float))
        or not math.isfinite(duration)
        or duration < 0
    ):
        raise ContractError(f"CodeQL result {path} has an invalid duration")
    return CodeqlLanguageResult(language, candidate_sha, outcome, float(duration))


def _validate_result(result: CodeqlLanguageResult) -> None:
    if result.language not in SUPPORTED_CODEQL_LANGUAGES:
        raise ContractError(f"CodeQL result has an unsupported language: {result.language}")
    if SHA1_PATTERN.fullmatch(result.candidate_sha) is None:
        raise ContractError("CodeQL result has a malformed candidate SHA")
    if result.outcome not in OUTCOMES:
        raise ContractError("CodeQL result has an unsupported outcome")
    if not math.isfinite(result.duration_seconds) or result.duration_seconds < 0:
        raise ContractError("CodeQL result has an invalid duration")


def aggregate_codeql(
    plan,
    results: tuple[CodeqlLanguageResult, ...],
    *,
    producer: str = "codeql-shadow-aggregator",
    created_at: int = 0,
) -> EvidenceManifest:
    requirement = next(
        item for item in plan.requirements if item.family == "codeql-advanced"
    )
    by_language: dict[str, CodeqlLanguageResult] = {}
    errors: list[str] = []
    for result in results:
        _validate_result(result)
        if result.language in by_language:
            errors.append(f"duplicate CodeQL language result: {result.language}")
        by_language[result.language] = result
    selected = set(plan.codeql_languages) if requirement.selected else set()
    expected = set(SUPPORTED_CODEQL_LANGUAGES)
    if not selected <= expected:
        errors.append("Validation plan selected an unsupported CodeQL language")
    missing = selected - set(by_language)
    errors.extend(f"missing CodeQL language result: {language}" for language in sorted(missing))
    stale = [
        result.language
        for result in results
        if result.candidate_sha != plan.candidate.candidate_sha
    ]
    errors.extend(f"stale CodeQL language result: {language}" for language in sorted(stale))
    if not requirement.selected:
        outcome = "not-required"
        reason = requirement.reason
    elif errors:
        outcome = "indeterminate"
        reason = "; ".join(errors)
    else:
        selected_results = tuple(by_language[language] for language in sorted(selected))
        outcomes = {result.outcome for result in selected_results}
        if "infrastructure-failure" in outcomes:
            outcome = "infrastructure-failure"
        elif "indeterminate" in outcomes:
            outcome = "indeterminate"
        elif "product-failure" in outcomes:
            outcome = "product-failure"
        elif "stale" in outcomes:
            outcome = "stale"
        elif outcomes == {"passed"}:
            outcome = "passed"
        else:
            outcome = "indeterminate"
        reason = "parallel CodeQL languages: " + ", ".join(
            f"{result.language}={result.outcome}" for result in selected_results
        )
    selected_results = tuple(
        by_language[language] for language in sorted(selected) if language in by_language
    )
    artifact_digests = tuple(
        (
            f"codeql:{result.language}",
            hashlib.sha256(
                canonical_json(
                    {
                        "language": result.language,
                        "candidateSha": result.candidate_sha,
                        "outcome": result.outcome,
                        "durationSeconds": result.duration_seconds,
                    }
                ).encode()
            ).hexdigest(),
        )
        for result in selected_results
    )
    duration = max((result.duration_seconds for result in results), default=0)
    return manifest_for_requirement(
        plan,
        requirement,
        outcome=outcome,
        producer=producer,
        reason=reason,
        duration_seconds=duration,
        critical_path_seconds=duration,
        artifact_digests=artifact_digests,
        created_at=created_at,
    )


def load_results(directory: Path) -> tuple[CodeqlLanguageResult, ...]:
    results = []
    for path in sorted(directory.glob("*.json")):
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise ContractError(f"cannot read CodeQL result {path}: {error}") from error
        results.append(_language_result(payload, path))
    return tuple(results)


def run(args: argparse.Namespace) -> int:
    plan = parse_plan(args.plan.read_text(encoding="utf-8"))
    manifest = aggregate_codeql(
        plan,
        load_results(args.result_dir),
        created_at=args.now,
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(serialize_manifest(manifest), encoding="utf-8")
    return 0 if manifest.outcome in {"passed", "not-required"} else 1


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--plan", type=Path, required=True)
    command.add_argument("--result-dir", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    command.add_argument("--now", type=int, default=0)
    return command


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        return run(args)
    except (ContractError, OSError, ValueError) as error:
        print(f"CodeQL aggregation failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
