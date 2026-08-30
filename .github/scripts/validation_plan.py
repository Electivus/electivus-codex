#!/usr/bin/env python3
"""Build the single additive Validation plan for one immutable candidate."""

from dataclasses import dataclass
import fnmatch
import hashlib
from pathlib import PurePosixPath
from typing import Mapping

from validation_contracts import CandidateIdentity
from validation_contracts import ContractError
from validation_contracts import EvidenceRequirement
from validation_contracts import SCHEMA_VERSION
from validation_contracts import VALIDATION_IMPLEMENTATION
from validation_contracts import ValidationFingerprint
from validation_contracts import ValidationPlan
from validation_contracts import canonical_json
from validation_contracts import validate_candidate


SURFACES = (
    "repository-documentation",
    "rust",
    "api-protocol-sdk",
    "runtime-state-postgresql",
    "execution-sandbox-v8",
    "platform-build",
    "package-release",
    "validation-architecture",
    "upstream-synchronization",
)
RISK_MODIFIERS = (
    "security",
    "breaking",
    "migration",
    "publication",
    "validation-authority",
    "synchronization",
    "unknown",
)
EVIDENCE_FAMILIES = (
    "repository-hygiene",
    "rust-fast",
    "linux-x64-bazel",
    "api-protocol-sdk",
    "postgresql",
    "v8",
    "windows-x64",
    "codeql-advanced",
    "code-quality",
    "linux-x64-cargo",
    "linux-arm64",
    "linux-musl",
    "release-packaging",
    "synchronization-topology",
)
SUPPORTED_CODEQL_LANGUAGES = (
    "rust",
    "python",
    "javascript-typescript",
    "go",
    "java-kotlin",
    "c-cpp",
    "csharp",
    "ruby",
    "swift",
)
REPOSITORY_CODEQL_LANGUAGES = ("rust", "python", "javascript-typescript")

SURFACE_PATTERNS: dict[str, tuple[str, ...]] = {
    "repository-documentation": (
        "README.md",
        "CODE_OF_CONDUCT.md",
        "CONTRIBUTING.md",
        "SECURITY.md",
        "docs/**",
        "*.md",
        ".github/CODEOWNERS",
        ".github/ISSUE_TEMPLATE/**",
        ".github/PULL_REQUEST_TEMPLATE/**",
        ".github/pull_request_template.md",
    ),
    "rust": (
        "codex-rs/**/*.rs",
        "codex-rs/**/Cargo.toml",
        "codex-rs/**/Cargo.lock",
        "rust-toolchain",
        "rust-toolchain.toml",
        "justfile",
        "MODULE.bazel",
        "MODULE.bazel.lock",
    ),
    "api-protocol-sdk": (
        "codex-rs/app-server-protocol/**",
        "codex-rs/protocol/**",
        "codex-rs/core-api/**",
        "codex-rs/app-server/**",
        "codex-rs/app-server-client/**",
        "sdk/**",
        "codex-cli/**",
    ),
    "runtime-state-postgresql": (
        "codex-rs/state/**",
        "codex-rs/thread-store/**",
        "codex-rs/memories/**",
        "**/migrations/**",
        "**/*.sql",
        ".github/workflows/postgres-runtime-state-contracts.yml",
        ".github/scripts/postgres_contract_inventory.py",
    ),
    "execution-sandbox-v8": (
        "codex-rs/sandboxing/**",
        "codex-rs/linux-sandbox/**",
        "codex-rs/windows-sandbox-rs/**",
        "codex-rs/v8-poc/**",
        "third_party/v8/**",
        "patches/*v8*",
        "patches/*llvm*",
        "patches/*rules_cc*",
        ".github/actions/setup-rusty-v8/**",
        ".github/workflows/v8-canary.yml",
        ".github/scripts/*v8*",
    ),
    "platform-build": (
        ".bazelrc",
        "BUILD",
        "**/BUILD",
        "**/*.BUILD.bazel",
        "**/*.bzl",
        ".github/actions/**",
        ".github/workflows/**",
        "Dockerfile*",
        "**/Dockerfile*",
        "codex-rs/windows-*/**",
        "codex-rs/exec-server/**",
    ),
    "package-release": (
        "scripts/codex_package/**",
        "scripts/install/**",
        ".github/workflows/electivus-release.yml",
        ".github/workflows/rust-release*.yml",
        ".github/workflows/python-runtime-release.yml",
        ".github/workflows/python-sdk-release.yml",
        ".github/workflows/r2-release.yml",
        ".github/workflows/rusty-v8-release.yml",
        ".github/scripts/build-*.sh",
        ".github/scripts/publish_*.py",
        ".github/actions/*sign*/**",
    ),
    "validation-architecture": (
        ".github/workflows/validation*.yml",
        ".github/workflows/validation*.yaml",
        ".github/workflows/blocking-ci.yml",
        ".github/workflows/repo-checks.yml",
        ".github/workflows/CI-ARCHITECTURE-SPEC.md",
        ".github/scripts/validation*.py",
        ".github/scripts/check_*topology.py",
        ".github/ci-validation-inventory.json",
        ".github/quarantined-checks.toml",
        "docs/adr/*validation*.md",
        "CONTEXT.md",
        "CONTEXT-MAP.md",
        "AGENTS.md",
    ),
    "upstream-synchronization": (
        ".github/workflows/upstream-release-sync.yml",
        ".github/scripts/upstream_sync*.py",
        ".github/scripts/sync_upstream*.py",
        ".github/upstream-sync-manifests/**",
        "docs/adr/*sync*.md",
    ),
}

