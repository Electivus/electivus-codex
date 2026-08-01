import contextlib
import datetime as dt
import io
from pathlib import Path
import subprocess
import tempfile
import unittest

import check_rust_test_policy


class RustTestPolicyCliTests(unittest.TestCase):
    def test_unclassified_ignore_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = Path(temp_dir)
            (repo / "src").mkdir()
            (repo / "src/lib.rs").write_text(
                "#[test]\n#[ignore]\nfn needs_classification() {}\n", encoding="utf-8"
            )
            (repo / "policy.toml").write_text("version = 1\n", encoding="utf-8")
            (repo / "quarantines.toml").write_text(
                "version = 1\nquarantines = []\n", encoding="utf-8"
            )
            subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
            subprocess.run(["git", "add", "src/lib.rs"], cwd=repo, check=True)
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                result = check_rust_test_policy.main(
                    [
                        "--repo",
                        str(repo),
                        "--policy",
                        str(repo / "policy.toml"),
                        "--quarantines",
                        str(repo / "quarantines.toml"),
                        "--today",
                        "2026-07-31",
                    ]
                )

        self.assertEqual(1, result)
        self.assertIn("src/lib.rs", output.getvalue())
        self.assertIn("needs_classification", output.getvalue())
        self.assertIn("unclassified", output.getvalue())

    def test_inventory_finds_direct_and_multiline_cfg_attr_but_not_text(self) -> None:
        source = r"""
// #[ignore]
/* #[cfg_attr(windows, ignore)] */
const EXAMPLE: &str = "#[ignore]";
const RAW: &str = r#"#[ignore = "not code"]"#;
#[my_ignore]
fn ordinary_identifier() {}

#[test]
#[ignore = "manual run"]
fn direct_test() {}

#[test]
#[cfg_attr(
    target_os = "windows",
    ignore = "Linux-only fixture",
)]
async fn conditional_test() {}
"""

        occurrences = check_rust_test_policy.inventory_file("src/lib.rs", source)

        self.assertEqual(
            [
                check_rust_test_policy.IgnoreOccurrence(
                    "src/lib.rs", "direct_test", 'ignore="manual run"'
                ),
                check_rust_test_policy.IgnoreOccurrence(
                    "src/lib.rs",
                    "conditional_test",
                    'cfg_attr(target_os="windows",ignore="Linux-only fixture")',
                ),
            ],
            occurrences,
        )

    def test_second_ignore_on_classified_test_is_unclassified(self) -> None:
        occurrences = check_rust_test_policy.inventory_file(
            "src/lib.rs",
            '#[test]\n#[ignore]\n#[ignore = "second"]\nfn duplicated() {}\n',
        )
        policy = {
            "ignore": [
                {
                    "path": "src/lib.rs",
                    "test": "duplicated",
                    "attribute": "ignore",
                    "category": "manual-smoke",
                }
            ]
        }

        issues = check_rust_test_policy.validate_ignore_policy(occurrences, policy)

        self.assertTrue(
            any("unclassified" in issue and "second" in issue for issue in issues)
        )

    def test_every_legitimate_ignore_category_is_accepted(self) -> None:
        categories = sorted(check_rust_test_policy.ALLOWED_CATEGORIES)
        occurrences = []
        records = []
        for index, category in enumerate(categories):
            if category == "temporary-certification":
                tests = sorted(check_rust_test_policy.TEMPORARY_CERTIFICATION_TESTS)
            else:
                tests = [f"test_{index}"]
            for test in tests:
                occurrence = check_rust_test_policy.IgnoreOccurrence(
                    f"src/category_{index}.rs", test, 'ignore="classified"'
                )
                occurrences.append(occurrence)
                record = {
                    "path": occurrence.path,
                    "test": occurrence.test,
                    "attribute": occurrence.attribute,
                    "category": category,
                }
                if category == "temporary-certification":
                    record["tracking"] = (
                        "https://github.com/Electivus/electivus-codex/issues/89"
                    )
                records.append(record)

        issues = check_rust_test_policy.validate_ignore_policy(
            occurrences, {"ignore": records}
        )

        self.assertEqual([], issues)

    def test_changed_stale_unknown_and_wrong_temporary_categories_fail(self) -> None:
        occurrences = [
            check_rust_test_policy.IgnoreOccurrence("src/lib.rs", "current", "ignore")
        ]
        policy = {
            "ignore": [
                {
                    "path": "src/lib.rs",
                    "test": "old_name",
                    "attribute": "ignore",
                    "category": "made-up",
                },
                {
                    "path": "src/lib.rs",
                    "test": "injected_user_input_triggers_follow_up_request_with_deltas",
                    "attribute": "ignore",
                    "category": "manual-smoke",
                },
                {
                    "path": "src/lib.rs",
                    "test": "not_inherited_flaky",
                    "attribute": "ignore",
                    "category": "temporary-certification",
                    "tracking": "https://github.com/Electivus/electivus-codex/issues/89",
                },
            ]
        }

        issues = check_rust_test_policy.validate_ignore_policy(occurrences, policy)

        self.assertTrue(any("unknown category" in issue for issue in issues))
        self.assertTrue(any("stale" in issue for issue in issues))
        self.assertTrue(any("unclassified" in issue for issue in issues))
        self.assertTrue(
            any("must use temporary-certification" in issue for issue in issues)
        )
        self.assertTrue(any("only the two" in issue for issue in issues))


