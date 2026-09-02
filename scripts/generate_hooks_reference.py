#!/usr/bin/env python3
"""Generate a detailed Markdown reference for every Codex hook event."""

import argparse
from dataclasses import dataclass, field
import html
import json
from pathlib import Path
import re
import sys
from typing import Any, Iterable


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_SCHEMA_DIR = REPO_ROOT / "codex-rs" / "hooks" / "schema" / "generated"
DEFAULT_OUTPUT_PATH = REPO_ROOT / "docs" / "hooks-reference.md"
DEFAULT_TITLE = "Codex hooks reference"
NO_DESCRIPTION = "_No description available in the generated schema._"
SCHEMA_FILE_RE = re.compile(
    r"^(?P<event>.+)\.command\.(?P<direction>input|output)\.schema\.json$"
)

JsonObject = dict[str, Any]
FieldPath = tuple[str, ...]


@dataclass(frozen=True)
class EventMetadata:
    name: str
    slug: str
    timing: str
    matcher: str
    scope: str
    plain_stdout: str
    exit_two: str
    aggregation: str
    source: str
    behavior: tuple[str, ...]


EVENTS = (
    EventMetadata(
        "SessionStart",
        "session-start",
        "Before the first model turn after startup, resume, clear, or compaction.",
        "Session source: `startup`, `resume`, `clear`, or `compact`.",
        "thread",
        "Injected as additional model context.",
        "Failure; no special exit-code-2 behavior.",
        "Any `continue:false` stops startup; contexts remain in declaration order.",
        "codex-rs/hooks/src/events/session_start.rs",
        (
            "`continue:false` stops a root session start and may carry `stopReason`.",
            "`hookSpecificOutput.additionalContext` and non-JSON stdout become contextual user fragments.",
            "A JSON-looking but invalid payload fails the hook; empty stdout is a successful no-op.",
        ),
    ),
    EventMetadata(
        "SessionEnd",
        "session-end",
        "During root-session teardown, after Codex attempts to flush the transcript.",
        "Fixed reason `other`.",
        "thread",
        "Ignored.",
        "Failure; no special exit-code-2 behavior.",
        "Every matched process is awaited; output cannot change teardown.",
        "codex-rs/hooks/src/events/session_end.rs",
        (
            "Runs only for root sessions; thread-spawn subagents use `SubagentStart`/`SubagentStop`.",
            "Has an input schema but intentionally no command output schema.",
            "Defaults to a 1-second timeout and is capped at 3 seconds to preserve shutdown headroom.",
        ),
    ),
    EventMetadata(
        "UserPromptSubmit",
        "user-prompt-submit",
        "Before a user input is recorded and submitted to the model.",
        "Ignored; every handler for the event runs.",
        "turn",
        "Injected as additional model context.",
        "Blocks submission when stderr contains a reason.",
        "Any stop/block stops submission; first reason wins; contexts preserve declaration order.",
        "codex-rs/hooks/src/events/user_prompt_submit.rs",
        (
            "Supports `continue:false`, `decision:block`, and `hookSpecificOutput.additionalContext`.",
            "A block decision requires a non-empty `reason`; context emitted with a valid block is retained.",
            "Plain stdout is context, while JSON-looking invalid stdout fails the hook.",
        ),
    ),
    EventMetadata(
        "PreToolUse",
        "pre-tool-use",
        "Immediately before a selected tool handler executes.",
        "Canonical tool name plus compatibility aliases.",
        "turn",
        "Ignored unless it looks like invalid JSON.",
        "Blocks the tool call when stderr contains a reason.",
        "Any deny blocks; first block reason wins; last-completing valid input rewrite wins.",
        "codex-rs/hooks/src/events/pre_tool_use.rs",
        (
            "Supports legacy `decision:block` and hook-specific `permissionDecision:deny`.",
            "`permissionDecision:allow` is accepted only with `updatedInput`; `ask` is unsupported.",
            "A block discards every proposed input rewrite; additional context remains available.",
        ),
    ),
    EventMetadata(
        "PermissionRequest",
        "permission-request",
        "In the approval path before guardian review or user approval UI.",
        "Canonical tool name plus compatibility aliases.",
        "turn",
        "Ignored unless it looks like invalid JSON.",
        "Denies approval when stderr contains a reason.",
        "Any deny wins; otherwise the last applicable allow wins; no verdict defers to normal approval.",
        "codex-rs/hooks/src/events/permission_request.rs",
        (
            "Returns an optional allow/deny verdict and never rewrites tool input.",
            "`updatedInput`, `updatedPermissions`, and `interrupt:true` are reserved but unsupported.",
            "The approval path budgets the maximum matching hook timeout because handlers run concurrently.",
        ),
    ),
    EventMetadata(
        "PostToolUse",
        "post-tool-use",
        "After a tool has produced a successful output.",
        "Canonical tool name plus compatibility aliases.",
        "turn",
        "Ignored unless it looks like invalid JSON.",
        "Blocks follow-up processing and feeds stderr back to the model.",
        "Any explicit block wins; feedback and additional context preserve declaration order.",
        "codex-rs/hooks/src/events/post_tool_use.rs",
        (
            "`continue:false` marks the hook stopped and substitutes feedback for the model-visible tool output; the tool has already run.",
            "`decision:block` rejects the completed tool result back to the model but cannot undo its side effects.",
            "A block requires a non-empty reason; `updatedMCPToolOutput` is unsupported.",
            "Stop/block feedback is joined with blank lines and may be spilled before model visibility.",
        ),
    ),
    EventMetadata(
        "PreCompact",
        "pre-compact",
        "Immediately before manual or automatic context compaction.",
        "Compaction trigger: `manual` or `auto`.",
        "turn",
        "Ignored unless it looks like invalid JSON.",
        "Failure; stderr is used as the error message.",
        "Any `continue:false` prevents compaction; first stop reason wins.",
        "codex-rs/hooks/src/events/compact.rs",
        (
            "Only the universal output fields are declared.",
            "`continue:false` stops before compaction; a missing reason gets a synthesized status message.",
            "Block decisions and plain-text context are not part of this event contract.",
        ),
    ),
    EventMetadata(
        "PostCompact",
        "post-compact",
        "After manual or automatic context compaction completes.",
        "Compaction trigger: `manual` or `auto`.",
        "turn",
        "Ignored unless it looks like invalid JSON.",
        "Failure; stderr is used as the error message.",
        "Any `continue:false` stops the post-compaction flow; first stop reason wins.",
        "codex-rs/hooks/src/events/compact.rs",
        (
            "Only the universal output fields are declared.",
            "Compaction has already happened when this event runs.",
            "Block decisions and plain-text context are not part of this event contract.",
        ),
    ),
    EventMetadata(
        "SubagentStart",
        "subagent-start",
        "When a user-visible thread-spawn subagent starts.",
        "Subagent `agent_type`.",
        "thread",
        "Injected as additional model context.",
        "Failure; no special exit-code-2 behavior.",
        "Contexts preserve declaration order; stop decisions are ignored.",
        "codex-rs/hooks/src/events/session_start.rs",
        (
            "Shares start-hook output handling but is context-injection-only.",
            "`continue:false` is parsed but does not stop a subagent start.",
            "Internal or synthetic subagents do not expose user-configured lifecycle hooks.",
        ),
    ),
    EventMetadata(
        "SubagentStop",
        "subagent-stop",
        "When a user-visible thread-spawn subagent reaches turn completion.",
        "Subagent `agent_type`.",
        "turn",
        "Invalid; successful output must be JSON or empty.",
        "Blocks completion and uses stderr as a continuation prompt.",
        "Any `continue:false` overrides blocks; otherwise all blocking prompts are preserved.",
        "codex-rs/hooks/src/events/stop.rs",
        (
            "Includes parent and subagent transcript paths when available.",
            "`decision:block` produces a hook-attributed continuation fragment for the subagent.",
            "Blocking fragments are model-budgeted and may spill to the OS temporary directory.",
        ),
    ),
    EventMetadata(
        "Stop",
        "stop",
        "After a root agent turn reaches completion, before completion is finalized.",
        "Ignored; every handler for the event runs.",
        "turn",
        "Invalid; successful output must be JSON or empty.",
        "Blocks completion and uses stderr as a continuation prompt.",
        "Any `continue:false` overrides blocks; otherwise blocking reasons are joined in declaration order.",
        "codex-rs/hooks/src/events/stop.rs",
        (
            "`decision:block` asks the agent to continue with the supplied reason.",
            "`stop_hook_active` lets a hook detect a continuation caused by an earlier Stop hook.",
            "Blocking prompts retain per-hook attribution and may spill before model injection.",
        ),
    ),
    EventMetadata(
        "Interrupt",
        "interrupt",
        "Right before an explicitly interrupted turn is aborted.",
        "Ignored; every handler for the event runs.",
        "turn",
        "Invalid; successful output must be JSON or empty.",
        "Failure; no special exit-code-2 behavior.",
        "Every matched process is awaited; output cannot prevent interruption.",
        "codex-rs/hooks/src/events/interrupt.rs",
        (
            "A non-empty successful output may contain only the optional `systemMessage` warning.",
            "Hook failures are reported through lifecycle events but do not cancel the interruption.",
            "Command hooks default to a 1-second timeout and are capped at 3 seconds; executor-scoped hooks run asynchronously.",
        ),
    ),
)


