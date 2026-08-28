import json
import unittest
from dataclasses import replace
from pathlib import Path

from upstream_sync_manifest import (
    MAX_CONFLICTS_SHOWN,
    MAX_PULL_REQUEST_BODY_CHARACTERS,
    MAX_RENDERED_CONFLICT_BYTES,
    RELEASE_URL_PREFIX,
    ReleaseIdentity,
    SynchronizationManifest,
    bounded_conflict_paths,
    canonical_release_url,
    manifest_path,
    parse_manifest,
    render_pull_request_body,
    serialize_manifest,
    validate_chain,
)

RELEASE = "a" * 40
FORK = "b" * 40
SEED_COMMIT = "b3a6d7f67cf056e18472c2b9ec26d3999ed40b7b"
DELETE = object()


def make_manifest(
    *,
    commit: str = RELEASE,
    tag: str = "rust-v1.2.3",
    previous: str | None = None,
    selection_mode: str = "automatic",
    preparation_mode: str = "clean",
    conflict_paths: tuple[str, ...] = (),
) -> SynchronizationManifest:
    return SynchronizationManifest(
        schema_version=1,
        release=ReleaseIdentity(tag, commit, canonical_release_url(tag)),
        fork_base_sha=FORK,
        previous_release_commit=previous,
        selection_mode=selection_mode,
        preparation_mode=preparation_mode,
        conflict_paths=conflict_paths,
    )


def load_seed() -> tuple[SynchronizationManifest, str, Path]:
    repository = Path(__file__).parents[2]
    seed_path = repository / manifest_path(SEED_COMMIT)
    seed_text = seed_path.read_text(encoding="utf-8")
    return parse_manifest(seed_text), seed_text, seed_path


