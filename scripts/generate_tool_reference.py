#!/usr/bin/env python3
"""Generate a Markdown catalog of Codex's core and bundled-extension tools."""

import argparse
from dataclasses import dataclass, field
import json
from pathlib import Path
import re
import sys


REPO_ROOT = Path(__file__).resolve().parent.parent
RUST_ROOT = REPO_ROOT / "codex-rs"
DEFAULT_OUTPUT_PATH = REPO_ROOT / "docs" / "tool-reference.md"
NO_DESCRIPTION = "_Description is generated at runtime or is not available._"

AVAILABILITY = {
    name: gate
    for rule in """
apply_patch :: No dedicated feature; execution environment + model capability.
clock.curr_time :: `features.current_time_reminder`.
clock.sleep :: `features.current_time_reminder` + its `sleep_tool` setting.
create_goal get_goal update_goal :: `features.goals` + persistent state.
exec wait :: `features.code_mode`/`code_mode_only`, or model tool mode.
exec_command write_stdin :: `features.shell_tool` + `features.unified_exec` + environment.
shell_command :: `features.shell_tool` + one local environment.
followup_task interrupt_agent list_agents send_message spawn_agent wait_agent :: `features.multi_agent_v2` or model-selected v2.
get_context_remaining new_context :: `features.token_budget`.
image_gen.imagegen :: `features.image_generation` + provider, model, and auth capabilities.
list_available_plugins_to_install request_plugin_install :: `features.tool_suggest`, `features.apps`, and `features.plugins`.
list_mcp_resource_templates list_mcp_resources read_mcp_resource :: No dedicated feature; at least one configured MCP server.
memories.add_ad_hoc_note memories.list memories.read memories.search :: `features.memories` + `memories.use_memories` + `dedicated_tools`.
multi_agent_v1.close_agent multi_agent_v1.resume_agent multi_agent_v1.send_input multi_agent_v1.spawn_agent multi_agent_v1.wait_agent :: `features.multi_agent` or model-selected v1 + `agents.enabled`.
request_permissions :: `features.request_permissions_tool` + execution environment.
request_user_input :: `tools.experimental_request_user_input.enabled`; Default mode also uses `features.default_mode_request_user_input`.
skills.list skills.read :: No dedicated feature; enabled skill provider/orchestrator setting.
test_sync_tool :: No user feature; model test-tool capability.
tool_search :: No active feature; model search + namespaces + deferred tools.
update_plan :: `tools.update_plan.enabled`.
view_image :: No dedicated feature; execution environment.
wait_for_environment :: `features.deferred_executor`.
web.run :: `features.standalone_web_search` or Responses Lite + web settings.
web_search :: No dedicated feature; web mode + provider/model capability.
""".strip().splitlines()
    for names, gate in [rule.split(" :: ", 1)]
    for name in names.split()
}


@dataclass
class Tool:
    name: str
    kind: str
    descriptions: list[str] = field(default_factory=list)
    sources: set[str] = field(default_factory=set)
    input_contracts: list[str] = field(default_factory=list)
    output_contracts: list[str] = field(default_factory=list)


def _matching_delimiter(text: str, start: int, opening: str, closing: str) -> int:
    depth = 1
    index = start + 1
    while index < len(text):
        if text.startswith("//", index):
            index = text.find("\n", index)
            if index < 0:
                return len(text) - 1
        elif text.startswith("/*", index):
            end = text.find("*/", index + 2)
            index = len(text) if end < 0 else end + 2
        elif text[index] == '"':
            index += 1
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                elif text[index] == '"':
                    index += 1
                    break
                else:
                    index += 1
        elif text[index] == "r" and re.match(r'r#*"', text[index:]):
            match = re.match(r'r(#{0,16})"', text[index:])
            assert match is not None
            marker = '"' + match.group(1)
            end = text.find(marker, index + len(match.group(0)))
            index = len(text) if end < 0 else end + len(marker)
        else:
            if text[index] == opening:
                depth += 1
            elif text[index] == closing:
                depth -= 1
                if depth == 0:
                    return index
            index += 1
    raise ValueError(f"unclosed {opening!r} at byte {start}")


def _blocks(text: str, marker: str) -> list[tuple[int, int, str]]:
    blocks = []
    for match in re.finditer(re.escape(marker) + r"\s*\{", text):
        start = text.find("{", match.start())
        end = _matching_delimiter(text, start, "{", "}")
        blocks.append((match.start(), end + 1, text[start + 1 : end]))
    return blocks