@dataclass
class HookSchemas:
    metadata: EventMetadata
    input_path: Path | None = None
    input_schema: JsonObject | None = None
    output_path: Path | None = None
    output_schema: JsonObject | None = None


@dataclass
class SchemaField:
    path: FieldPath
    required: bool = False
    schemas: list[JsonObject] = field(default_factory=list)

    def add(self, schema: JsonObject, required: bool) -> None:
        self.required |= required
        if not any(existing is schema for existing in self.schemas):
            self.schemas.append(schema)


@dataclass(frozen=True)
class RuntimeFacts:
    feature_key: str
    feature_stage: str
    feature_default_enabled: bool
    default_timeout_sec: int
    session_end_default_timeout_sec: int
    session_end_max_timeout_sec: int
    additional_context_token_limit: int


def _read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _require_match(
    pattern: str, text: str, source: Path, flags: int = 0
) -> re.Match[str]:
    match = re.search(pattern, text, flags)
    if match is None:
        raise ValueError(f"could not extract {pattern!r} from {source}")
    return match


def load_runtime_facts(repo_root: Path = REPO_ROOT) -> RuntimeFacts:
    feature_path = repo_root / "codex-rs" / "features" / "src" / "lib.rs"
    discovery_path = (
        repo_root / "codex-rs" / "hooks" / "src" / "engine" / "discovery.rs"
    )
    session_end_path = (
        repo_root / "codex-rs" / "hooks" / "src" / "events" / "session_end.rs"
    )
    spill_path = repo_root / "codex-rs" / "hooks" / "src" / "output_spill.rs"

    feature = _require_match(
        r"id:\s*Feature::CodexHooks,\s*key:\s*\"([^\"]+)\",\s*"
        r"stage:\s*Stage::(\w+),\s*default_enabled:\s*(true|false)",
        _read_text(feature_path),
        feature_path,
        re.DOTALL,
    )
    discovery = _read_text(discovery_path)
    default_timeout = _require_match(
        r"timeout_sec\.unwrap_or\((\d+)\)\.max\(1\)",
        discovery,
        discovery_path,
    )
    session_end = _read_text(session_end_path)
    session_default = _require_match(
        r"SESSION_END_DEFAULT_TIMEOUT_SEC:\s*u64\s*=\s*(\d+)",
        session_end,
        session_end_path,
    )
    session_max = _require_match(
        r"SESSION_END_MAX_TIMEOUT_SEC:\s*u64\s*=\s*(\d+)",
        session_end,
        session_end_path,
    )
    spill = _require_match(
        r"DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT:\s*usize\s*=\s*([\d_]+)",
        _read_text(spill_path),
        spill_path,
    )
    return RuntimeFacts(
        feature_key=feature.group(1),
        feature_stage=feature.group(2),
        feature_default_enabled=feature.group(3) == "true",
        default_timeout_sec=int(default_timeout.group(1)),
        session_end_default_timeout_sec=int(session_default.group(1)),
        session_end_max_timeout_sec=int(session_max.group(1)),
        additional_context_token_limit=int(spill.group(1).replace("_", "")),
    )


