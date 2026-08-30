from pathlib import Path
import unittest

import check_codeql_policy as policy


class CodeqlPolicyTests(unittest.TestCase):
    def test_active_workflows_do_not_grant_codeql_authority(self) -> None:
        root = Path(__file__).resolve().parents[2]
        sources = {
            str(path): path.read_text(encoding="utf-8")
            for path in (root / ".github/workflows").glob("*.y*ml")
        }
        self.assertEqual([], policy.validate_workflows(sources))

    def test_codeql_action_is_rejected(self) -> None:
        issues = policy.validate_workflows(
            {"workflow.yml": "uses: github/codeql-action/analyze@deadbeef"}
        )
        self.assertIn("workflow.yml reintroduces the CodeQL action", issues)

    def test_codeql_job_and_security_events_write_are_rejected(self) -> None:
        issues = policy.validate_workflows(
            {
                "workflow.yml": """jobs:
  codeql:
    permissions:
      security-events: write
"""
            }
        )
        self.assertEqual(
            [
                "workflow.yml reintroduces a CodeQL job or step",
                "workflow.yml grants code-scanning write authority",
            ],
            issues,
        )


if __name__ == "__main__":
    unittest.main()