RISK_PATTERNS: dict[str, tuple[str, ...]] = {
    "security": (
        "SECURITY.md",
        ".github/workflows/**",
        ".github/actions/**",
        ".github/scripts/*security*",
        "codex-rs/secrets/**",
        "codex-rs/process-hardening/**",
        "codex-rs/workload-identity/**",
        "codex-rs/network-proxy/**",
        "codex-rs/sandboxing/**",
        "**/*auth*",
        "**/*credential*",
        "**/*permission*",
    ),
    "breaking": (
        "codex-rs/app-server-protocol/**",
        "codex-rs/protocol/**",
        "codex-rs/core-api/**",
        "sdk/**",
        "codex-cli/**",
    ),
    "migration": (
        "**/migrations/**",
        "**/*.sql",
        "codex-rs/state/**",
        "codex-rs/thread-store/**",
        "codex-rs/memories/**",
    ),
    "publication": (
        "scripts/install/**",
        "scripts/codex_package/**",
        ".github/workflows/electivus-release.yml",
        ".github/workflows/rust-release*.yml",
        ".github/workflows/python-runtime-release.yml",
        ".github/workflows/python-sdk-release.yml",
        ".github/workflows/r2-release.yml",
        ".github/workflows/rusty-v8-release.yml",
        ".github/scripts/publish_*.py",
        ".github/actions/*sign*/**",
    ),
    "validation-authority": (
        ".github/workflows/**",
        ".github/scripts/**",
        ".github/ci-validation-inventory.json",
        ".github/quarantined-checks.toml",
        "AGENTS.md",
        "CONTEXT*.md",
        "docs/adr/*validation*.md",
    ),
    "synchronization": (
        ".github/workflows/upstream-release-sync.yml",
        ".github/scripts/upstream_sync*.py",
        ".github/scripts/sync_upstream*.py",
        ".github/upstream-sync-manifests/**",
        "docs/adr/*sync*.md",
    ),
}

LANGUAGE_SUFFIXES: dict[str, tuple[str, ...]] = {
    "rust": (".rs",),
    "python": (".py",),
    "javascript-typescript": (".js", ".jsx", ".mjs", ".ts", ".tsx"),
    "go": (".go",),
    "java-kotlin": (".java", ".kt", ".kts"),
    "c-cpp": (".c", ".cc", ".cpp", ".cxx", ".h", ".hpp"),
    "csharp": (".cs",),
    "ruby": (".rb",),
    "swift": (".swift",),
}


@dataclass(frozen=True)
class Classification:
    surfaces: tuple[str, ...]
    risk_modifiers: tuple[str, ...]
    codeql_languages: tuple[str, ...]
    reasons: tuple[tuple[str, str], ...]
    unknown_paths: tuple[str, ...] = ()
    errors: tuple[str, ...] = ()


def _matches(path: str, pattern: str) -> bool:
    return fnmatch.fnmatchcase(path, pattern)