def registered_hook_event_names(repo_root: Path = REPO_ROOT) -> tuple[str, ...]:
    path = repo_root / "codex-rs" / "config" / "src" / "hook_config.rs"
    text = _read_text(path)
    block = _require_match(
        r"pub struct HookEventsToml\s*\{(?P<body>.*?)\n\}", text, path, re.DOTALL
    ).group("body")
    names = re.findall(
        r"#\[serde\(rename\s*=\s*\"([^\"]+)\",\s*default\)\]\s*"
        r"pub\s+\w+:\s+Vec<MatcherGroup>",
        block,
    )
    if not names:
        raise ValueError(f"no hook event declarations found in {path}")
    return tuple(names)


def configured_handler_types(repo_root: Path = REPO_ROOT) -> tuple[str, ...]:
    path = repo_root / "codex-rs" / "config" / "src" / "hook_config.rs"
    text = _read_text(path)
    block = _require_match(
        r"pub enum HookHandlerConfig\s*\{(?P<body>.*?)\n\}", text, path, re.DOTALL
    ).group("body")
    names = re.findall(r"#\[serde\(rename\s*=\s*\"([^\"]+)\"\)\]", block)
    if not names:
        raise ValueError(f"no hook handler types found in {path}")
    return tuple(names)


def load_hook_schemas(schema_dir: Path = DEFAULT_SCHEMA_DIR) -> list[HookSchemas]:
    registered = set(registered_hook_event_names())
    metadata_names = {event.name for event in EVENTS}
    if missing_metadata := registered - metadata_names:
        raise ValueError(
            "registered events without metadata: " + ", ".join(sorted(missing_metadata))
        )
    active_events = tuple(event for event in EVENTS if event.name in registered)
    by_slug = {event.slug: HookSchemas(event) for event in active_events}
    seen_files = 0
    for path in sorted(schema_dir.glob("*.schema.json")):
        match = SCHEMA_FILE_RE.match(path.name)
        if match is None:
            raise ValueError(f"unexpected hook schema filename: {path}")
        slug = match.group("event")
        if slug not in by_slug:
            raise ValueError(f"schema has no event metadata: {path}")
        with path.open(encoding="utf-8") as schema_file:
            schema = json.load(schema_file)
        if not isinstance(schema, dict):
            raise ValueError(f"schema root must be a JSON object: {path}")
        schemas = by_slug[slug]
        direction = match.group("direction")
        if getattr(schemas, f"{direction}_path") is not None:
            raise ValueError(f"duplicate {direction} schema for {slug}")
        setattr(schemas, f"{direction}_path", path)
        setattr(schemas, f"{direction}_schema", schema)
        seen_files += 1

    if seen_files == 0:
        raise ValueError(f"no hook schemas found in {schema_dir}")
    missing_inputs = [
        schemas.metadata.name
        for schemas in by_slug.values()
        if schemas.input_schema is None
    ]
    if missing_inputs:
        raise ValueError(f"events without input schemas: {', '.join(missing_inputs)}")
    return [by_slug[event.slug] for event in active_events]


