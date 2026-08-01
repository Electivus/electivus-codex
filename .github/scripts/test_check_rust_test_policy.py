import datetime as dt
from pathlib import Path
import tempfile
import unittest

import check_rust_test_policy as policy


class RustTestPolicyTests(unittest.TestCase):
    def test_inventory_ignores_comments_strings_and_non_ignore_identifiers(self) -> None:
        source = r'''
// #[ignore]
/* #[cfg_attr(windows, ignore)] */
const EXAMPLE: &str = "#[ignore]";
const RAW: &str = r#"#[ignore = "not code"]"#;
#[my_ignore]
fn ordinary_identifier() {}
#[ignore = "manual run"]
fn direct_test() {}
#[cfg_attr(
    target_os = "windows",
    ignore = "Linux-only fixture",
)]
async fn conditional_test() {}
'''
        self.assertEqual(
            [
                policy.IgnoreOccurrence("src/lib.rs", "direct_test", 'ignore="manual run"'),
                policy.IgnoreOccurrence("src/lib.rs", "conditional_test", 'cfg_attr(target_os="windows",ignore="Linux-only fixture")'),
            ],
            policy.inventory_file("src/lib.rs", source),
        )

    def test_raw_string_ignore_forms_are_exact_and_unclassified(self) -> None:
        cases = (
            ('#[ignore = r"reason"]\nfn direct() {}', 'ignore=r"reason"'),
            (
                '#[cfg_attr(windows, ignore = r"reason")]\nfn conditional() {}',
                'cfg_attr(windows,ignore=r"reason")',
            ),
            ('#[ignore = r#"reason"#]\nfn hashed() {}', 'ignore=r#"reason"#'),
        )
        for source, attribute in cases:
            with self.subTest(attribute=attribute):
                occurrences = policy.inventory_file("src/lib.rs", source)
                self.assertEqual(attribute, occurrences[0].attribute)
                issues = policy.validate_ignore_policy(occurrences, {"ignores": {}})
                self.assertIn("unclassified", "\n".join(issues))

    def test_second_ignore_on_classified_test_is_unclassified(self) -> None:
        occurrences = policy.inventory_file(
            "src/lib.rs", '#[ignore]\n#[ignore = "second"]\nfn duplicated() {}\n'
        )
        inherited = policy.IgnoreOccurrence(
            "src/lib.rs", "injected_user_input_triggers_follow_up_request_with_deltas", "ignore"
        )
        occurrences.append(inherited)
        records = {
            occurrences[0].identity: "manual-smoke",
            inherited.identity: "manual-smoke",
            "src/lib.rs::stale::ignore": "made-up",
        }
        issues = policy.validate_ignore_policy(occurrences, {"ignores": records})
        joined = "\n".join(issues)
        for expected in ("unclassified", "stale", "unknown category", "must be temporary"):
            self.assertIn(expected, joined)

    def test_every_legitimate_category_is_accepted(self) -> None:
        occurrences: list[policy.IgnoreOccurrence] = []
        records: dict[str, str] = {}
        examples: dict[str, policy.IgnoreOccurrence] = {}
        for index, category in enumerate(sorted(policy.ALLOWED_CATEGORIES)):
            tests = (
                sorted(policy.TEMPORARY_CERTIFICATION_TESTS)
                if category == "temporary-certification"
                else [f"test_{index}"]
            )
            for test in tests:
                reason = "helper process fixture" if category == "helper-process" else "classified"
                occurrence = policy.IgnoreOccurrence(f"src/category_{index}.rs", test, f'ignore="{reason}"')
                occurrences.append(occurrence)
                examples[category] = occurrence
                records[occurrence.identity] = category + (
                    "|https://github.com/Electivus/electivus-codex/issues/89"
                    if category == "temporary-certification"
                    else ""
                )
        self.assertEqual([], policy.validate_ignore_policy(occurrences, {"ignores": records}))
        helper = examples["helper-process"]
        specialized = examples["specialized-environment"]
        swapped = {helper.identity: "specialized-environment", specialized.identity: "helper-process"}
        issues = "\n".join(policy.validate_ignore_policy([helper, specialized], {"ignores": swapped}))
        self.assertIn("subprocess entry point must use helper-process", issues)
        self.assertIn("helper-process requires a subprocess entry point", issues)


class QuarantinePolicyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp_dir.name)
        self.workflows = self.repo / ".github/workflows"
        self.workflows.mkdir(parents=True)
        self.write_workflow("extended.yml", "jobs:\n  rust-tests:\n    runs-on: ubuntu-latest\n")

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def write_workflow(self, name: str, body: str) -> None:
        (self.workflows / name).write_text(f"name: Extended\n{body}", encoding="utf-8")

    def record(self) -> dict[str, object]:
        return {
            "check_identity": "Deep Linux / Cargo nextest x64 / shard 2",
            "scope": "x86_64-unknown-linux-gnu nextest shard 2",
            "evidence": "Runs 1001 and 1004 failed with the same runner disconnect.",
            "justification": "Intermittent runner loss masks otherwise actionable shard results.",
            "extended_workflow": "extended.yml",
            "extended_job": "rust-tests",
            "tracking": "https://github.com/Electivus/electivus-codex/issues/123",
            "start_date": dt.date(2026, 7, 31),
            "expiry_date": dt.date(2026, 8, 7),
        }

    def validate(self, records: list[dict[str, object]], today: dt.date = dt.date(2026, 7, 31)) -> list[str]:
        return policy.validate_quarantines({"quarantines": records}, today, self.repo)

    def test_complete_seven_day_quarantine_with_real_extended_job_is_valid(self) -> None:
        self.assertEqual([], self.validate([self.record()]))

    def test_extended_workflow_and_job_references_fail_closed(self) -> None:
        self.write_workflow("malformed.yml", "jobs:\n  broken: [\n")
        cases = (
            ("missing.yml", "rust-tests", "does not exist"),
            ("extended.yml", "missing-job", "extended_job does not exist"),
            ("malformed.yml", "broken", "malformed extended workflow"),
        )
        for workflow, job, expected in cases:
            with self.subTest(workflow=workflow, job=job):
                record = self.record()
                record.update(extended_workflow=workflow, extended_job=job)
                self.assertIn(expected, "\n".join(self.validate([record])))

    def test_quarantine_requires_narrow_auditable_unexpired_fields(self) -> None:
        record = self.record()
        record.update(
            check_identity="Deep Linux *",
            scope="all tests",
            evidence="",
            justification="flaky",
            tracking="#123",
            expiry_date=dt.date(2026, 8, 8),
        )
        joined = "\n".join(self.validate([record], dt.date(2026, 8, 9)))
        for expected in (
            "without wildcards",
            "narrowest affected surface",
            "nonblank evidence",
            "substantive",
            "exact GitHub",
            "seven-day maximum",
            "expired",
        ):
            with self.subTest(expected=expected):
                self.assertIn(expected, joined)

    def test_quarantine_identity_is_unique_and_expiry_follows_start(self) -> None:
        first = self.record()
        first["expiry_date"] = dt.date(2026, 7, 30)
        joined = "\n".join(self.validate([first, self.record()]))
        self.assertIn("duplicate", joined)
        self.assertIn("precedes", joined)


if __name__ == "__main__":
    unittest.main()