def normalize_changed_files(changed_files: object) -> tuple[str, ...]:
    if not isinstance(changed_files, (list, tuple, set, frozenset)):
        raise ContractError("changed files must be an array")
    if len(changed_files) > 2_000:
        raise ContractError("changed files exceed their item budget")
    normalized = set()
    for value in changed_files:
        if not isinstance(value, str) or not value or len(value) > 4_096:
            raise ContractError("changed files contain an invalid path")
        if "\x00" in value or "\n" in value or value.startswith("/"):
            raise ContractError("changed files contain an invalid path")
        path = PurePosixPath(value)
        if path.is_absolute() or ".." in path.parts:
            raise ContractError("changed files contain an invalid path")
        normalized.add(value)
    return tuple(sorted(normalized))


def _metadata_bool(metadata: Mapping[str, object], name: str) -> bool | None:
    value = metadata.get(name)
    if value is None:
        return None
    return value if isinstance(value, bool) else None


def _languages_for_paths(paths: tuple[str, ...]) -> tuple[str, ...]:
    languages = {
        language
        for language, suffixes in LANGUAGE_SUFFIXES.items()
        if any(path.endswith(suffix) for path in paths for suffix in suffixes)
    }
    return tuple(language for language in SUPPORTED_CODEQL_LANGUAGES if language in languages)


def classify_changed_files(
    changed_files: object,
    *,
    metadata: Mapping[str, object] | None = None,
    branch: str = "",
) -> Classification:
    metadata = metadata or {}
    errors: list[str] = []
    try:
        paths = normalize_changed_files(changed_files)
    except ContractError as error:
        paths = ()
        errors.append(str(error))

    surfaces = {
        surface
        for surface, patterns in SURFACE_PATTERNS.items()
        if any(_matches(path, pattern) for path in paths for pattern in patterns)
    }
    unknown_paths = tuple(
        path
        for path in paths
        if not any(
            _matches(path, pattern)
            for patterns in SURFACE_PATTERNS.values()
            for pattern in patterns
        )
    )
    if unknown_paths:
        surfaces.add("repository-documentation")
    if branch.startswith("automation/upstream-sync/"):
        surfaces.add("upstream-synchronization")
    if not paths:
        errors.append("changed-path comparison is empty or unavailable")

    risk_modifiers = {
        modifier
        for modifier, patterns in RISK_PATTERNS.items()
        if any(_matches(path, pattern) for path in paths for pattern in patterns)
    }
    if unknown_paths:
        risk_modifiers.add("unknown")

    for name in ("classification_uncertain", "comparison_failed", "unknown_policy_state"):
        if name not in metadata:
            continue
        value = _metadata_bool(metadata, name)
        if value is True:
            risk_modifiers.add("unknown")
            errors.append(f"{name} requires fail-safe broad validation")
        elif value is not False:
            risk_modifiers.add("unknown")
            errors.append(f"{name} has malformed metadata")
    comparison_status = metadata.get("comparison_status")
    if comparison_status not in (None, "ok"):
        risk_modifiers.add("unknown")
        errors.append("changed-path comparison status is not ok")

    if not surfaces:
        surfaces.add("repository-documentation")
        risk_modifiers.add("unknown")
    if not risk_modifiers and errors:
        risk_modifiers.add("unknown")

    if risk_modifiers & {"security", "validation-authority", "synchronization", "unknown"}:
        codeql_languages = REPOSITORY_CODEQL_LANGUAGES
    else:
        codeql_languages = _languages_for_paths(paths)
    reasons = tuple(
        sorted(
            [(surface, "matched changed paths") for surface in surfaces]
            + [(modifier, "matched risk policy") for modifier in risk_modifiers]
        )
    )
    return Classification(
        surfaces=tuple(surface for surface in SURFACES if surface in surfaces),
        risk_modifiers=tuple(modifier for modifier in RISK_MODIFIERS if modifier in risk_modifiers),
        codeql_languages=codeql_languages,
        reasons=reasons,
        unknown_paths=unknown_paths,
        errors=tuple(dict.fromkeys(errors)),
    )


def _selected(
    family: str,
    *,
    stage: str,
    reason: str,
    profile: str,
) -> EvidenceRequirement:
    if stage == "integrated":
        retention = "integrated-certification"
    elif stage == "preflight":
        retention = "intra-run"
    else:
        retention = (
            "certification-required-pull-request"
            if profile == "certification-required"
            else "ordinary-pull-request"
        )
    return EvidenceRequirement(family, stage, True, "required", reason, retention)


