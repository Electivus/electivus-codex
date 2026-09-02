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
#[my_ignore] fn ordinary_identifier() {}
#[ignore = "manual run"] fn direct_test() {}
#[cfg_attr(target_os = "windows", ignore = "Linux-only fixture")]
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
            (
                '#[ignore = r"reason"]\nfn direct() {}',
                policy.IgnoreOccurrence("src/lib.rs", "direct", 'ignore=r"reason"'),
            ),
            (
                '#[cfg_attr(windows, ignore = r#"reason"#)]\nfn conditional() {}',
                policy.IgnoreOccurrence("src/lib.rs", "conditional", 'cfg_attr(windows,ignore=r#"reason"#)'),
            ),
        )
        for source, expected in cases:
            with self.subTest(expected=expected):
                occurrences = policy.inventory_file("src/lib.rs", source)
                self.assertEqual([expected], occurrences)
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
        records = {occurrences[0].identity: "manual-smoke", inherited.identity: "manual-smoke", "src/lib.rs::stale::ignore": "made-up"}
        issues = policy.validate_ignore_policy(occurrences, {"ignores": records})
        joined = "\n".join(issues)
        for expected in ("unclassified", "stale", "unknown category", "must be temporary"):
            self.assertIn(expected, joined)

    def test_every_legitimate_category_is_accepted(self) -> None:
        occurrences: list[policy.IgnoreOccurrence] = []
        records: dict[str, str] = {}
        for index, category in enumerate(sorted(policy.ALLOWED_CATEGORIES)):
            tests = sorted(policy.TEMPORARY_CERTIFICATION_TESTS) if category == "temporary-certification" else [f"test_{index}"]
            for test in tests:
                occurrence = policy.IgnoreOccurrence(f"src/category_{index}.rs", test, 'ignore="classified"')
                occurrences.append(occurrence)
                records[occurrence.identity] = category + (
                    "|https://github.com/Electivus/electivus-codex/issues/89"
                    if category == "temporary-certification"
                    else ""
                )
        self.assertEqual([], policy.validate_ignore_policy(occurrences, {"ignores": records}))

    def test_manifest_assignment_is_authoritative_over_helper_reason_text(self) -> None:
        occurrence = policy.IgnoreOccurrence("src/new.rs", "ordinary_test", 'ignore="spawned by helper process fixture"')
        self.assertEqual(
            [],
            policy.validate_ignore_policy([occurrence], {"ignores": {occurrence.identity: "specialized-environment"}}),
        )
        self.assertIn("unclassified", "\n".join(policy.validate_ignore_policy([occurrence], {"ignores": {}})))

    def test_pending_upstream_ignore_classifies_absent_and_imported_test(self) -> None:
        occurrence = policy.IgnoreOccurrence(
            "src/upstream.rs", "helper_child", 'ignore="invoked by parent"'
        )
        manifest = {
            "ignores": {},
            "pending_upstream_ignores": {
                occurrence.identity: "helper-process|https://github.com/Electivus/electivus-codex/pull/207"
            },
        }
        self.assertEqual([], policy.validate_ignore_policy([], manifest))
        self.assertEqual([], policy.validate_ignore_policy([occurrence], manifest))

    def test_pending_upstream_ignore_requires_tracking_and_unique_identity(self) -> None:
        occurrence = policy.IgnoreOccurrence(
            "src/upstream.rs", "helper_child", 'ignore="invoked by parent"'
        )
        manifest = {
            "ignores": {occurrence.identity: "helper-process"},
            "pending_upstream_ignores": {
                occurrence.identity: "made-up|not-a-tracking-url",
                "src/future.rs::future::ignore": "temporary-certification|https://github.com/Electivus/electivus-codex/issues/208",
            },
        }
        issues = "\n".join(policy.validate_ignore_policy([occurrence], manifest))
        for expected in (
            "duplicate active and pending",
            "unknown pending upstream category",
            "requires a GitHub issue or pull request URL",
            "cannot be temporary-certification",
        ):
            self.assertIn(expected, issues)


class QuarantinePolicyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.repo = Path(self.temp_dir.name)
        self.workflows = self.repo / ".github/workflows"
        self.workflows.mkdir(parents=True)
        self.write_workflow(
            "extended.yml",
            "jobs:\n  rust-tests:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n",
        )

    def write_workflow(self, name: str, body: str) -> None:
        (self.workflows / name).write_text(
            f"name: Extended\non: workflow_call\n{body}", encoding="utf-8"
        )

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

    def test_extended_workflow_subset_fails_closed(self) -> None:
        self.assertEqual([], self.validate([self.record()]))
        self.write_workflow(
            "valid.yml",
            "jobs:\n  rust-tests:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n  reusable:\n    uses: ./.github/workflows/reusable.yml\n",
        )
        self.assertEqual(({"reusable", "rust-tests"}, []), policy._workflow_jobs(self.repo, "valid.yml"))
        self.assertEqual(
            (set(), ["extended workflow does not exist: missing.yml"]),
            policy._workflow_jobs(self.repo, "missing.yml"),
        )
        invalid = (
            ("empty-runs-on", "  broken:\n    runs-on: []\n    steps:\n      - run: echo ok\n", '"runs-on" section should not be empty [syntax-check]'),
            ("duplicate-runs-on", "  broken:\n    runs-on: ubuntu-latest\n    runs-on:\n    steps:\n      - run: echo ok\n", 'key "runs-on" is duplicated in "broken" job. previously defined at line:5,col:5 [syntax-check]'),
            ("missing-steps", "  broken:\n    runs-on: ubuntu-latest\n", '"steps" section is missing in job "broken" [syntax-check]'),
            ("malformed-quote", '  broken:\n    runs-on: "ubuntu" garbage\n    steps:\n      - run: echo ok\n', "could not parse as YAML: yaml: line 4: did not find expected key [syntax-check]"),
        )
        for name, body, diagnostic in invalid:
            with self.subTest(name=name):
                self.write_workflow(f"{name}.yml", f"jobs:\n{body}")
                expected = (
                    set(),
                    [f"actionlint rejected extended workflow {name}.yml: {diagnostic}"],
                )
                self.assertEqual(expected, policy._workflow_jobs(self.repo, f"{name}.yml"))

        missing_actionlint = "/definitely/missing/actionlint"
        self.assertEqual(
            (set(), [f"actionlint executable not found: {missing_actionlint}"]),
            policy._workflow_jobs(self.repo, "valid.yml", actionlint=missing_actionlint),
        )

        record = self.record() | {"extended_job": "missing-job"}
        self.assertEqual(
            ["quarantine record 1 extended_job does not exist in extended.yml: missing-job"],
            self.validate([record]),
        )

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
        first = self.record() | {"expiry_date": dt.date(2026, 7, 30)}
        duplicate_issues = "\n".join(self.validate([first, self.record()]))
        for expected in ("duplicate", "precedes"):
            self.assertIn(expected, duplicate_issues)


if __name__ == "__main__":
    unittest.main()
