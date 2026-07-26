#!/usr/bin/env python3

"""Generate a Markdown reference from Codex's config JSON Schema."""

import argparse
import json
import sys
from collections import OrderedDict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_SCHEMA = REPO_ROOT / "codex-rs/core/config.schema.json"
DEFAULT_OUTPUT = Path("config-reference.md")
UNION_KEYS = ("anyOf", "oneOf")


def merge_schemas(base: dict[str, Any], overlay: dict[str, Any]) -> dict[str, Any]:
    """Merge the parts of JSON Schema objects that can be composed by `allOf`."""
    merged = dict(base)
    for key, value in overlay.items():
        if key == "properties" and isinstance(value, dict):
            properties = dict(merged.get("properties", {}))
            properties.update(value)
            merged[key] = properties
        elif key == "required" and isinstance(value, list):
            merged[key] = list(dict.fromkeys([*merged.get(key, []), *value]))
        else:
            merged[key] = value
    return merged


class SchemaResolver:
    def __init__(self, root: dict[str, Any]) -> None:
        self.root = root

    def resolve_ref(self, ref: str) -> dict[str, Any]:
        if not ref.startswith("#/"):
            raise ValueError(f"unsupported non-local schema reference: {ref}")

        value: Any = self.root
        for encoded_part in ref[2:].split("/"):
            part = encoded_part.replace("~1", "/").replace("~0", "~")
            if not isinstance(value, dict) or part not in value:
                raise ValueError(f"schema reference does not exist: {ref}")
            value = value[part]
        if not isinstance(value, dict):
            raise ValueError(f"schema reference is not an object: {ref}")
        return value

    def normalize(
        self, schema: dict[str, Any], resolving: frozenset[str] = frozenset()
    ) -> dict[str, Any]:
        """Resolve top-level references and `allOf` compositions."""
        normalized: dict[str, Any] = {}
        ref = schema.get("$ref")
        if isinstance(ref, str) and ref not in resolving:
            normalized = self.normalize(self.resolve_ref(ref), resolving | {ref})

        for component in schema.get("allOf", []):
            if isinstance(component, dict):
                normalized = merge_schemas(
                    normalized, self.normalize(component, resolving)
                )

        local = {
            key: value for key, value in schema.items() if key not in {"$ref", "allOf"}
        }
        return merge_schemas(normalized, local)