def _reference_target(root: JsonObject, reference: str) -> JsonObject | None:
    prefix = "#/definitions/"
    if not reference.startswith(prefix):
        return None
    target = root.get("definitions", {}).get(reference.removeprefix(prefix))
    return target if isinstance(target, dict) else None


def _schema_fragments(
    node: JsonObject,
    root: JsonObject,
    active_references: frozenset[str] = frozenset(),
) -> Iterable[JsonObject]:
    yield node
    reference = node.get("$ref")
    if isinstance(reference, str) and reference not in active_references:
        target = _reference_target(root, reference)
        if target is not None:
            yield from _schema_fragments(target, root, active_references | {reference})
    for combinator in ("allOf", "anyOf", "oneOf"):
        variants = node.get(combinator)
        if isinstance(variants, list):
            for variant in variants:
                if isinstance(variant, dict):
                    yield from _schema_fragments(variant, root, active_references)


def collect_schema_fields(root: JsonObject) -> list[SchemaField]:
    fields: dict[FieldPath, SchemaField] = {}
    visited: set[tuple[int, FieldPath]] = set()

    def walk_object(node: JsonObject, prefix: FieldPath) -> None:
        visit_key = (id(node), prefix)
        if visit_key in visited:
            return
        visited.add(visit_key)
        for fragment in _schema_fragments(node, root):
            required = fragment.get("required", [])
            required_names = set(required) if isinstance(required, list) else set()
            properties = fragment.get("properties", {})
            if not isinstance(properties, dict):
                continue
            for name, child in properties.items():
                if not isinstance(name, str) or not isinstance(child, dict):
                    continue
                path = (*prefix, name)
                fields.setdefault(path, SchemaField(path)).add(
                    child, name in required_names
                )
                for child_fragment in _schema_fragments(child, root):
                    if isinstance(child_fragment.get("properties"), dict):
                        walk_object(child_fragment, path)
                    items = child_fragment.get("items")
                    if isinstance(items, dict):
                        walk_object(items, (*path, "[]"))

    walk_object(root, ())
    return list(fields.values())


def _unique(values: Iterable[Any]) -> list[Any]:
    result: list[Any] = []
    for value in values:
        if value not in result:
            result.append(value)
    return result


def format_schema_type(schemas: list[JsonObject], root: JsonObject) -> str:
    types: list[str] = []
    for schema in schemas:
        for fragment in _schema_fragments(schema, root):
            declared = fragment.get("type")
            values = [declared] if isinstance(declared, str) else declared
            if isinstance(values, list):
                types.extend(value for value in values if isinstance(value, str))
            elif isinstance(fragment.get("properties"), dict):
                types.append("object")
    types = _unique(types)
    return " or ".join(types) if types else "any JSON value"


def format_schema_description(schemas: list[JsonObject], root: JsonObject) -> str:
    descriptions = _unique(
        description.strip()
        for schema in schemas
        for fragment in _schema_fragments(schema, root)
        if isinstance((description := fragment.get("description")), str)
        and description.strip()
    )
    return " ".join(descriptions) if descriptions else NO_DESCRIPTION