def _without_test_modules(text: str) -> str:
    pattern = re.compile(
        r"#\[cfg\(test\)\]\s*(?:#\[path\s*=\s*[^]]+\]\s*)?mod\s+\w+\s*\{"
    )
    while match := pattern.search(text):
        start = text.find("{", match.start())
        end = _matching_delimiter(text, start, "{", "}")
        text = text[: match.start()] + " " * (end + 1 - match.start()) + text[end + 1 :]
    return text


def _string_literal(expression: str) -> str | None:
    raw = re.search(r'r(#{0,16})"(.*?)"\1', expression, re.DOTALL)
    if raw:
        return raw.group(2)
    regular = re.search(r'"(?:\\.|[^"\\])*"', expression, re.DOTALL)
    if regular:
        try:
            return json.loads(regular.group(0))
        except json.JSONDecodeError:
            return None
    return None


def _field(block: str, name: str) -> str | None:
    match = re.search(rf"\b{re.escape(name)}\s*:\s*", block)
    if not match:
        return None
    start = match.end()
    depth = 0
    index = start
    while index < len(block):
        char = block[index]
        raw = re.match(r'r(#{0,16})"', block[index:])
        if raw:
            marker = '"' + raw.group(1)
            end = block.find(marker, index + len(raw.group(0)))
            if end < 0:
                return None
            index = end + len(marker)
            continue
        if char == '"':
            literal = _string_literal(block[index:])
            if literal is None:
                return None
            quoted = re.match(r'"(?:\\.|[^"\\])*"', block[index:], re.DOTALL)
            assert quoted is not None
            index += len(quoted.group(0))
            continue
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
        elif char == "," and depth == 0:
            return block[start:index].strip()
        index += 1
    return block[start:].strip()


def _constants(
    files: dict[Path, str],
) -> tuple[dict[tuple[Path, str], str], dict[str, str]]:
    local = {}
    by_name: dict[str, set[str]] = {}
    pattern = re.compile(
        r"\bconst\s+(\w+)\s*:\s*&str\s*=\s*(include_str!\(\s*\"[^\"]+\"\s*\)|r#{0,16}\".*?\"#{0,16}|\"(?:\\.|[^\"\\])*\")\s*;",
        re.DOTALL,
    )
    for path, text in files.items():
        for match in pattern.finditer(text):
            include = re.fullmatch(r'include_str!\(\s*"([^"]+)"\s*\)', match.group(2))
            value = (
                (path.parent / include.group(1))
                .resolve()
                .read_text(encoding="utf-8")
                .strip()
                if include
                else _string_literal(match.group(2))
            )
            if value is not None:
                local[path, match.group(1)] = value
                by_name.setdefault(match.group(1), set()).add(value)
    unique = {
        name: next(iter(values)) for name, values in by_name.items() if len(values) == 1
    }
    return local, unique


def _evaluate(
    expression: str | None, path: Path, constants: tuple[dict, dict]
) -> str | None:
    if not expression:
        return None
    local, unique = constants
    include = re.search(r'include_str!\(\s*"([^"]+)"\s*\)', expression)
    if include:
        included = (path.parent / include.group(1)).resolve()
        return included.read_text(encoding="utf-8").strip()
    literal = _string_literal(expression)
    if literal is not None:

        def substitute(match: re.Match) -> str:
            return local.get(
                (path, match.group(1)), unique.get(match.group(1), match.group(0))
            )

        literal = re.sub(r"\{(\w+)\}", substitute, literal)
        return literal.replace("{}", "{runtime value}").strip()
    identifier = re.search(
        r"(?:\w+::)*(\w+)(?:\.to_string\(\)|\.clone\(\))?\s*$", expression
    )
    if identifier:
        name = identifier.group(1)
        return local.get((path, name), unique.get(name))
    return None


