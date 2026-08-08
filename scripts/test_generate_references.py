import unittest

import generate_config_reference as config_reference
import generate_hooks_reference as hooks_reference
import generate_tool_reference as tool_reference


class ReferenceGeneratorsTest(unittest.TestCase):
    def test_committed_config_schema_is_fully_reachable(self) -> None:
        schema = config_reference.load_schema(config_reference.DEFAULT_SCHEMA_PATH)
        entries = config_reference.collect_reference_entries(schema)
        paths = {config_reference.format_config_path(entry.path) for entry in entries}
        top_level = {entry.path[0] for entry in entries if len(entry.path) == 1}

        self.assertEqual(top_level, set(schema["properties"]))
        self.assertIn("mcp_servers.<key>.command", paths)
        self.assertIn(
            "| Configuration | Type | Description |",
            config_reference.render_markdown(schema, entries),
        )

    def test_real_tool_catalog_covers_core_and_extensions(self) -> None:
        tools = tool_reference.collect_tools()
        names = {tool.name for tool in tools}
        expected = set("apply_patch image_gen.imagegen memories.read web.run".split())

        self.assertGreaterEqual(len(tools), 40)
        self.assertEqual(names, set(tool_reference.AVAILABILITY))
        self.assertTrue(expected.issubset(names))
        self.assertTrue(all(tool.input_contracts for tool in tools))
        self.assertTrue(all(tool.output_contracts for tool in tools))
        memory_read = next(tool for tool in tools if tool.name == "memories.read")
        self.assertIn("path: String", memory_read.input_contracts[0])
        self.assertNotIn("authority:", memory_read.input_contracts[0])
        markdown = tool_reference.render_markdown(tools)
        self.assertIn("| Tool | Feature / availability |", markdown)
        self.assertIn("`features.image_generation`", markdown)
        self.assertIn("No active feature; model search", markdown)
        self.assertIn("Runtime-defined tool families", markdown)
        self.assertIn("Input and output contracts", markdown)
        self.assertIn("schema::output_schema_for::<ReadMemoryResponse>()", markdown)

    def test_hook_reference_covers_every_registered_event_and_schema(self) -> None:
        schemas = hooks_reference.load_hook_schemas()
        registered = set(hooks_reference.registered_hook_event_names())
        documented = {item.metadata.name for item in schemas}
        fixture_names = {
            path.name
            for path in hooks_reference.DEFAULT_SCHEMA_DIR.glob("*.schema.json")
        }
        represented = {
            path.name
            for item in schemas
            for path in (item.input_path, item.output_path)
            if path is not None
        }

        self.assertEqual(documented, registered)
        self.assertEqual(represented, fixture_names)
        self.assertTrue(all(item.input_schema is not None for item in schemas))
        missing_outputs = {
            item.metadata.name for item in schemas if item.output_schema is None
        }
        self.assertEqual(missing_outputs, {"SessionEnd"})

        pre_tool = next(item for item in schemas if item.metadata.name == "PreToolUse")
        self.assertIn(
            "hookSpecificOutput.updatedInput",
            {
                ".".join(field.path)
                for field in hooks_reference.collect_schema_fields(
                    pre_tool.output_schema
                )
            },
        )
        user_prompt = next(
            item for item in schemas if item.metadata.name == "UserPromptSubmit"
        )
        turn_id = next(
            field
            for field in hooks_reference.collect_schema_fields(user_prompt.input_schema)
            if field.path == ("turn_id",)
        )
        self.assertIn(
            "Codex extension",
            hooks_reference.format_schema_description(
                turn_id.schemas, user_prompt.input_schema
            ),
        )

        facts = hooks_reference.load_runtime_facts()
        self.assertEqual(facts.feature_key, "hooks")
        self.assertTrue(facts.feature_default_enabled)
        markdown = hooks_reference.render_markdown(
            schemas,
            facts,
            hooks_reference.configured_handler_types(),
        )
        self.assertIn("all 11 hook events", markdown)
        self.assertIn("all 21 committed", markdown)
        self.assertIn("SessionEnd` does not declare", markdown)
        self.assertIn("permissionDecision:allow", markdown)
        self.assertTrue(all(name in markdown for name in fixture_names))
