#!/usr/bin/env python3
"""Generate a Markdown reference from Codex's config.toml JSON Schema."""

import argparse
from dataclasses import dataclass, field
import html
import json
from pathlib import Path
import sys
from typing import Any, Iterable


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_SCHEMA_PATH = REPO_ROOT / "codex-rs" / "core" / "config.schema.json"
DEFAULT_TITLE = "Codex config.toml reference"
COMBINATORS = ("allOf", "anyOf", "oneOf")
NO_DESCRIPTION = "_No description available._"


JsonObject = dict[str, Any]
ConfigPath = tuple[str, ...]


@dataclass
class ReferenceEntry:
    path: ConfigPath
    schemas: list[JsonObject] = field(default_factory=list)

    def add_schema(self, schema: JsonObject) -> None:
        if not any(existing is schema for existing in self.schemas):
            self.schemas.append(schema)


def load_schema(path: Path) -> JsonObject:
    with path.open(encoding="utf-8") as schema_file:
        schema = json.load(schema_file)
    if not isinstance(schema, dict):
        raise ValueError(f"schema root must be a JSON object: {path}")
    return schema


def _reference_name(reference: str) -> str | None:
    prefix = "#/definitions/"
    if not reference.startswith(prefix):
        return None
    return reference.removeprefix(prefix)


def _definition_for(root_schema: JsonObject, reference: str) -> JsonObject | None:
    name = _reference_name(reference)
    if name is None:
        return None
    definition = root_schema.get("definitions", {}).get(name)
    return definition if isinstance(definition, dict) else None


def collect_reference_entries(root_schema: JsonObject) -> list[ReferenceEntry]:
    entries: dict[ConfigPath, ReferenceEntry] = {}

    def add_entry(path: ConfigPath, node: JsonObject) -> None:
        entries.setdefault(path, ReferenceEntry(path)).add_schema(node)

    def walk(
        node: JsonObject,
        path: ConfigPath,
        active_references: frozenset[str],
    ) -> None:
        reference = node.get("$ref")
        if isinstance(reference, str) and reference not in active_references:
            definition = _definition_for(root_schema, reference)
            if definition is not None:
                walk(definition, path, active_references | {reference})

        for combinator in COMBINATORS:
            variants = node.get(combinator, [])
            if isinstance(variants, list):
                for variant in variants:
                    if isinstance(variant, dict):
                        walk(variant, path, active_references)

        properties = node.get("properties", {})
        if isinstance(properties, dict):
            for name, child in properties.items():
                if not isinstance(name, str) or not isinstance(child, dict):
                    continue
                child_path = (*path, name)
                add_entry(child_path, child)
                walk(child, child_path, active_references)

        additional_properties = node.get("additionalProperties")
        if isinstance(additional_properties, dict):
            child_path = (*path, "<key>")
            add_entry(child_path, additional_properties)
            walk(additional_properties, child_path, active_references)

        items = node.get("items")
        if isinstance(items, dict):
            walk(items, (*path, "[]"), active_references)

    walk(root_schema, (), frozenset())
    return sorted(entries.values(), key=lambda e: tuple(map(str.casefold, e.path)))


def format_config_path(path: ConfigPath) -> str:
    return ".".join(path).replace(".[]", "[]")


def _walk_schema_fragments(
    node: JsonObject,
    root_schema: JsonObject,
    active_references: frozenset[str] = frozenset(),
) -> Iterable[JsonObject]:
    yield node

    reference = node.get("$ref")
    if isinstance(reference, str) and reference not in active_references:
        definition = _definition_for(root_schema, reference)
        if definition is not None:
            yield from _walk_schema_fragments(
                definition,
                root_schema,
                active_references | {reference},
            )

    for combinator in COMBINATORS:
        variants = node.get(combinator, [])
        if isinstance(variants, list):
            for variant in variants:
                if isinstance(variant, dict):
                    yield from _walk_schema_fragments(
                        variant,
                        root_schema,
                        active_references,
                    )


def _type_names_for_node(node: JsonObject, root_schema: JsonObject) -> list[str]:
    type_names: list[str] = []
    for fragment in _walk_schema_fragments(node, root_schema):
        declared_types = fragment.get("type")
        if isinstance(declared_types, str):
            declared_types = [declared_types]
        if not isinstance(declared_types, list):
            continue

        for declared_type in declared_types:
            if declared_type == "object":
                rendered_type = "table"
            elif declared_type == "array":
                items = fragment.get("items")
                if isinstance(items, dict):
                    item_type = format_schema_type([items], root_schema)
                    rendered_type = f"array<{item_type}>"
                else:
                    rendered_type = "array"
            elif isinstance(declared_type, str):
                rendered_type = declared_type
            else:
                continue

            if rendered_type not in type_names:
                type_names.append(rendered_type)
    return type_names


def _enum_values_for_node(node: JsonObject, root_schema: JsonObject) -> list[Any]:
    enum_values: list[Any] = []
    for fragment in _walk_schema_fragments(node, root_schema):
        values = fragment.get("enum", [])
        if not isinstance(values, list):
            continue
        for value in values:
            if value not in enum_values:
                enum_values.append(value)
    return enum_values


