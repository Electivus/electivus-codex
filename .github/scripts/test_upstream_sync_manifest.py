import json
import unittest
from dataclasses import replace
from pathlib import Path

from upstream_sync_manifest import ReleaseIdentity
from upstream_sync_manifest import RELEASE_URL_PREFIX
from upstream_sync_manifest import SynchronizationManifest
from upstream_sync_manifest import canonical_release_url
from upstream_sync_manifest import manifest_path
from upstream_sync_manifest import parse_manifest
from upstream_sync_manifest import serialize_manifest


RELEASE = "a" * 40
FORK = "b" * 40
DELETE = object()


def make_manifest(
    *,
    tag: str = "rust-v1.2.3",
    previous: str | None = None,
    selection_mode: str = "automatic",
    preparation_mode: str = "clean",
    conflict_paths: tuple[str, ...] = (),
) -> SynchronizationManifest:
    return SynchronizationManifest(
        schema_version=1,
        release=ReleaseIdentity(tag, RELEASE, canonical_release_url(tag)),
        fork_base_sha=FORK,
        previous_release_commit=previous,
        selection_mode=selection_mode,
        preparation_mode=preparation_mode,
        conflict_paths=conflict_paths,
    )


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

    def test_pr153_seed_is_canonical_and_checkout_independent(self) -> None:
        repository = Path(__file__).parents[2]
        seed_commit = "b3a6d7f67cf056e18472c2b9ec26d3999ed40b7b"
        seed_tag = "rust-v0.150.0-alpha.5"
        seed_path = repository / f".github/upstream-sync-manifests/{seed_commit}.json"
        seed_text = seed_path.read_text(encoding="utf-8")
        seed = parse_manifest(seed_text)

        expected = SynchronizationManifest(
            1,
            ReleaseIdentity(seed_tag, seed_commit, canonical_release_url(seed_tag)),
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
            stable_tag, seed_commit, canonical_release_url(stable_tag)
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


if __name__ == "__main__":
    unittest.main()