class QuarantinePolicyTests(unittest.TestCase):
    def valid_record(self) -> dict[str, object]:
        return {
            "check_identity": "Deep Linux / Cargo nextest x64 / shard 2",
            "scope": "x86_64-unknown-linux-gnu nextest shard 2",
            "evidence": "Runs 1001 and 1004 failed with the same runner disconnect.",
            "justification": "Intermittent runner loss is masking otherwise actionable shard results.",
            "extended_workflow": "postmerge-ci.yml",
            "extended_job": "Full Rust / Tests x64 / shard 2",
            "tracking": "https://github.com/Electivus/electivus-codex/issues/123",
            "start_date": dt.date(2026, 7, 31),
            "expiry_date": dt.date(2026, 8, 7),
        }

    def test_complete_seven_day_quarantine_is_accepted(self) -> None:
        issues = check_rust_test_policy.validate_quarantines(
            {"quarantines": [self.valid_record()]}, dt.date(2026, 7, 31)
        )

        self.assertEqual([], issues)

    def test_quarantine_requires_auditable_narrow_unexpired_record(self) -> None:
        invalid = self.valid_record()
        invalid.update(
            {
                "check_identity": "Deep Linux *",
                "scope": "all tests",
                "evidence": "",
                "justification": "flaky",
                "extended_workflow": "",
                "extended_job": "",
                "tracking": "#123",
                "expiry_date": dt.date(2026, 8, 8),
            }
        )

        issues = check_rust_test_policy.validate_quarantines(
            {"quarantines": [invalid]}, dt.date(2026, 8, 9)
        )

        joined = "\n".join(issues)
        for expected in (
            "without wildcards",
            "narrowest affected surface",
            "nonblank evidence",
            "substantive",
            "nonblank extended_workflow",
            "nonblank extended_job",
            "exact GitHub",
            "seven-day maximum",
            "expired",
        ):
            with self.subTest(expected=expected):
                self.assertIn(expected, joined)

    def test_quarantine_identity_is_unique_and_expiry_cannot_precede_start(
        self,
    ) -> None:
        first = self.valid_record()
        first["expiry_date"] = dt.date(2026, 7, 30)
        second = self.valid_record()

        issues = check_rust_test_policy.validate_quarantines(
            {"quarantines": [first, second]}, dt.date(2026, 7, 31)
        )

        self.assertTrue(any("duplicate" in issue for issue in issues))
        self.assertTrue(any("precedes" in issue for issue in issues))


if __name__ == "__main__":
    unittest.main()