def format_schema_constraints(schemas: list[JsonObject], root: JsonObject) -> str:
    details: list[str] = []
    for schema in schemas:
        for fragment in _schema_fragments(schema, root):
            if "const" in fragment:
                details.append(
                    f"const `{json.dumps(fragment['const'], ensure_ascii=False)}`"
                )
            if isinstance(fragment.get("enum"), list):
                values = ", ".join(
                    f"`{json.dumps(value, ensure_ascii=False)}`"
                    for value in fragment["enum"]
                )
                details.append(f"enum: {values}")
            if "default" in fragment:
                details.append(
                    f"default `{json.dumps(fragment['default'], ensure_ascii=False)}`"
                )
            for key in (
                "minimum",
                "maximum",
                "minLength",
                "maxLength",
                "pattern",
                "format",
            ):
                if key in fragment:
                    details.append(
                        f"{key} `{json.dumps(fragment[key], ensure_ascii=False)}`"
                    )
            if fragment.get("additionalProperties") is False:
                details.append("additional properties forbidden")
    return "; ".join(_unique(details)) or "—"


def _escape_cell(value: str) -> str:
    return html.escape(value, quote=False).replace("|", "\\|").replace("\n", "<br>")


def _schema_table(schema: JsonObject) -> list[str]:
    lines = [
        "<!-- prettier-ignore -->",
        "| Field | Required | Type | Constraints / default | Description |",
        "| --- | --- | --- | --- | --- |",
    ]
    for entry in collect_schema_fields(schema):
        field_name = ".".join(entry.path).replace(".[]", "[]")
        lines.append(
            "| `{}` | {} | <code>{}</code> | {} | {} |".format(
                _escape_cell(field_name),
                "yes" if entry.required else "no",
                _escape_cell(format_schema_type(entry.schemas, schema)),
                _escape_cell(format_schema_constraints(entry.schemas, schema)),
                _escape_cell(format_schema_description(entry.schemas, schema)),
            )
        )
    return lines


def _raw_schema(path: Path, schema: JsonObject) -> list[str]:
    return [
        f"<details><summary>Raw JSON Schema: <code>{path.name}</code></summary>",
        "",
        "<!-- prettier-ignore -->",
        "```json",
        json.dumps(schema, ensure_ascii=False, indent=2, sort_keys=True),
        "```",
        "",
        "</details>",
    ]


def _display_path(path: Path) -> str:
    try:
        return path.relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return path.as_posix()


def _configuration_section(
    facts: RuntimeFacts,
    handler_types: tuple[str, ...],
    event_names: frozenset[str],
) -> list[str]:
    handlers = ", ".join(f"`{handler}`" for handler in handler_types)
    if "mcp_tool" in handler_types:
        handler_summary = (
            f"Configured handler variants are {handlers}. `command` and `mcp_tool` execute today; "
            "`prompt` and `agent` parse successfully but discovery skips them with warnings. Empty "
            "commands are also skipped. `commandWindows` overrides `command` only on Windows."
        )
        async_summary = (
            "| `async` | When true, schedules a command hook without waiting for its result or applying "
            "control effects. SessionEnd emits a warning and still runs synchronously. |"
        )
    else:
        handler_summary = (
            f"Configured handler variants are {handlers}. Only `command` executes today; `prompt` "
            "and `agent` parse successfully but discovery skips them with warnings. Empty commands "
            "are also skipped. `commandWindows` overrides `command` only on Windows."
        )
        async_summary = (
            "| `async` | Unsupported. Non-SessionEnd async hooks are skipped; SessionEnd emits a warning "
            "and still runs synchronously. |"
        )
    timeout_rules = (
        "stricter SessionEnd and Interrupt rules"
        if "Interrupt" in event_names
        else "a stricter SessionEnd rule"
    )
    timeout_summary = (
        f"| `timeout` | Seconds; defaults to {facts.default_timeout_sec}, is normalized to at least 1, "
        f"with {timeout_rules} below. |"
    )
    return [
        "## Availability and configuration",
        "",
        f"The feature key is `features.{facts.feature_key}` (stage `{facts.feature_stage}`, "
        f"default `{str(facts.feature_default_enabled).lower()}`). The legacy feature alias "
        "`features.codex_hooks` resolves to the same feature. Guardian review sessions disable "
        "hooks even when their parent session enables them.",
        "",
        "Hooks can be declared under `[hooks]` in a config layer or in a sibling `hooks.json`. "
        "The JSON wrapper accepts an optional `description` plus a `hooks` object. If both "
        "representations contain hooks for the same layer, Codex loads both and emits a warning.",
        "",
        "```toml",
        "[features]",
        f"{facts.feature_key} = true",
        "",
        "[[hooks.PreToolUse]]",
        "matcher = '^(Bash|apply_patch)$'",
        "",
        "[[hooks.PreToolUse.hooks]]",
        'type = "command"',
        'command = "python3 .codex/hooks/pre_tool.py"',
        'commandWindows = "python .codex\\\\hooks\\\\pre_tool.py"',
        "timeout = 30",
        'statusMessage = "Checking policy"',
        "additionalContextLimit = 2500",
        "",
        "[hooks.state.'C:\\path\\to\\.codex\\hooks.json:pre_tool_use:0:0']",
        "enabled = true",
        'trusted_hash = "<hash returned by hooks/list>"',
        "```",
        "",
        handler_summary,
        "",
        "<!-- prettier-ignore -->",
        "| Command field | Meaning |",
        "| --- | --- |",
        "| `command` | Shell command used on non-Windows platforms and as the Windows fallback. |",
        "| `commandWindows` | Optional Windows-only command override; `command_windows` is accepted as an alias. |",
        timeout_summary,
        async_summary,
        "| `statusMessage` | Optional UI text carried in running/completed summaries. |",
        f"| `additionalContextLimit` | Approximate per-handler token spill threshold; absent uses {facts.additional_context_token_limit:,}, `0` disables spilling. Valid only for context-producing events. |",
        "",
        "Hook state keys use `<source>:<snake_case_event>:<matcher_group_index>:<handler_index>`. "
        "The positional suffix is not yet a durable hook ID. Only user and session-flag layers "
        "may set `enabled` and `trusted_hash`; later layers win field by field.",
        "",
    ]