def _not_selected(family: str, reason: str, profile: str) -> EvidenceRequirement:
    retention = (
        "certification-required-pull-request"
        if profile == "certification-required"
        else "ordinary-pull-request"
    )
    return EvidenceRequirement(family, "not-required", False, "not-required", reason, retention)


def _fingerprint(
    candidate: CandidateIdentity,
    classification: Classification,
    profile: str,
    requirements: tuple[EvidenceRequirement, ...],
    metadata: Mapping[str, object],
    changed_files: tuple[str, ...],
) -> ValidationFingerprint:
    selected = tuple(item.family for item in requirements if item.selected)
    source = (
        ("candidateSha", candidate.candidate_sha),
        ("baseSha", candidate.base_sha or ""),
        ("headSha", candidate.head_sha or ""),
    )
    safe_dependency_values = {
        name: str(metadata[name])
        for name in (
            "lockfile_digest",
            "toolchain_digest",
            "manifest_digest",
            "sync_release_baseline",
            "sync_fork_baseline",
            "sync_predecessor",
            "sync_release_tag",
            "sync_selection_mode",
            "sync_preparation_mode",
        )
        if name in metadata and isinstance(metadata[name], str)
    }
    dependencies = tuple(
        [("schemaVersion", str(SCHEMA_VERSION)), ("selectedEvidence", ",".join(selected))]
        + sorted(safe_dependency_values.items())
    )
    platforms = []
    for family in selected:
        platforms.extend(
            {
                "linux-x64-bazel": ("linux-x64",),
                "linux-x64-cargo": ("linux-x64",),
                "linux-arm64": ("linux-arm64",),
                "linux-musl": ("linux-musl",),
                "windows-x64": ("windows-x64",),
            }.get(family, ())
        )
    commands = tuple(f"validation:{family}" for family in selected)
    parameters = (
        ("changeSurfaces", ",".join(classification.surfaces)),
        ("riskModifiers", ",".join(classification.risk_modifiers)),
        ("codeqlLanguages", ",".join(classification.codeql_languages)),
        ("policyErrors", "\n".join(classification.errors)),
    )
    inputs = (
        (
            "changedPathsSha256",
            hashlib.sha256(canonical_json(changed_files).encode()).hexdigest(),
        ),
        ("candidateKind", candidate.kind),
        ("branch", candidate.branch),
    )
    return ValidationFingerprint(
        source=source,
        validation_implementation=VALIDATION_IMPLEMENTATION,
        dependencies=dependencies,
        toolchains=(
            ("rust", str(metadata.get("rust_toolchain", "declared-by-repository"))),
            ("runner", str(metadata.get("runner_image", "standard-public-runner"))),
        ),
        commands=commands,
        platforms=tuple(dict.fromkeys(platforms)),
        profile=profile,
        parameters=parameters,
        inputs=inputs,
    )


