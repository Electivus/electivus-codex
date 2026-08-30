import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import check_codeql_disabled as policy


class DisabledCodeScanningPolicyTests(unittest.TestCase):
    def test_codeql_authority_cannot_be_reintroduced_in_root_workflows(self) -> None:
        cases = (
            (
                "CodeQL action",
                {
                    ".github/workflows/example.yml": "uses: github/codeql-action/analyze@abc\n"
                },
            ),
            (
                "CodeQL action",
                {
                    ".github/workflows/example.yml": (
                        'uses: "github/codeq\\\n  l-action/analyze@v3"\n'
                    )
                },
            ),
            (
                "CodeQL reference",
                {
                    ".github/workflows/example.yml": (
                        "run: |\n  codeq\\\n  l database analyze\n"
                    )
                },
            ),
            (
                "CodeQL reference",
                {".github/workflows/example.yml": ("run: co'de'ql database analyze\n")},
            ),
            (
                "security-events permission",
                {".github/workflows/example.yml": "security-events: write\n"},
            ),
            (
                "security-events permission",
                {
                    ".github/workflows/example.yml": (
                        "permissions: {contents: read, security-events: write}\n"
                    )
                },
            ),
            (
                "write-all permission",
                {".github/workflows/example.yml": "permissions: write-all\n"},
            ),
            (
                "code-scanning authority",
                {
                    ".github/workflows/example.yml": "required-authority: code_scanning\n"
                },
            ),
            (
                "CodeQL workflow name",
                {".github/workflows/codeql.yml": "name: analysis\n"},
            ),
            (
                "CodeQL reference",
                {
                    ".github/workflows/example.yml": (
                        f"run: {policy.POLICY_CHECK_COMMAND} && codeql database analyze\n"
                    )
                },
            ),
        )
        for expected, sources in cases:
            with self.subTest(expected=expected):
                self.assertIn(expected, "\n".join(policy.validate_workflows(sources)))

    def test_codeql_authority_cannot_be_hidden_in_local_actions(self) -> None:
        with TemporaryDirectory() as temp_dir:
            repo = Path(temp_dir)
            workflow = repo / ".github/workflows/example.yml"
            action = repo / ".github/actions/example/action.yml"
            workflow.parent.mkdir(parents=True)
            action.parent.mkdir(parents=True)
            workflow.write_text("name: example\n", encoding="utf-8")
            action.write_text(
                "runs:\n  using: composite\n  steps:\n"
                "    - uses: github/codeql-action/init@abc\n",
                encoding="utf-8",
            )

            self.assertEqual(
                ["CodeQL action: .github/actions/example/action.yml"],
                policy.validate_workflows(policy.load_automation_sources(repo)),
            )

    def test_codeql_authority_cannot_be_delegated_to_repository_scripts(
        self,
    ) -> None:
        cases = (
            (".github/scripts/analyze.py", "python3 .github/scripts/analyze.py", None),
            ("tools/analyze.py", "python3 tools/analyze.py", None),
            ("tools/analyze.bat", "tools\\analyze.bat", None),
            ("tools/analyze.cmd", "tools\\analyze.cmd", None),
            ("tools/analyze", "./tools/analyze", 0o755),
            ("justfile", "just analyze", None),
            ("tools/BUILD", "bazel run //tools:analyze", None),
            ("tools/rules.bzl", "bazel run //tools:analyze", None),
        )
        for script_path, command, mode in cases:
            with (
                self.subTest(script_path=script_path),
                TemporaryDirectory() as temp_dir,
            ):
                repo = Path(temp_dir)
                workflow = repo / ".github/workflows/example.yml"
                script = repo / script_path
                workflow.parent.mkdir(parents=True)
                script.parent.mkdir(parents=True, exist_ok=True)
                workflow.write_text(f"run: {command}\n", encoding="utf-8")
                script.write_text(
                    'subprocess.run(["codeql", "database", "analyze"])\n',
                    encoding="utf-8",
                )
                if mode is not None:
                    script.chmod(mode)

                self.assertEqual(
                    [f"CodeQL reference: {script_path}"],
                    policy.validate_workflows(policy.load_automation_sources(repo)),
                )

    def test_every_helper_test_consumer_uses_the_scripts_environment(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        helper_test_command = (
            "uv run --project scripts --python python3 just test-github-scripts"
        )
        consumers = []
        for workflow in sorted((repo / ".github/workflows").glob("*.yml")):
            source = workflow.read_text(encoding="utf-8")
            if "just test-github-scripts" not in source:
                continue
            consumers.append(workflow.name)
            self.assertIn("uses: astral-sh/setup-uv@", source)
            self.assertNotIn(
                "\n          just test-github-scripts\n",
                source,
            )
            self.assertIn(helper_test_command, source)

        self.assertTrue(consumers)

    def test_repository_checks_enforce_the_disabled_policy(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        self.assertEqual(
            [], policy.validate_workflows(policy.load_automation_sources(repo))
        )
        repo_checks = (repo / ".github/workflows/repo-checks.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(policy.POLICY_CHECK_COMMAND, repo_checks)


if __name__ == "__main__":
    unittest.main()