def _runtime_section(facts: RuntimeFacts, event_names: frozenset[str]) -> list[str]:
    matcher_agnostic_events = (
        "`UserPromptSubmit`, `Stop`, and `Interrupt`"
        if "Interrupt" in event_names
        else "`UserPromptSubmit` and `Stop`"
    )
    bounded_timeout_events = (
        "`SessionEnd` and `Interrupt`" if "Interrupt" in event_names else "`SessionEnd`"
    )
    return [
        "## Discovery, trust, matching, and execution",
        "",
        "### Discovery and trust",
        "",
        "1. Managed requirements hooks are appended first. Across requirement layers, event "
        "lists are append-only; conflicting active-platform managed directories fail closed.",
        "2. Active config layers are visited from lowest to highest precedence. Each unique layer "
        "folder may contribute `hooks.json`, followed by the layer's TOML hooks.",
        "3. Plugin hook sources are appended last. Plugin commands receive `PLUGIN_ROOT`, "
        "`CLAUDE_PLUGIN_ROOT`, `PLUGIN_DATA`, and `CLAUDE_PLUGIN_DATA`; `${NAME}` placeholders "
        "for these variables are expanded before execution.",
        "4. `allow_managed_hooks_only` filters every unmanaged source, including user, project, "
        "session-flag, and plugin hooks. Managed hooks are always enabled and have trust status "
        "`managed`.",
        "",
        "Unmanaged commands are hashed from a normalized event/matcher/handler identity. A matching "
        "stored hash is `trusted`; a different stored hash is `modified`; no stored hash is "
        "`untrusted`. Only enabled managed/trusted hooks execute, unless the invocation uses "
        "`--dangerously-bypass-hook-trust`. Bypass does not re-enable an explicitly disabled hook.",
        "",
        "### Matcher rules",
        "",
        'An omitted matcher, `""`, or `"*"` matches all. A pattern containing only ASCII '
        "letters, digits, `_`, and `|` is an exact-name list; other patterns compile as Rust "
        "regular expressions. Invalid regexes skip the entire matcher group with a warning. "
        f"{matcher_agnostic_events} discard configured matchers. Tool events also test internal "
        "compatibility aliases, but each handler runs at most once and stdin always retains the "
        "canonical tool name (`apply_patch` also matches `Write`/`Edit`; `spawn_agent` also matches "
        "`Agent`).",
        "",
        "### Command protocol and scheduling",
        "",
        "Every selected command receives one compact JSON object on stdin (no trailing newline), "
        "runs with the event `cwd`, and has piped stdin/stdout/stderr. On Windows the fallback is "
        "`%COMSPEC% /C`; elsewhere it is `$SHELL -lc`. A connected execution environment may "
        "supply its own derived shell program/arguments. Output bytes are decoded lossily as UTF-8, "
        "and the child is configured to be killed if its future is dropped.",
        "",
        "All matching handlers for one event start concurrently. Results are collected by completion "
        "but sorted back into configured/display order for reporting and most aggregation. The one "
        "intentional completion-order rule is competing `PreToolUse.updatedInput`: the last process "
        "to finish wins. Duplicate declarations are not deduplicated.",
        "",
        "<!-- prettier-ignore -->",
        "| Timeout class | Default | Bound |",
        "| --- | ---: | ---: |",
        f"| Most command hooks | {facts.default_timeout_sec}s | minimum 1s; no explicit maximum |",
        f"| {bounded_timeout_events} | {facts.session_end_default_timeout_sec}s | 1–{facts.session_end_max_timeout_sec}s |",
        "",
        "Spawn, stdin-write, wait, timeout, missing-exit-code, serialization, and nonzero-exit errors "
        "are recorded as failed hook runs; they do not by themselves abort unrelated matching "
        "handlers. Exit code `0` enables event-specific stdout parsing. Exit code `2` is a special "
        "block/deny signal only for `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, "
        "`PostToolUse`, `SubagentStop`, and `Stop`, and requires a non-empty stderr reason.",
        "",
    ]