def build_plan(
    candidate: CandidateIdentity,
    changed_files: object,
    *,
    metadata: Mapping[str, object] | None = None,
) -> ValidationPlan:
    metadata = metadata or {}
    validate_candidate(candidate)
    paths = normalize_changed_files(changed_files)
    classification = classify_changed_files(paths, metadata=metadata, branch=candidate.branch)
    profile = (
        "certification-required"
        if candidate.kind in {"integrated", "release", "synchronization"}
        or set(classification.risk_modifiers)
        & {"security", "validation-authority", "synchronization", "unknown"}
        else "ordinary"
    )
    selected: dict[str, tuple[str, str]] = {}
    selected["repository-hygiene"] = (
        "preflight",
        "every candidate receives bounded repository and policy checks",
    )
    code_surfaces = set(classification.surfaces) - {"repository-documentation"}
    if code_surfaces:
        selected["linux-x64-bazel"] = (
            "merge-gate",
            "Linux x64 Bazel is Essential product evidence",
        )
        selected["code-quality"] = (
            "merge-gate",
            "code-quality remains an independent automated gate",
        )
    if "rust" in classification.surfaces:
        selected["rust-fast"] = (
            "merge-gate",
            "Rust candidates receive fast formatter and dependency hygiene",
        )
    if "api-protocol-sdk" in classification.surfaces:
        selected["api-protocol-sdk"] = (
            "merge-gate",
            "API/protocol/SDK paths require their targeted contract family",
        )
    if "runtime-state-postgresql" in classification.surfaces:
        selected["postgresql"] = (
            "merge-gate",
            "Runtime State and PostgreSQL paths require contract evidence",
        )
    if "execution-sandbox-v8" in classification.surfaces:
        selected["v8"] = (
            "merge-gate",
            "execution and V8 paths require the selected V8 evidence family",
        )
    if "platform-build" in classification.surfaces:
        selected["windows-x64"] = (
            "merge-gate",
            "platform/build changes conservatively select Windows x64",
        )
    if "package-release" in classification.surfaces:
        selected["release-packaging"] = (
            "certification-required" if profile == "certification-required" else "merge-gate",
            "package and release changes require immutable packaging evidence",
        )
    if classification.codeql_languages:
        selected["codeql-advanced"] = (
            "codeql-shadow",
            "affected CodeQL languages run in parallel in Shadow validation",
        )

    if profile == "certification-required":
        for family, reason in (
            (
                "linux-x64-cargo",
                "Certification-required candidates receive deep Linux x64 evidence",
            ),
            ("linux-arm64", "Certification-required candidates protect ARM64 breadth"),
            ("linux-musl", "Certification-required candidates protect musl breadth"),
        ):
            selected[family] = ("certification-required", reason)
        if "windows-x64" not in selected:
            selected["windows-x64"] = (
                "certification-required",
                "conservative Certification-required platform evidence",
            )
        if "package-release" in classification.surfaces or "publication" in classification.risk_modifiers:
            selected["release-packaging"] = (
                "certification-required",
                "publication risk requires package boundary evidence",
            )
        if "upstream-synchronization" in classification.surfaces:
            selected["synchronization-topology"] = (
                "certification-required",
                "Synchronization PRs require frozen-baseline topology evidence",
            )

    if candidate.kind == "integrated":
        selected.update(
            {
                "linux-x64-bazel": (
                    "integrated",
                    "every Integrated change receives full Linux x64 Bazel evidence",
                ),
                "postgresql": (
                    "integrated",
                    "every Integrated change receives PostgreSQL Runtime State evidence",
                ),
                "v8": (
                    "integrated",
                    "every Integrated change receives V8 and sandbox evidence",
                ),
                "windows-x64": (
                    "integrated",
                    "every Integrated change receives Windows x64 evidence",
                ),
                "linux-x64-cargo": (
                    "integrated",
                    "every Integrated change receives full Linux x64 Cargo and nextest evidence",
                ),
                "linux-arm64": (
                    "integrated",
                    "every Integrated change receives Linux ARM64 evidence",
                ),
                "linux-musl": (
                    "integrated",
                    "every Integrated change receives Linux musl evidence",
                ),
            }
        )
        for family, (_, reason) in tuple(selected.items()):
            if family != "repository-hygiene":
                selected[family] = ("integrated", reason)

    requirements = tuple(
        _selected(family, stage=selected[family][0], reason=selected[family][1], profile=profile)
        if family in selected
        else _not_selected(
            family,
            "not selected by the additive Change surface and Risk modifier plan",
            profile,
        )
        for family in EVIDENCE_FAMILIES
    )
    fingerprint = _fingerprint(
        candidate, classification, profile, requirements, metadata, paths
    )
    plan = ValidationPlan(
        schema_version=SCHEMA_VERSION,
        validation_implementation=VALIDATION_IMPLEMENTATION,
        candidate=candidate,
        surfaces=classification.surfaces,
        risk_modifiers=classification.risk_modifiers,
        profile=profile,
        codeql_languages=classification.codeql_languages,
        requirements=requirements,
        fingerprint=fingerprint,
        policy_errors=classification.errors,
    )
    return plan


def plan_summary(plan: ValidationPlan) -> str:
    selected = ", ".join(item.family for item in plan.requirements if item.selected)
    return (
        f"profile={plan.profile}; surfaces={','.join(plan.surfaces)}; "
        f"risk={','.join(plan.risk_modifiers) or 'none'}; "
        f"selected={selected or 'none'}"
    )