class UpstreamSyncManifestTest(unittest.TestCase):
    def assert_payload_mutations_rejected(self, canonical, mutations) -> None:
        for name, paths, values, diagnostic in mutations:
            with self.subTest(name=name):
                payload = json.loads(canonical)
                if isinstance(paths, str):
                    changes = ((paths, values),)
                else:
                    changes = zip(paths, values, strict=True)
                for path, value in changes:
                    target = payload
                    *parents, key = path.split(".")
                    for component in parents:
                        target = target[component]
                    if value is DELETE:
                        del target[key]
                    else:
                        target[key] = value
                document = json.dumps(payload, indent=2) + "\n"
                with self.assertRaisesRegex(ValueError, diagnostic):
                    parse_manifest(document)

    def assert_manifest_mutations_rejected(self, manifest, mutations) -> None:
        for name, changes, diagnostic in mutations:
            with self.subTest(name=name):
                with self.assertRaisesRegex(ValueError, diagnostic):
                    serialize_manifest(replace(manifest, **changes))

    def test_manifest_round_trip_uses_one_canonical_serialization(self) -> None:
        expected = make_manifest(previous="c" * 40, selection_mode="manual")
        canonical = serialize_manifest(expected)
        parsed = parse_manifest(canonical)

        self.assertEqual(parsed, expected)
        self.assertEqual(serialize_manifest(parsed), canonical)
        with self.assertRaisesRegex(ValueError, "canonically"):
            parse_manifest(json.dumps(json.loads(canonical)) + "\n")

    def test_parser_rejects_invalid_documents_for_the_expected_reason(self) -> None:
        canonical = serialize_manifest(make_manifest())
        manifest_shape = "manifest must contain exactly"
        release_shape = "release must contain exactly"
        schema_error = "unsupported Synchronization manifest schemaVersion"
        sha_error = "must be a lowercase 40-character SHA"
        invalid_tag = "rust-v1.2.03"
        invalid_tag_url = f"{RELEASE_URL_PREFIX}{invalid_tag}"
        array_error = "conflictPaths must be an array"
        release_sha_error = rf"release\.commit {sha_error}"
        fork_sha_error = f"forkBaseSha {sha_error}"
        predecessor_error = f"previousReleaseCommit {sha_error}"
        tag_error = r"release tag must be an exact rust-v<SemVer> tag"
        url_error = "release URL is not canonical"
        invalid_tag_fields = ("release.tag", "release.url")
        invalid_tag_values = (invalid_tag, invalid_tag_url)
        bad_url = "https://example.test/release"
        preparation_error = "invalid preparationMode"
        mutations = (
            ("unknown field", "extra", True, manifest_shape),
            ("missing field", "forkBaseSha", DELETE, manifest_shape),
            ("unknown release field", "release.extra", True, release_shape),
            ("release is not an object", "release", [], release_shape),
            ("conflicts are not an array", "conflictPaths", {}, array_error),
            ("schema type", "schemaVersion", True, schema_error),
            ("schema version", "schemaVersion", 2, schema_error),
            ("uppercase release sha", "release.commit", "A" * 40, release_sha_error),
            ("short fork sha", "forkBaseSha", "b" * 39, fork_sha_error),
            ("invalid predecessor", "previousReleaseCommit", 7, predecessor_error),
            (
                "invalid tag",
                invalid_tag_fields,
                invalid_tag_values,
                tag_error,
            ),
            ("noncanonical url", "release.url", bad_url, url_error),
            ("selection enum", "selectionMode", "scheduled", "invalid selectionMode"),
            ("preparation enum", "preparationMode", "merged", preparation_error),
        )
        self.assert_payload_mutations_rejected(canonical, mutations)

        duplicate_top_level = canonical.replace(
            '  "schemaVersion": 1,',
            '  "schemaVersion": 1,\n  "schemaVersion": 1,',
        )
        duplicate_release = canonical.replace(
            '    "tag": "rust-v1.2.3",',
            '    "tag": "rust-v1.2.3",\n    "tag": "rust-v1.2.3",',
        )
        nan_document = canonical.replace('"schemaVersion": 1', '"schemaVersion": NaN')
        invalid_documents = (
            (duplicate_top_level, "duplicate field: schemaVersion"),
            (duplicate_release, "duplicate field: tag"),
            ("[]\n", manifest_shape),
            (nan_document, "invalid Synchronization manifest JSON constant: NaN"),
        )
        for document, diagnostic in invalid_documents:
            with self.subTest(diagnostic=diagnostic):
                with self.assertRaisesRegex(ValueError, diagnostic):
                    parse_manifest(document)

    def test_release_tags_follow_semver_and_determine_the_canonical_url(self) -> None:
        for tag, selection_mode in (
            ("rust-v0.0.0", "automatic"),
            ("rust-v1.2.3-alpha.1", "manual"),
            ("rust-v1.2.3-alpha+build.01", "manual"),
        ):
            with self.subTest(tag=tag):
                manifest = make_manifest(tag=tag, selection_mode=selection_mode)
                self.assertEqual(parse_manifest(serialize_manifest(manifest)), manifest)

        invalid_tags = ("sdk-v1.2.3", "rust-v1.2", "rust-v01.2.3", "rust-v1.2.3-01")
        invalid_tags += (
            "rust-v1.2.3-alpha..1",
            "rust-v1.2.3+",
            "rust-v1.2.3\N{ARABIC-INDIC DIGIT ONE}",
        )
        for tag in invalid_tags:
            with self.subTest(tag=tag):
                with self.assertRaisesRegex(ValueError, r"exact rust-v<SemVer> tag"):
                    canonical_release_url(tag)

    def test_automatic_selection_rejects_non_seed_prereleases(self) -> None:
        automatic = make_manifest(tag="rust-v9.9.9-alpha.1")

        with self.assertRaisesRegex(ValueError, "stable release"):
            serialize_manifest(automatic)
        manual = replace(automatic, selection_mode="manual")
        self.assertEqual(parse_manifest(serialize_manifest(manual)), manual)

    def test_conflict_paths_are_valid_sorted_unique_and_match_preparation(self) -> None:
        manifest = make_manifest(
            preparation_mode="conflicting",
            conflict_paths=(".github/workflow.yml", "codex-rs/core/src/lib.rs"),
        )
        self.assertEqual(parse_manifest(serialize_manifest(manifest)), manifest)
        utf8_boundary = replace(manifest, conflict_paths=("é" * 2048,))
        self.assertEqual(
            parse_manifest(serialize_manifest(utf8_boundary)), utf8_boundary
        )

        invalid_paths = ("", ".", "../outside", "/absolute", "a/./b", "a/../b", "a//b")
        invalid_paths += ("nul\0path", "é" * 2049, "\ud800")
        invalid_path_diagnostic = "conflictPaths contains an invalid repository path"
        ordering_diagnostic = "conflictPaths must be sorted and unique"
        disagreement = "preparationMode and conflictPaths disagree"
        mutations = (
            *(
                (f"path={path!r}", {"conflict_paths": (path,)}, invalid_path_diagnostic)
                for path in invalid_paths
            ),
            *(
                (
                    f"conflicts={conflicts!r}",
                    {"conflict_paths": conflicts},
                    ordering_diagnostic,
                )
                for conflicts in (("z", "a"), ("a", "a"))
            ),
            ("clean with conflicts", {"preparation_mode": "clean"}, disagreement),
            (
                "conflicting without conflicts",
                {"preparation_mode": "conflicting", "conflict_paths": ()},
                disagreement,
            ),
        )
        self.assert_manifest_mutations_rejected(manifest, mutations)

    def test_manifest_and_rendered_conflicts_have_aggregate_budgets(self) -> None:
        oversized_manifest = make_manifest(
            preparation_mode="conflicting",
            conflict_paths=tuple(
                f"{index:03}-{'x' * 100}.txt" for index in range(100)
            ),
        )
        with self.assertRaisesRegex(ValueError, "manifest exceeds its byte budget"):
            serialize_manifest(oversized_manifest)

        expanded_path = "é" * 2048
        displayed = bounded_conflict_paths((expanded_path, "z.txt"))
        encoded = json.dumps(displayed, ensure_ascii=True).encode("ascii")
        self.assertEqual(displayed, ("z.txt",))
        self.assertLessEqual(len(encoded), MAX_RENDERED_CONFLICT_BYTES)

    def test_pr153_seed_is_canonical_and_checkout_independent(self) -> None:
        seed, seed_text, seed_path = load_seed()
        repository = Path(__file__).parents[2]
        seed_tag = "rust-v0.150.0-alpha.5"

        expected = SynchronizationManifest(
            1,
            ReleaseIdentity(seed_tag, SEED_COMMIT, canonical_release_url(seed_tag)),
            "da655a4b51761edaa429fcad912e6ac3e17e32ee",
            None,
            "automatic",
            "conflicting",
            tuple(json.loads(seed_text)["conflictPaths"]),
        )
        self.assertEqual(seed, expected)
        self.assertEqual(len(seed.conflict_paths), 48)
        self.assertEqual(
            manifest_path(seed.release.commit),
            seed_path.relative_to(repository).as_posix(),
        )
        self.assertEqual(serialize_manifest(seed), seed_text)
        stable_tag = "rust-v0.150.0"
        stable_release = ReleaseIdentity(
            stable_tag, SEED_COMMIT, canonical_release_url(stable_tag)
        )
        seed_diagnostic = "PR #153 seed manifest"
        self.assert_manifest_mutations_rejected(
            seed,
            (
                ("selection mode", {"selection_mode": "manual"}, seed_diagnostic),
                ("tag and URL", {"release": stable_release}, seed_diagnostic),
                ("fork", {"fork_base_sha": "d" * 40}, seed_diagnostic),
                (
                    "conflicts",
                    {"conflict_paths": seed.conflict_paths[:-1]},
                    seed_diagnostic,
                ),
                (
                    "multiple fields",
                    {
                        "release": stable_release,
                        "fork_base_sha": "d" * 40,
                        "previous_release_commit": "e" * 40,
                        "selection_mode": "manual",
                    },
                    seed_diagnostic,
                ),
            ),
        )

    def test_chain_accepts_out_of_order_input_and_returns_the_unique_tip(self) -> None:
        seed, _, _ = load_seed()
        second = make_manifest(
            commit="c" * 40,
            tag="rust-v1.2.4",
            previous=SEED_COMMIT,
        )
        tip = make_manifest(
            commit="d" * 40,
            tag="rust-v1.2.5",
            previous=second.release.commit,
            selection_mode="manual",
            preparation_mode="conflicting",
            conflict_paths=("codex-rs/core/src/lib.rs",),
        )

        self.assertEqual(validate_chain((tip, seed, second)), tip)
        self.assertEqual(validate_chain((seed, second, tip)), tip)

    def test_chain_rejects_every_non_linear_or_noncanonical_shape(self) -> None:
        seed, _, _ = load_seed()
        second = make_manifest(
            commit="c" * 40,
            tag="rust-v1.2.4",
            previous=SEED_COMMIT,
        )
        duplicate_commit = make_manifest(
            commit=second.release.commit,
            tag="rust-v1.2.9",
            previous=SEED_COMMIT,
        )
        duplicate_tag = make_manifest(
            commit="d" * 40,
            tag=second.release.tag,
            previous=second.release.commit,
        )
        fork = make_manifest(
            commit="e" * 40,
            tag="rust-v1.2.6",
            previous=SEED_COMMIT,
        )
        cycle_left = make_manifest(
            commit="e" * 40,
            tag="rust-v1.2.6",
            previous="f" * 40,
        )
        cycle_right = make_manifest(
            commit="f" * 40,
            tag="rust-v1.2.7",
            previous=cycle_left.release.commit,
        )
        disconnected_root = make_manifest(
            commit="e" * 40,
            tag="rust-v1.2.6",
        )
        missing_link = replace(second, previous_release_commit="e" * 40)
        invalid_chains = (
            ("empty", (), "must not be empty"),
            ("wrong root", (make_manifest(),), "rooted at the PR #153 seed"),
            (
                "duplicate commit",
                (seed, second, duplicate_commit),
                "duplicate release commit",
            ),
            (
                "duplicate tag",
                (seed, second, duplicate_tag),
                "duplicate release tag",
            ),
            ("missing link", (seed, missing_link), "predecessor .* is missing"),
            ("fork and multiple tips", (seed, second, fork), "forks.*multiple tips"),
            ("cycle", (seed, cycle_left, cycle_right), "contains a cycle"),
            (
                "disconnected roots",
                (seed, disconnected_root),
                "exactly one root.*disconnected components",
            ),
        )

        for name, manifests, diagnostic in invalid_chains:
            with self.subTest(name=name):
                with self.assertRaisesRegex(ValueError, diagnostic):
                    validate_chain(manifests)

    def test_clean_pull_request_body_is_a_pure_manifest_rendering(self) -> None:
        manifest = make_manifest(previous=SEED_COMMIT)
        expected = f"""\
Synchronizes the published Codex CLI release [rust-v1.2.3]({manifest.release.url}).

- Release SHA (`release.commit`): `{manifest.release.commit}`
- Fork baseline (`forkBaseSha`): `{manifest.fork_base_sha}`
- Predecessor (`previousReleaseCommit`): `{SEED_COMMIT}`
- Selection (`selectionMode`): `automatic`
- Preparation (`preparationMode`): `clean`
- Manifest: `{manifest_path(manifest.release.commit)}`

Next action: Review the Baseline reconciliation and approve its workflow runs.
"""

        self.assertEqual(render_pull_request_body(manifest), expected)
        self.assertEqual(render_pull_request_body(manifest), expected)

    def test_pull_request_body_requires_a_predecessor_except_for_the_seed(self) -> None:
        with self.assertRaisesRegex(ValueError, "non-seed.*requires a predecessor"):
            render_pull_request_body(make_manifest())

        seed, _, _ = load_seed()
        seed_body = render_pull_request_body(seed)
        self.assertIn(
            "- Predecessor (`previousReleaseCommit`): `none (PR #153 seed)`",
            seed_body,
        )

    def test_conflicting_pull_request_body_has_a_coherent_next_action(self) -> None:
        manifest = make_manifest(
            previous=SEED_COMMIT,
            selection_mode="manual",
            preparation_mode="conflicting",
            conflict_paths=("docs/a.md", "src/b.rs"),
        )
        expected = f"""\
Synchronizes the published Codex CLI release [rust-v1.2.3]({manifest.release.url}).

- Release SHA (`release.commit`): `{manifest.release.commit}`
- Fork baseline (`forkBaseSha`): `{manifest.fork_base_sha}`
- Predecessor (`previousReleaseCommit`): `{SEED_COMMIT}`
- Selection (`selectionMode`): `manual`
- Preparation (`preparationMode`): `conflicting`
- Manifest: `{manifest_path(manifest.release.commit)}`

Next action: Perform explicit Semantic reconciliation, then mark this PR ready for review.

Conflicts (2 total; showing 2):

    "docs/a.md"
    "src/b.rs"

Omitted conflicts: 0. The complete conflict evidence is in `{manifest_path(manifest.release.commit)}`.
"""

        self.assertEqual(render_pull_request_body(manifest), expected)

    def test_pull_request_body_shows_at_most_twenty_conflicts_and_exact_total(
        self,
    ) -> None:
        conflicts = tuple(f"conflict-{index:02}.txt" for index in range(22))
        manifest = make_manifest(
            previous=SEED_COMMIT,
            preparation_mode="conflicting",
            conflict_paths=conflicts,
        )

        body = render_pull_request_body(manifest)

        self.assertIn("Conflicts (22 total; showing 20)", body)
        self.assertIn('    "conflict-19.txt"', body)
        self.assertNotIn("conflict-20.txt", body)
        self.assertIn("Omitted conflicts: 2", body)
        self.assertEqual(body.count('\n    "conflict-'), MAX_CONFLICTS_SHOWN)

    def test_pull_request_body_escapes_paths_without_markdown_injection(self) -> None:
        path = "docs/safe` **markdown**\n# injected-☃.md"
        manifest = make_manifest(
            previous=SEED_COMMIT,
            preparation_mode="conflicting",
            conflict_paths=(path,),
        )

        body = render_pull_request_body(manifest)
        encoded_lines = [
            line.strip() for line in body.splitlines() if line.startswith("    ")
        ]

        self.assertEqual([json.loads(line) for line in encoded_lines], [path])
        self.assertNotIn("\n# injected", body)
        self.assertNotIn("☃", body)
        self.assertIn(r"\n# injected-\u2603.md", body)
        self.assertEqual(render_pull_request_body(manifest), body)

    def test_pull_request_body_budget_omits_only_complete_long_paths(self) -> None:
        paths = tuple(f"{index:02}-" + "x" * 4_080 for index in range(20))
        manifest = make_manifest(
            previous=SEED_COMMIT,
            preparation_mode="conflicting",
            conflict_paths=paths,
        )

        body = render_pull_request_body(manifest)
        encoded_lines = [
            line.strip() for line in body.splitlines() if line.startswith("    ")
        ]
        displayed = [json.loads(line) for line in encoded_lines]

        self.assertLessEqual(len(body), MAX_PULL_REQUEST_BODY_CHARACTERS)
        self.assertGreater(len(displayed), 0)
        self.assertLess(len(displayed), len(paths))
        self.assertEqual(displayed, list(paths[: len(displayed)]))
        self.assertIn(f"Omitted conflicts: {len(paths) - len(displayed)}", body)

    def test_pull_request_body_skips_an_expanded_path_that_does_not_fit(self) -> None:
        conflicts = ("\x01" * 4_096, "z.txt")
        manifest = make_manifest(
            previous=SEED_COMMIT,
            preparation_mode="conflicting",
            conflict_paths=conflicts,
        )

        body = render_pull_request_body(manifest)
        encoded_lines = [
            line.strip() for line in body.splitlines() if line.startswith("    ")
        ]

        self.assertEqual([json.loads(line) for line in encoded_lines], ["z.txt"])
        self.assertIn("Conflicts (2 total; showing 1)", body)
        self.assertIn("Omitted conflicts: 1", body)
        self.assertLessEqual(len(body), MAX_PULL_REQUEST_BODY_CHARACTERS)

    def test_pull_request_body_fails_closed_when_no_complete_path_fits(self) -> None:
        path = ("☃" * 1_364) + ".md"
        manifest = make_manifest(
            previous=SEED_COMMIT,
            preparation_mode="conflicting",
            conflict_paths=(path,),
        )

        with self.assertRaisesRegex(ValueError, "complete conflict path"):
            render_pull_request_body(manifest)

    def test_pull_request_body_fails_closed_when_fixed_metadata_exceeds_budget(
        self,
    ) -> None:
        tag = f"rust-v1.2.{('9' * MAX_PULL_REQUEST_BODY_CHARACTERS)}"
        manifest = make_manifest(
            tag=tag,
            previous=SEED_COMMIT,
            selection_mode="manual",
        )

        with self.assertRaisesRegex(ValueError, "metadata exceeds"):
            render_pull_request_body(manifest)


if __name__ == "__main__":
    unittest.main()