def json_literal(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def enum_values(schema: dict[str, Any], resolver: SchemaResolver) -> list[Any] | None:
    node = resolver.normalize(schema)
    if "const" in node:
        return [node["const"]]
    if isinstance(node.get("enum"), list):
        return node["enum"]

    union_key = next((key for key in UNION_KEYS if key in node), None)
    if union_key is None:
        return None

    values: list[Any] = []
    for variant in node[union_key]:
        if not isinstance(variant, dict):
            return None
        variant_values = enum_values(variant, resolver)
        if variant_values is None:
            return None
        values.extend(variant_values)

    unique_values: list[Any] = []
    seen_values: set[str] = set()
    for value in values:
        marker = json_literal(value)
        if marker not in seen_values:
            unique_values.append(value)
            seen_values.add(marker)
    return unique_values


def type_label(schema: dict[str, Any], resolver: SchemaResolver) -> str:
    node = resolver.normalize(schema)
    values = enum_values(node, resolver)
    if values is not None:
        rendered_values = [
            value if isinstance(value, str) else json_literal(value) for value in values
        ]
        return f"enum {{{', '.join(rendered_values)}}}"

    union_key = next((key for key in UNION_KEYS if key in node), None)
    if union_key is not None:
        labels = [
            type_label(variant, resolver)
            for variant in node[union_key]
            if isinstance(variant, dict)
        ]
        labels = list(dict.fromkeys(labels))
        if labels:
            return " / ".join(labels)

    schema_type = node.get("type")
    if isinstance(schema_type, list):
        return " / ".join(str(value) for value in schema_type)
    if schema_type == "array":
        items = node.get("items")
        item_type = type_label(items, resolver) if isinstance(items, dict) else "value"
        return f"array<{item_type}>"
    if schema_type == "object" or "properties" in node:
        additional = node.get("additionalProperties")
        if isinstance(additional, dict) and not node.get("properties") and additional:
            return f"table<string, {type_label(additional, resolver)}>"
        return "table"
    if isinstance(schema_type, str):
        schema_format = node.get("format")
        return (
            f"{schema_type} ({schema_format})"
            if isinstance(schema_format, str)
            else schema_type
        )
    return "value"


def top_level_refs(schema: dict[str, Any]) -> set[str]:
    refs: set[str] = set()
    ref = schema.get("$ref")
    if isinstance(ref, str):
        refs.add(ref)
    for component in schema.get("allOf", []):
        if isinstance(component, dict):
            refs.update(top_level_refs(component))
    return refs


@dataclass
class ConfigRow:
    path: str
    types: list[str] = field(default_factory=list)
    defaults: list[str] = field(default_factory=list)
    description: str = ""

    def add_schema(self, schema: dict[str, Any], resolver: SchemaResolver) -> None:
        node = resolver.normalize(schema)
        label = type_label(node, resolver)
        if label not in self.types:
            self.types.append(label)
        if "default" in node:
            default = json_literal(node["default"])
            if default not in self.defaults:
                self.defaults.append(default)
        if not self.description and isinstance(node.get("description"), str):
            self.description = node["description"].strip()


class ConfigCollector:
    def __init__(self, schema: dict[str, Any]) -> None:
        self.resolver = SchemaResolver(schema)
        self.rows: OrderedDict[str, ConfigRow] = OrderedDict()

    def collect(self) -> list[ConfigRow]:
        self._collect_children(self.resolver.root, "", frozenset())
        return list(self.rows.values())

    def _add(self, path: str, schema: dict[str, Any]) -> None:
        row = self.rows.setdefault(path, ConfigRow(path))
        row.add_schema(schema, self.resolver)

    def _collect_property(
        self,
        path: str,
        schema: dict[str, Any],
        ancestor_refs: frozenset[str],
    ) -> None:
        self._add(path, schema)
        self._collect_children(schema, path, ancestor_refs)

    def _collect_children(
        self,
        schema: dict[str, Any],
        path: str,
        ancestor_refs: frozenset[str],
    ) -> None:
        refs = top_level_refs(schema)
        if refs & ancestor_refs:
            return
        descendant_refs = ancestor_refs | refs
        node = self.resolver.normalize(schema)

        properties = node.get("properties")
        if isinstance(properties, dict):
            for name in sorted(properties):
                child = properties[name]
                if not isinstance(child, dict):
                    continue
                child_path = f"{path}.{name}" if path else name
                self._collect_property(child_path, child, descendant_refs)

        additional = node.get("additionalProperties")
        if isinstance(additional, dict) and additional:
            child_path = f"{path}.<name>" if path else "<name>"
            self._collect_property(child_path, additional, descendant_refs)

        items = node.get("items")
        if isinstance(items, dict):
            self._collect_children(items, f"{path}[]", descendant_refs)

        for union_key in UNION_KEYS:
            for variant in node.get(union_key, []):
                if isinstance(variant, dict):
                    self._collect_children(variant, path, descendant_refs)


def markdown_code(value: str) -> str:
    delimiter = "`"
    while delimiter in value:
        delimiter += "`"
    padding = " " if value.startswith("`") or value.endswith("`") else ""
    return f"{delimiter}{padding}{value}{padding}{delimiter}"


def markdown_cell(value: str) -> str:
    return value.replace("|", "\\|").replace("\r\n", "\n").replace("\n", "<br>")


def render_markdown(schema: dict[str, Any], schema_label: str) -> str:
    rows = ConfigCollector(schema).collect()
    title = schema.get("title", "Configuration")
    lines = [
        "<!-- Generated by scripts/generate_config_reference.py. Do not edit. -->",
        "",
        f"# {title} reference",
        "",
        f"Generated from {markdown_code(schema_label)}.",
        "",
        "Dynamic table keys are represented by `<name>` and array elements by `[]`.",
        "",
        "| Configuration | Type | Default | Description |",
        "| --- | --- | --- | --- |",
    ]
    for row in rows:
        type_value = " / ".join(row.types)
        default_value = " / ".join(row.defaults) if row.defaults else "-"
        description = row.description or "-"
        lines.append(
            "| "
            + " | ".join(
                [
                    markdown_cell(markdown_code(row.path)),
                    markdown_cell(markdown_code(type_value)),
                    markdown_cell(markdown_code(default_value)),
                    markdown_cell(description),
                ]
            )
            + " |"
        )
    lines.append("")
    return "\n".join(lines)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate a Markdown reference from Codex's config JSON Schema."
    )
    parser.add_argument(
        "--schema",
        type=Path,
        default=DEFAULT_SCHEMA,
        help=f"JSON Schema to read (default: {DEFAULT_SCHEMA}).",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=DEFAULT_OUTPUT,
        help=f"Markdown file to write (default: {DEFAULT_OUTPUT}).",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        schema = json.loads(args.schema.read_text(encoding="utf-8"))
        if not isinstance(schema, dict):
            raise ValueError("schema root must be a JSON object")
        resolved_schema = args.schema.resolve()
        try:
            schema_label = str(resolved_schema.relative_to(REPO_ROOT))
        except ValueError:
            schema_label = str(args.schema)
        markdown = render_markdown(schema, schema_label)
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(markdown, encoding="utf-8")
    except (OSError, json.JSONDecodeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    print(f"Wrote {args.out}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
