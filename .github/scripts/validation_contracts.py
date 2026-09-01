#!/usr/bin/env python3
"""Bounded versioned contracts for fork-owned validation planning."""

from dataclasses import dataclass
import hashlib
import json
import math
import re
from typing import Any


SCHEMA_VERSION = 1
VALIDATION_IMPLEMENTATION = "electivus-validation-v1"
MAX_ITEMS = 2_000
MAX_TEXT_BYTES = 4_096
MAX_JSON_INTEGER = 2**63 - 1

SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")
CANDIDATE_KINDS = frozenset(
    {"pull-request", "integrated", "release", "surveillance", "synchronization"}
)
PROFILES = frozenset({"ordinary", "certification-required"})


class ContractError(ValueError):
    """Raised when an untrusted validation object violates its contract."""


def _text(value: object, name: str, *, allow_empty: bool = False) -> str:
    if not isinstance(value, str):
        raise ContractError(f"{name} must be a string")
    if not allow_empty and not value:
        raise ContractError(f"{name} must not be empty")
    try:
        encoded = value.encode()
    except UnicodeEncodeError as error:
        raise ContractError(f"{name} must be valid UTF-8") from error
    if len(encoded) > MAX_TEXT_BYTES:
        raise ContractError(f"{name} exceeds its byte budget")
    if any(
        (ord(character) < 32 and character != "\t") or 0x7F <= ord(character) <= 0x9F
        for character in value
    ):
        raise ContractError(f"{name} contains a control character")
    return value


def _sha(value: object, name: str) -> str:
    value = _text(value, name)
    if SHA_PATTERN.fullmatch(value) is None:
        raise ContractError(f"{name} must be a lowercase 40-character SHA")
    return value


def _sha256(value: object, name: str) -> str:
    value = _text(value, name)
    if SHA256_PATTERN.fullmatch(value) is None:
        raise ContractError(f"{name} must be a lowercase 64-character SHA-256")
    return value


def _integer(
    value: object,
    name: str,
    *,
    minimum: int = 0,
    maximum: int | None = None,
) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ContractError(f"{name} must be an integer of at least {minimum}")
    if maximum is not None and value > maximum:
        raise ContractError(f"{name} exceeds its bounded range")
    return value


def _array(value: object, name: str) -> list[Any] | tuple[Any, ...]:
    if not isinstance(value, (list, tuple)):
        raise ContractError(f"{name} must be an array")
    if len(value) > MAX_ITEMS:
        raise ContractError(f"{name} exceeds its item budget")
    return value


def _strings(value: object, name: str) -> tuple[str, ...]:
    result = tuple(_text(item, name) for item in _array(value, name))
    if len(set(result)) != len(result):
        raise ContractError(f"{name} must not contain duplicates")
    return result


def _pairs(value: object, name: str) -> tuple[tuple[str, str], ...]:
    result = []
    for pair in _array(value, name):
        if not isinstance(pair, (list, tuple)) or len(pair) != 2:
            raise ContractError(f"{name} must contain key/value pairs")
        result.append(
            (
                _text(pair[0], f"{name}.key"),
                _text(pair[1], f"{name}.value", allow_empty=True),
            )
        )
    if len({key for key, _ in result}) != len(result):
        raise ContractError(f"{name} must not contain duplicate keys")
    return tuple(result)