def _output_section(facts: RuntimeFacts, event_count: int) -> list[str]:
    return [
        "## Output protocol, context limits, and observability",
        "",
        "A successful JSON response must be exactly one JSON object. Unknown fields are rejected by "
        "the generated command schemas. Universal fields are `continue` (default `true`), "
        "`stopReason`, `suppressOutput` (default `false`), and `systemMessage`. Event parsers apply "
        "stricter semantic rules than the schemas: for example, `PreToolUse` and "
        "`PermissionRequest` reject `continue:false`, `stopReason`, and `suppressOutput`; "
        "`PostToolUse` rejects `suppressOutput`. `systemMessage` becomes a warning entry. Several "
        "events currently parse but otherwise ignore `suppressOutput`.",
        "",
        "`additionalContext` is supported only by `SessionStart`, `SubagentStart`, "
        "`UserPromptSubmit`, `PreToolUse`, and `PostToolUse`. Each original context is compared with "
        f"an approximate {facts.additional_context_token_limit:,}-token default. Oversized text is "
        "saved in full below `<OS temp>/hook_outputs/<thread_id>/<uuid>.txt` and replaced by a "
        "budgeted head/tail preview plus the recovery path. A write failure falls back to an in-memory "
        "truncated preview. `additionalContextLimit = 0` disables spilling for that handler.",
        "",
        "Codex emits `hook/started` and `hook/completed` app-server v2 notifications. Run summaries "
        "include ID, event, handler/execution type, scope, source/path, display order, status message, "
        "timestamps, duration, status, and typed entries (`warning`, `stop`, `feedback`, `context`, "
        "or `error`). `hooks/list` resolves effective hook metadata, warnings, and errors for each cwd, "
        "including enablement, current hash, trust status, plugin ID, and configured context limit. "
        "Completed runs also emit count/duration metrics and analytics tagged by event, source, and "
        "status. Hook started/completed events are not reconstructed into thread history.",
        "",
        "### Legacy `notify` hook",
        "",
        f"Top-level `notify = [program, args...]` is separate from the {event_count} configurable hook "
        "events. After an agent turn, Codex appends one kebab-case JSON argument with type "
        "`agent-turn-complete`, thread/turn IDs, cwd, optional client, input messages, and the last "
        "assistant message. It uses null stdio, does not wait for completion, and a spawn failure is "
        "logged while the turn continues.",
        "",
    ]


def _event_catalog(schemas: list[HookSchemas]) -> list[str]:
    lines = [
        "## Event catalog",
        "",
        "<!-- prettier-ignore -->",
        "| Event | Fires | Matcher input | Scope | Output schema |",
        "| --- | --- | --- | --- | --- |",
    ]
    for item in schemas:
        output = f"`{item.output_path.name}`" if item.output_path else "none"
        event = item.metadata
        lines.append(
            f"| [`{event.name}`](#{event.slug}) | {event.timing} | {event.matcher} | "
            f"`{event.scope}` | {output} |"
        )
    lines.append("")
    return lines


def _event_details(item: HookSchemas) -> list[str]:
    event = item.metadata
    assert item.input_path is not None and item.input_schema is not None
    lines = [
        f"## {event.name}",
        "",
        f"- Timing: {event.timing}",
        f"- Matcher: {event.matcher}",
        f"- Scope: `{event.scope}`.",
        f"- Plain stdout on exit 0: {event.plain_stdout}",
        f"- Exit code 2: {event.exit_two}",
        f"- Aggregation: {event.aggregation}",
    ]
    lines.extend(f"- {behavior}" for behavior in event.behavior)
    lines.extend(
        [
            f"- Runtime source: `{event.source}`.",
            "",
            f"### {event.name} command input",
            "",
            f"Canonical fixture: `{_display_path(item.input_path)}`.",
            "",
            *_schema_table(item.input_schema),
            "",
            *_raw_schema(item.input_path, item.input_schema),
            "",
            f"### {event.name} command output",
            "",
        ]
    )
    if item.output_path is None or item.output_schema is None:
        lines.extend(
            [
                "`SessionEnd` does not declare a command output schema. Stdout is ignored; only "
                "process completion, timeout/error state, exit code, and stderr on failure affect "
                "the run summary.",
                "",
            ]
        )
    else:
        lines.extend(
            [
                f"Canonical fixture: `{_display_path(item.output_path)}`.",
                "",
                *_schema_table(item.output_schema),
                "",
                *_raw_schema(item.output_path, item.output_schema),
                "",
            ]
        )
    return lines