def _expand_contract(text: str, start: int, expression: str | None) -> str | None:
    if not expression:
        return expression
    snippets: list[str] = []
    if re.search(r"\bproperties\b", expression):
        assignments = list(
            re.finditer(r"\blet\s+(?:mut\s+)?properties\s*=\s*", text[:start])
        )
        if assignments:
            assignment = assignments[-1]
            delimiters = [(text.find(char, assignment.end()), char) for char in "([{"]
            delimiter_start, opening = min(item for item in delimiters if item[0] >= 0)
            closing = {"(": ")", "[": "]", "{": "}"}[opening]
            delimiter_end = _matching_delimiter(text, delimiter_start, opening, closing)
            assignment_end = text.find(";", delimiter_end, start) + 1
            snippets.append(text[assignment.start() : assignment_end].strip())
            tail = text[assignment_end:start]
            for mutation in re.finditer(r"\bproperties\.(?:insert|extend)\s*\(", tail):
                call_start = tail.find("(", mutation.start())
                call_end = _matching_delimiter(tail, call_start, "(", ")")
                statement_end = tail.find(";", call_end) + 1
                snippets.append(tail[mutation.start() : statement_end].strip())
    helper_names = set(
        re.findall(r"\b([a-z]\w*(?:schema|parameters)\w*)\s*\(", expression)
    )
    for helper_name in sorted(helper_names):
        helper = re.search(rf"\bfn\s+{re.escape(helper_name)}\s*\(", text)
        if helper:
            body_start = text.find("{", helper.end())
            body_end = _matching_delimiter(text, body_start, "{", "}")
            snippets.append(text[helper.start() : body_end + 1].strip())
    snippets.append(expression)
    return "\n\n".join(snippets)


def _type_definition(type_name: str, files: dict[Path, str]) -> str | None:
    pattern = re.compile(rf"\b(?:struct|enum)\s+{re.escape(type_name)}\b[^{{;]*\{{")
    for text in files.values():
        if match := pattern.search(text):
            body_start = text.find("{", match.start())
            body_end = _matching_delimiter(text, body_start, "{", "}")
            return text[match.start() : body_end + 1].strip()
    return None


def collect_tools(root: Path = RUST_ROOT) -> list[Tool]:
    scan_roots = [
        root / "core/src/tools",
        root / "ext",
        root / "tools/src",
        root / "code-mode-protocol/src",
    ]
    paths = {
        path
        for scan_root in scan_roots
        for path in scan_root.rglob("*.rs")
        if "tests" not in path.stem
    }
    protocol_models = root / "protocol/src/models.rs"
    if protocol_models.is_file():
        paths.add(protocol_models)
    files = {
        path: _without_test_modules(path.read_text(encoding="utf-8")) for path in paths
    }
    constants = _constants(files)
    tools: dict[str, Tool] = {}

    def add(
        name: str | None,
        kind: str,
        description: str | None,
        path: Path,
        input_contract: str | None = None,
        output_contract: str | None = None,
    ) -> None:
        if not name:
            return
        entry = tools.setdefault(name, Tool(name, kind))
        if description and description not in entry.descriptions:
            entry.descriptions.append(description)
        if input_contract and input_contract not in entry.input_contracts:
            entry.input_contracts.append(input_contract)
        if output_contract and output_contract not in entry.output_contracts:
            entry.output_contracts.append(output_contract)
        entry.sources.add(path.relative_to(root.parent).as_posix())

    for path, text in files.items():
        namespaces = _blocks(text, "ResponsesApiNamespace")
        for marker, kind in (
            ("ResponsesApiTool", "function"),
            ("FreeformTool", "freeform"),
        ):
            for start, _, block in _blocks(text, marker):
                name = _evaluate(_field(block, "name"), path, constants)
                description = _evaluate(_field(block, "description"), path, constants)
                if description is None and re.search(r"\bdescription\s*,", block):
                    assignments = list(
                        re.finditer(
                            r"\blet\s+description\s*=\s*(.*?);\s*(?=ToolSpec::)",
                            text[:start],
                            re.DOTALL,
                        )
                    )
                    if assignments:
                        description = _evaluate(
                            assignments[-1].group(1), path, constants
                        )
                if description is None and name == "wait_for_environment":
                    description = _evaluate("DEFAULT_TOOL_DESCRIPTION", path, constants)
                containing = [item for item in namespaces if item[0] < start < item[1]]
                if containing:
                    namespace = min(containing, key=lambda item: item[1] - item[0])
                    namespace_name = _evaluate(
                        _field(namespace[2], "name"), path, constants
                    )
                    if namespace_name and name:
                        name = f"{namespace_name}.{name}"
                input_contract = _field(
                    block, "parameters" if kind == "function" else "format"
                )
                if input_contract is None and re.search(r"\bparameters\s*,", block):
                    input_contract = (
                        "parse_tool_input_schema_without_compaction(&commands_schema())"
                    )
                output_contract = (
                    _field(block, "output_schema") if kind == "function" else "None"
                )
                input_contract = _expand_contract(text, start, input_contract)
                output_contract = _expand_contract(text, start, output_contract)
                if name == "image_gen.imagegen":
                    definition = _type_definition("ImagegenArgs", files)
                    input_contract = "schema_for::<ImagegenArgs>()"
                    if definition:
                        input_contract += f"\n\n{definition}"
                add(
                    name,
                    kind,
                    description,
                    path,
                    input_contract,
                    output_contract,
                )

        helper_pattern = re.compile(
            r"(?P<helper>(?:memory|skill)_function_tool)\s*::\s*<\s*(?P<input>[\w:]+)\s*,\s*(?P<output>[\w:]+)\s*>\s*\(\s*(?P<name>[^,]+),\s*(?P<description>r#{0,16}\".*?\"#{0,16}|\"(?:\\.|[^\"\\])*\")",
            re.DOTALL,
        )
        for match in helper_pattern.finditer(text):
            namespace = (
                "memories" if match.group("helper").startswith("memory") else "skills"
            )
            name = _evaluate(match.group("name"), path, constants)
            input_contract = f"schema::input_schema_for::<{match.group('input')}>()"
            output_contract = (
                f"Some(schema::output_schema_for::<{match.group('output')}>())"
            )
            if definition := _type_definition(match.group("input"), {path: text}):
                input_contract += f"\n\n{definition}"
            if definition := _type_definition(match.group("output"), files):
                output_contract += f"\n\n{definition}"
            add(
                f"{namespace}.{name}" if name else None,
                "function",
                _string_literal(match.group("description")),
                path,
                input_contract,
                output_contract,
            )

    add(
        "tool_search",
        "hosted",
        "Search deferred tool metadata and load matching tools.",
        root / "core/src/tools/handlers/tool_search_spec.rs",
        'JsonSchema::object(properties, Some(vec!["query".to_string()]), Some(false.into()))',
        "None",
    )
    add(
        "web_search",
        "hosted",
        "Search the web using the provider-hosted web search tool.",
        root / "core/src/tools/hosted_spec.rs",
        "Provider-hosted WebSearch options; no function-call input JSON Schema.",
        "Provider-hosted result; no output JSON Schema declared in ToolSpec.",
    )
    return sorted(tools.values(), key=lambda tool: tool.name.casefold())