def _object(value: object, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ContractError(f"{name} must be an object")
    return value


def _keys(value: dict[str, Any], expected: set[str], name: str) -> None:
    if any(not isinstance(key, str) for key in value):
        raise ContractError(f"{name} has invalid fields (keys must be strings)")
    actual = set(value)
    if actual != expected:
        missing = ",".join(sorted(expected - actual))
        unexpected = ",".join(sorted(actual - expected))
        raise ContractError(
            f"{name} has invalid fields (missing={missing}; unexpected={unexpected})"
        )


MAX_JSON_INTEGER = 2**63 - 1


# Public codec primitives keep consumers independent from the implementation helpers above.
# They provide bounded text/digest/object checks and strict JSON hooks/codecs.
validate_text = _text
validate_sha256 = _sha256
require_object = _object
require_keys = _keys


def reject_json_duplicate(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ContractError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def reject_json_constant(value: str) -> Any:
    raise ContractError(f"invalid JSON constant: {value}")


def parse_json_integer(value: str) -> int:
    parsed = int(value)
    if not -MAX_JSON_INTEGER <= parsed <= MAX_JSON_INTEGER:
        raise ContractError("JSON integer exceeds its bounded range")
    return parsed


def validate_non_negative_number(
    value: object, name: str, *, maximum: int | float
) -> int | float:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or (isinstance(value, float) and not math.isfinite(value))
        or value < 0
        or value > maximum
    ):
        raise ContractError(f"{name} is out of range")
    return value


def decode_json_input(value: object, *, maximum_bytes: int, label: str) -> str:
    if isinstance(value, bytes):
        if len(value) > maximum_bytes:
            raise ContractError(f"{label} exceeds its input byte budget")
        try:
            value = value.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ContractError(f"{label} JSON must be valid UTF-8") from error
    if not isinstance(value, str):
        raise ContractError(f"{label} JSON must be text or UTF-8 bytes")
    try:
        size = len(value.encode("utf-8"))
    except UnicodeEncodeError as error:
        raise ContractError(f"{label} JSON must be valid UTF-8") from error
    if size > maximum_bytes:
        raise ContractError(f"{label} exceeds its input byte budget")
    return value


def serialize_json(payload: dict[str, object], *, maximum_bytes: int, label: str) -> str:
    try:
        text = (
            json.dumps(
                payload,
                ensure_ascii=False,
                allow_nan=False,
                sort_keys=True,
                indent=2,
            )
            + "\n"
        )
    except (TypeError, UnicodeEncodeError, ValueError) as error:
        raise ContractError(f"{label} cannot be canonically serialized") from error
    if len(text.encode("utf-8")) > maximum_bytes:
        raise ContractError(f"{label} exceeds its serialized byte budget")
    return text


def canonical_json(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


@dataclass(frozen=True)
class CandidateIdentity:
    event_name: str
    repository: str
    default_branch: str
    candidate_sha: str
    base_sha: str | None
    head_sha: str | None
    kind: str
    pull_request_number: int | None = None
    branch: str = ""


def candidate_to_dict(candidate: CandidateIdentity) -> dict[str, object]:
    return {
        "eventName": candidate.event_name,
        "repository": candidate.repository,
        "defaultBranch": candidate.default_branch,
        "candidateSha": candidate.candidate_sha,
        "baseSha": candidate.base_sha,
        "headSha": candidate.head_sha,
        "kind": candidate.kind,
        "pullRequestNumber": candidate.pull_request_number,
        "branch": candidate.branch,
    }


def validate_candidate(candidate: CandidateIdentity) -> None:
    if not isinstance(candidate, CandidateIdentity):
        raise ContractError("candidate has an invalid structure")
    _text(candidate.event_name, "candidate.eventName")
    _text(candidate.repository, "candidate.repository")
    _text(candidate.default_branch, "candidate.defaultBranch")
    _sha(candidate.candidate_sha, "candidate.candidateSha")
    if candidate.base_sha is not None:
        _sha(candidate.base_sha, "candidate.baseSha")
    if candidate.head_sha is not None:
        _sha(candidate.head_sha, "candidate.headSha")
    _text(candidate.kind, "candidate.kind")
    if candidate.kind not in CANDIDATE_KINDS:
        raise ContractError("candidate.kind is unsupported")
    _text(candidate.branch, "candidate.branch", allow_empty=True)
    if candidate.pull_request_number is not None:
        _integer(
            candidate.pull_request_number,
            "candidate.pullRequestNumber",
            minimum=1,
            maximum=MAX_JSON_INTEGER,
        )
    if candidate.kind == "pull-request" and (
        candidate.base_sha is None
        or candidate.head_sha is None
        or candidate.pull_request_number is None
    ):
        raise ContractError(
            "pull-request candidates require base SHA, head SHA, and pull request number"
        )
    if candidate.kind != "pull-request" and candidate.pull_request_number is not None:
        raise ContractError(
            "only pull-request candidates may carry a pull request number"
        )


def candidate_from_dict(value: object) -> CandidateIdentity:
    payload = _object(value, "candidate")
    _keys(
        payload,
        {
            "eventName",
            "repository",
            "defaultBranch",
            "candidateSha",
            "baseSha",
            "headSha",
            "kind",
            "pullRequestNumber",
            "branch",
        },
        "candidate",
    )
    pull_request_number = payload["pullRequestNumber"]
    if pull_request_number is not None:
        pull_request_number = _integer(
            pull_request_number,
            "candidate.pullRequestNumber",
            minimum=1,
            maximum=MAX_JSON_INTEGER,
        )
    candidate = CandidateIdentity(
        event_name=_text(payload["eventName"], "candidate.eventName"),
        repository=_text(payload["repository"], "candidate.repository"),
        default_branch=_text(payload["defaultBranch"], "candidate.defaultBranch"),
        candidate_sha=_sha(payload["candidateSha"], "candidate.candidateSha"),
        base_sha=None
        if payload["baseSha"] is None
        else _sha(payload["baseSha"], "candidate.baseSha"),
        head_sha=None
        if payload["headSha"] is None
        else _sha(payload["headSha"], "candidate.headSha"),
        kind=_text(payload["kind"], "candidate.kind"),
        pull_request_number=pull_request_number,
        branch=_text(payload["branch"], "candidate.branch", allow_empty=True),
    )
    validate_candidate(candidate)
    return candidate


@dataclass(frozen=True)
class ValidationFingerprint:
    source: tuple[tuple[str, str], ...]
    validation_implementation: str
    dependencies: tuple[tuple[str, str], ...]
    toolchains: tuple[tuple[str, str], ...]
    commands: tuple[str, ...]
    platforms: tuple[str, ...]
    profile: str
    parameters: tuple[tuple[str, str], ...]
    inputs: tuple[tuple[str, str], ...]

    @property
    def digest(self) -> str:
        return hashlib.sha256(
            canonical_json(_fingerprint_payload(self)).encode()
        ).hexdigest()


def _pair_lists(values: tuple[tuple[str, str], ...]) -> list[list[str]]:
    return [[key, value] for key, value in values]


def _fingerprint_payload(fingerprint: ValidationFingerprint) -> dict[str, object]:
    return {
        "source": _pair_lists(fingerprint.source),
        "validationImplementation": fingerprint.validation_implementation,
        "dependencies": _pair_lists(fingerprint.dependencies),
        "toolchains": _pair_lists(fingerprint.toolchains),
        "commands": list(fingerprint.commands),
        "platforms": list(fingerprint.platforms),
        "profile": fingerprint.profile,
        "parameters": _pair_lists(fingerprint.parameters),
        "inputs": _pair_lists(fingerprint.inputs),
    }


def fingerprint_to_dict(fingerprint: ValidationFingerprint) -> dict[str, object]:
    return {**_fingerprint_payload(fingerprint), "digest": fingerprint.digest}


def validate_fingerprint(fingerprint: ValidationFingerprint) -> None:
    if not isinstance(fingerprint, ValidationFingerprint):
        raise ContractError("fingerprint has an invalid structure")
    for name, values in (
        ("fingerprint.source", fingerprint.source),
        ("fingerprint.dependencies", fingerprint.dependencies),
        ("fingerprint.toolchains", fingerprint.toolchains),
        ("fingerprint.parameters", fingerprint.parameters),
        ("fingerprint.inputs", fingerprint.inputs),
    ):
        _pairs(values, name)
    _text(
        fingerprint.validation_implementation,
        "fingerprint.validationImplementation",
    )
    if fingerprint.validation_implementation != VALIDATION_IMPLEMENTATION:
        raise ContractError("fingerprint.validationImplementation is unsupported")
    _strings(fingerprint.commands, "fingerprint.commands")
    _strings(fingerprint.platforms, "fingerprint.platforms")
    _text(fingerprint.profile, "fingerprint.profile")
    if fingerprint.profile not in PROFILES:
        raise ContractError("fingerprint.profile is unsupported")


def fingerprint_from_dict(value: object) -> ValidationFingerprint:
    payload = _object(value, "fingerprint")
    _keys(
        payload,
        {
            "source",
            "validationImplementation",
            "dependencies",
            "toolchains",
            "commands",
            "platforms",
            "profile",
            "parameters",
            "inputs",
            "digest",
        },
        "fingerprint",
    )
    fingerprint = ValidationFingerprint(
        source=_pairs(payload["source"], "fingerprint.source"),
        validation_implementation=_text(
            payload["validationImplementation"],
            "fingerprint.validationImplementation",
        ),
        dependencies=_pairs(payload["dependencies"], "fingerprint.dependencies"),
        toolchains=_pairs(payload["toolchains"], "fingerprint.toolchains"),
        commands=_strings(payload["commands"], "fingerprint.commands"),
        platforms=_strings(payload["platforms"], "fingerprint.platforms"),
        profile=_text(payload["profile"], "fingerprint.profile"),
        parameters=_pairs(payload["parameters"], "fingerprint.parameters"),
        inputs=_pairs(payload["inputs"], "fingerprint.inputs"),
    )
    validate_fingerprint(fingerprint)
    if _sha256(payload["digest"], "fingerprint.digest") != fingerprint.digest:
        raise ContractError("fingerprint digest does not match its fields")
    return fingerprint