def render_markdown(
    schemas: list[HookSchemas],
    facts: RuntimeFacts,
    handler_types: tuple[str, ...],
    *,
    title: str = DEFAULT_TITLE,
) -> str:
    event_names = frozenset(item.metadata.name for item in schemas)
    fixture_count = sum(item.input_path is not None for item in schemas) + sum(
        item.output_path is not None for item in schemas
    )
    lines = [
        f"# {title}",
        "",
        "<!-- Generated by scripts/generate_hooks_reference.py. Do not edit manually. -->",
        "",
        f"This reference covers all {len(schemas)} hook events and all {fixture_count} committed "
        "command schema fixtures in `codex-rs/hooks/schema/generated/`. Field tables and raw "
        "schemas come directly from those fixtures; lifecycle and operational details are traced "
        "to the Rust sources named throughout the document.",
        "",
        "The schemas describe accepted wire shapes. The runtime may intentionally support only a "
        "subset of schema-reserved decisions; those restrictions are called out below.",
        "",
        *_event_catalog(schemas),
        *_configuration_section(facts, handler_types, event_names),
        *_runtime_section(facts, event_names),
        *_output_section(facts, len(schemas)),
    ]
    for item in schemas:
        lines.extend(_event_details(item))
    lines.extend(
        [
            "## Canonical source map",
            "",
            "- `codex-rs/config/src/hook_config.rs`: config and handler shapes.",
            "- `codex-rs/hooks/src/engine/discovery.rs`: sources, precedence, normalization, trust, and enablement.",
            "- `codex-rs/hooks/src/engine/dispatcher.rs`: matching, concurrency, ordering, summaries, and scopes.",
            "- `codex-rs/hooks/src/engine/command_runner.rs`: shell, stdin/stdout/stderr, timeout, and process behavior.",
            "- `codex-rs/hooks/src/engine/output_parser.rs`: JSON decisions and unsupported combinations.",
            "- `codex-rs/hooks/src/events/*.rs`: per-event timing inputs, exit semantics, and aggregation.",
            "- `codex-rs/core/src/hook_runtime.rs`: integration into turns, tools, approvals, compaction, and subagents.",
            "- `codex-rs/hooks/src/output_spill.rs`: model-visible context budget and spill files.",
            "- `codex-rs/app-server-protocol/src/protocol/v2/hook.rs` and `plugin.rs`: public notifications and catalog metadata.",
            "- `codex-rs/hooks/src/legacy_notify.rs`: legacy post-turn notification compatibility.",
            "",
        ]
    )
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--schema-dir",
        type=Path,
        default=DEFAULT_SCHEMA_DIR,
        help=f"Directory containing generated hook schemas (default: {DEFAULT_SCHEMA_DIR})",
    )
    output_group = parser.add_mutually_exclusive_group()
    output_group.add_argument(
        "-o",
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT_PATH,
        help=f"Markdown file to write (default: {DEFAULT_OUTPUT_PATH}).",
    )
    output_group.add_argument(
        "--stdout",
        action="store_true",
        help="Write Markdown to stdout instead of the default output file.",
    )
    parser.add_argument(
        "--title",
        default=DEFAULT_TITLE,
        help=f"Top-level heading (default: {DEFAULT_TITLE!r}).",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        schemas = load_hook_schemas(args.schema_dir)
        registered = registered_hook_event_names()
        documented = tuple(event.metadata.name for event in schemas)
        if set(registered) != set(documented):
            raise ValueError(
                "event metadata does not match HookEventsToml: "
                f"registered={registered!r}, documented={documented!r}"
            )
        markdown = render_markdown(
            schemas,
            load_runtime_facts(),
            configured_handler_types(),
            title=args.title,
        )
        if args.stdout:
            sys.stdout.reconfigure(encoding="utf-8", newline="\n")
            sys.stdout.write(markdown)
        else:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            with args.output.open("w", encoding="utf-8", newline="\n") as output_file:
                output_file.write(markdown)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
