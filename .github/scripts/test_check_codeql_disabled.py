import unittest
from pathlib import Path

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
                "security-events permission",
                {".github/workflows/example.yml": "security-events: write\n"},
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

    def test_repository_checks_enforce_the_disabled_policy(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        self.assertEqual(
            [], policy.validate_workflows(policy.load_root_workflows(repo))
        )
        repo_checks = (repo / ".github/workflows/repo-checks.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(policy.POLICY_CHECK_COMMAND, repo_checks)


if __name__ == "__main__":
    unittest.main()