def _format_json_scalar(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def format_schema_type(schemas: list[JsonObject], root_schema: JsonObject) -> str:
    type_names: list[str] = []
    enum_values: list[Any] = []

    for node in schemas:
        for type_name in _type_names_for_node(node, root_schema):
            if type_name not in type_names:
                type_names.append(type_name)
        for value in _enum_values_for_node(node, root_schema):
            if value not in enum_values:
                enum_values.append(value)

    if not type_names:
        type_names.append("value")

    rendered = " or ".join(type_names)
    if enum_values and type_names == ["string"]:
        allowed_values = ", ".join(_format_json_scalar(value) for value in enum_values)
        rendered += f" ({allowed_values})"
    return rendered


def _common_description(
    node: JsonObject,
    root_schema: JsonObject,
    active_references: frozenset[str] = frozenset(),
) -> str | None:
    description = node.get("description")
    if isinstance(description, str) and description.strip():
        return description.strip()

    reference = node.get("$ref")
    if isinstance(reference, str) and reference not in active_references:
        definition = _definition_for(root_schema, reference)
        if definition is not None:
            referenced_description = _common_description(
                definition,
                root_schema,
                active_references | {reference},
            )
            if referenced_description:
                return referenced_description

    variants = node.get("allOf", [])
    if isinstance(variants, list):
        for variant in variants:
            if not isinstance(variant, dict):
                continue
            variant_description = _common_description(
                variant,
                root_schema,
                active_references,
            )
            if variant_description:
                return variant_description
    return None


def _variant_description_notes(
    node: JsonObject,
    root_schema: JsonObject,
    active_references: frozenset[str] = frozenset(),
) -> list[str]:
    notes: list[str] = []

    reference = node.get("$ref")
    if isinstance(reference, str) and reference not in active_references:
        definition = _definition_for(root_schema, reference)
        if definition is not None:
            notes.extend(
                _variant_description_notes(
                    definition,
                    root_schema,
                    active_references | {reference},
                )
            )

    for combinator in ("anyOf", "oneOf"):
        variants = node.get(combinator, [])
        if not isinstance(variants, list):
            continue
        for variant in variants:
            if not isinstance(variant, dict):
                continue
            description = _common_description(variant, root_schema, active_references)
            if not description:
                continue
            enum_values = _enum_values_for_node(variant, root_schema)
            if enum_values:
                labels = ", ".join(
                    f"`{_format_json_scalar(value)}`" for value in enum_values
                )
                note = f"{labels}: {description}"
            else:
                note = description
            if note not in notes:
                notes.append(note)
    return notes


def format_schema_description(
    schemas: list[JsonObject],
    root_schema: JsonObject,
) -> str:
    descriptions: list[str] = []
    variant_notes: list[str] = []

    for node in schemas:
        description = _common_description(node, root_schema)
        if description and description not in descriptions:
            descriptions.append(description)
        for note in _variant_description_notes(node, root_schema):
            if note not in variant_notes and note not in descriptions:
                variant_notes.append(note)

    paragraphs = descriptions
    if variant_notes:
        paragraphs.append("Options: " + " ".join(variant_notes))
    return "\n\n".join(paragraphs) if paragraphs else NO_DESCRIPTION


def _escape_table_cell(value: str) -> str:
    return value.replace("|", "\\|").replace("\r\n", "\n").replace("\n", "<br>")


def render_markdown(
    root_schema: JsonObject,
    entries: list[ReferenceEntry],
    *,
    title: str = DEFAULT_TITLE,
) -> str:
    lines = [
        f"# {title}",
        "",
        "<!-- Generated by scripts/generate_config_reference.py. Do not edit manually. -->",
        "",
        "Generated from `codex-rs/core/config.schema.json`. Dynamic table keys are shown as "
        "`<key>`, and `[]` identifies fields inside an array of tables.",
        "",
        "| Configuration | Type | Description |",
        "| --- | --- | --- |",
    ]

    for entry in entries:
        path = _escape_table_cell(format_config_path(entry.path))
        type_name = html.escape(format_schema_type(entry.schemas, root_schema))
        description = _escape_table_cell(
            format_schema_description(entry.schemas, root_schema)
        )
        lines.append(f"| `{path}` | <code>{type_name}</code> | {description} |")

    lines.append("")
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--schema",
        type=Path,
        default=DEFAULT_SCHEMA_PATH,
        help=f"JSON Schema to read (default: {DEFAULT_SCHEMA_PATH})",
    )
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        help="Markdown file to write. Omit to write to stdout.",
    )
    parser.add_argument(
        "--title",
        default=DEFAULT_TITLE,
        help=f"Top-level Markdown heading (default: {DEFAULT_TITLE!r}).",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        schema = load_schema(args.schema)
        entries = collect_reference_entries(schema)
        markdown = render_markdown(schema, entries, title=args.title)
        if args.output is None:
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