def render_markdown(tools: list[Tool]) -> str:
    lines = [
        "# Codex tool reference",
        "",
        "<!-- Generated by scripts/generate_tool_reference.py. Do not edit manually. -->",
        "",
        "This catalog lists every statically defined model tool in Codex's core registry and bundled extensions. A tool may require a feature, model capability, execution environment, extension, or collaboration mode before it is exposed.",
        "",
        "<!-- prettier-ignore -->",
        "| Tool | Feature / availability | Kind | Description | Source |",
        "| --- | --- | --- | --- | --- |",
    ]
    for tool in tools:
        description = (
            "<br><br>".join(tool.descriptions) if tool.descriptions else NO_DESCRIPTION
        )
        description = description.replace("|", "\\|").replace("\n", "<br>")
        sources = "<br>".join(f"`{source}`" for source in sorted(tool.sources))
        availability = AVAILABILITY.get(
            tool.name, "No dedicated feature; availability is runtime-defined."
        )
        lines.append(
            f"| `{tool.name}` | {availability} | `{tool.kind}` | {description} | {sources} |"
        )
    lines.extend(
        [
            "",
            "## Runtime-defined tool families",
            "",
            "These cannot be enumerated statically because their names and descriptions come from the active session:",
            "",
            "- MCP, app, and plugin tools: configured servers plus `features.apps`/`features.plugins` where applicable.",
            "- Client-supplied `dynamic_tools`: no feature; availability comes from the active client session.",
            "- Deferred tools: model search/namespace capability; discovered and loaded through `tool_search`.",
            "- Configurably namespaced core functions: notably collaboration tools gated by multi-agent v2.",
            "",
            "## Input and output contracts",
            "",
            "Contracts below are canonical Rust schema expressions. `None` means the ToolSpec declares no output JSON Schema; the runtime handler may still return content.",
            "",
        ]
    )
    for tool in tools:
        lines.extend([f"### `{tool.name}`", "", "**Input**", ""])
        for contract in tool.input_contracts or ["No input schema found statically."]:
            lines.extend(["```rust", contract, "```", ""])
        lines.extend(["**Output**", ""])
        for contract in tool.output_contracts or ["No output schema found statically."]:
            lines.extend(["```rust", contract, "```", ""])
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
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
    args = parser.parse_args()
    try:
        markdown = render_markdown(collect_tools())
        if args.stdout:
            sys.stdout.reconfigure(encoding="utf-8", newline="\n")
            sys.stdout.write(markdown)
        else:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(markdown, encoding="utf-8", newline="\n")
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
