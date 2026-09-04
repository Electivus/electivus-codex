import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import generate_config_reference as config_reference
import generate_hooks_reference as hooks_reference
import generate_tool_reference as tool_reference


REFERENCE_GENERATORS = (
    ("config", config_reference, config_reference.DEFAULT_OUTPUT_PATH),
    ("hooks", hooks_reference, hooks_reference.DEFAULT_OUTPUT_PATH),
    ("tool", tool_reference, tool_reference.DEFAULT_OUTPUT_PATH),
)
PENDING_REFERENCE_POLICY_PATH = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "pending-upstream-generated-references.json"
)
GITHUB_TRACKING_RE = re.compile(
    r"https://github\.com/Electivus/electivus-codex/(?:issues|pull)/\d+"
)
SHA256_RE = re.compile(r"[0-9a-f]{64}")


class ReferenceGeneratorsTest(unittest.TestCase):
    def test_cli_defaults_stdout_and_committed_docs_stay_synchronized(self) -> None:
        with PENDING_REFERENCE_POLICY_PATH.open(encoding="utf-8") as policy_file:
            policy = json.load(policy_file)
        pending = policy.get("pending_upstream_references", {})
        self.assertIsInstance(pending, dict)
        committed_paths = {
            name: committed_path
            for name, _module, committed_path in REFERENCE_GENERATORS
        }
        self.assertEqual(set(pending) - committed_paths.keys(), set())

        for name, record in pending.items():
            with self.subTest(name=name, policy="pending"):
                self.assertIsInstance(record, dict)
                self.assertEqual(
                    set(record),
                    {"generated_sha256", "committed_sha256", "tracking"},
                )
                for field in ("generated_sha256", "committed_sha256", "tracking"):
                    self.assertIsInstance(record[field], str)
                self.assertIsNotNone(SHA256_RE.fullmatch(record["generated_sha256"]))
                self.assertIsNotNone(SHA256_RE.fullmatch(record["committed_sha256"]))
                self.assertIsNotNone(GITHUB_TRACKING_RE.fullmatch(record["tracking"]))
                self.assertEqual(
                    hashlib.sha256(committed_paths[name].read_bytes()).hexdigest(),
                    record["committed_sha256"],
                )

        for name, module, committed_path in REFERENCE_GENERATORS:
            with self.subTest(name=name):
                script_path = Path(module.__file__)
                environment = os.environ.copy()
                environment["PYTHONIOENCODING"] = "cp1252"
                stdout_result = subprocess.run(
                    [sys.executable, script_path, "--stdout"],
                    check=False,
                    capture_output=True,
                    env=environment,
                )
                self.assertEqual(stdout_result.returncode, 0, stdout_result.stderr)
                committed = committed_path.read_bytes()
                if stdout_result.stdout != committed:
                    self.assertIn(name, pending)
                    self.assertEqual(
                        hashlib.sha256(stdout_result.stdout).hexdigest(),
                        pending[name]["generated_sha256"],
                    )

                with tempfile.TemporaryDirectory() as temp_dir:
                    default_output = Path(temp_dir) / committed_path.name
                    captured_stdout = io.StringIO()
                    with (
                        mock.patch.object(
                            module, "DEFAULT_OUTPUT_PATH", default_output
                        ),
                        mock.patch.object(sys, "argv", [str(script_path)]),
                        contextlib.redirect_stdout(captured_stdout),
                    ):
                        return_code = module.main()

                    self.assertEqual(return_code, 0)
                    self.assertEqual(captured_stdout.getvalue(), "")
                    self.assertEqual(default_output.read_bytes(), stdout_result.stdout)

    def test_cli_rejects_output_with_stdout(self) -> None:
        for name, module, _committed_path in REFERENCE_GENERATORS:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temp_dir:
                result = subprocess.run(
                    [
                        sys.executable,
                        Path(module.__file__),
                        "--output",
                        Path(temp_dir) / "reference.md",
                        "--stdout",
                    ],
                    check=False,
                    capture_output=True,
                    text=True,
                )

                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(result.stdout, "")

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
        self.assertIn(f"all {len(schemas)} hook events", markdown)
        self.assertIn(f"all {len(fixture_names)} committed", markdown)
        if "Interrupt" in documented:
            self.assertIn("stricter SessionEnd and Interrupt rules", markdown)
        else:
            self.assertNotIn("SessionEnd and Interrupt rules", markdown)
        self.assertIn("SessionEnd` does not declare", markdown)
        self.assertIn("permissionDecision:allow", markdown)
        self.assertTrue(all(name in markdown for name in fixture_names))
