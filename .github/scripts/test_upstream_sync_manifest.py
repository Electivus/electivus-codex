import json
import subprocess
import unittest
from dataclasses import replace
from pathlib import Path

from upstream_sync_manifest import ReleaseIdentity
from upstream_sync_manifest import SynchronizationManifest
from upstream_sync_manifest import MAX_PULL_REQUEST_BODY_CHARACTERS
from upstream_sync_manifest import canonical_release_url
from upstream_sync_manifest import manifest_filename
from upstream_sync_manifest import manifest_path
from upstream_sync_manifest import parse_manifest
from upstream_sync_manifest import render_pull_request_body
from upstream_sync_manifest import serialize_manifest
from upstream_sync_manifest import validate_chain


RELEASE = "a" * 40
FORK = "b" * 40


def make_manifest(
    commit: str = RELEASE,
    previous: str | None = None,
) -> SynchronizationManifest:
    tag = "rust-v1.2.3" if commit == RELEASE else f"rust-v1.2.{int(commit[0], 16)}"
    return SynchronizationManifest(
        schema_version=1,
        release=ReleaseIdentity(tag, commit, canonical_release_url(tag)),
        fork_base_sha=FORK,
        previous_release_commit=previous,
        selection_mode="automatic",
        preparation_mode="clean",
        conflict_paths=(),
    )


class UpstreamSyncManifestTest(unittest.TestCase):
    def test_manifest_round_trip_rejects_noncanonical_or_invalid_documents(self) -> None:
        expected = make_manifest()
        canonical = serialize_manifest(expected)

        self.assertEqual(parse_manifest(canonical), expected)
        payload = json.loads(canonical)
        mutations = {
            "schema type": lambda value: value.update(schemaVersion=True),
            "schema version": lambda value: value.update(schemaVersion=2),
            "unknown field": lambda value: value.update(extra=True),
            "unknown release field": lambda value: value["release"].update(extra=True),
            "uppercase sha": lambda value: value["release"].update(commit="A" * 40),
            "invalid tag": lambda value: value["release"].update(tag="rust-v1.2.03"),
            "noncanonical url": lambda value: value["release"].update(
                url="https://example.test/release"
            ),
            "selection enum": lambda value: value.update(selectionMode="scheduled"),
            "preparation enum": lambda value: value.update(preparationMode="merged"),
            "mode evidence": lambda value: value.update(
                preparationMode="conflicting", conflictPaths=[]
            ),
            "unsorted paths": lambda value: value.update(
                preparationMode="conflicting", conflictPaths=["z", "a"]
            ),
            "duplicate paths": lambda value: value.update(
                preparationMode="conflicting", conflictPaths=["a", "a"]
            ),
            "noncanonical path": lambda value: value.update(
                preparationMode="conflicting", conflictPaths=["a/../b"]
            ),
            "dot path": lambda value: value.update(
                preparationMode="conflicting", conflictPaths=["."]
            ),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                changed = json.loads(canonical)
                mutate(changed)
                with self.assertRaises(ValueError):
                    parse_manifest(json.dumps(changed, indent=2) + "\n")
        with self.assertRaisesRegex(ValueError, "canonically"):
            parse_manifest(json.dumps(payload) + "\n")

    def test_release_identity_determines_canonical_url_filename_and_path(self) -> None:
        manifest = make_manifest()

        self.assertEqual(
            (
                manifest.release.url,
                manifest_filename(manifest.release.commit),
                manifest_path(manifest.release.commit),
            ),
            (
                "https://github.com/openai/codex/releases/tag/rust-v1.2.3",
                f"{RELEASE}.json",
                f".github/upstream-sync-manifests/{RELEASE}.json",
            ),
        )
        for invalid in ("sdk-v1.2.3", "rust-v1.2.3-01"):
            with self.subTest(tag=invalid), self.assertRaises(ValueError):
                canonical_release_url(invalid)
        with self.assertRaises(ValueError):
            manifest_path("A" * 40)

    def test_chain_returns_its_unique_tip_and_rejects_invalid_graphs(self) -> None:
        root = make_manifest()
        second = make_manifest("c" * 40, RELEASE)
        tip = make_manifest("d" * 40, second.release.commit)

        self.assertEqual(validate_chain((tip, root, second)), tip)
        cycle_left = make_manifest("e" * 40, "f" * 40)
        cycle_right = make_manifest("f" * 40, cycle_left.release.commit)
        duplicate_tag = replace(
            second,
            release=replace(
                second.release,
                tag=root.release.tag,
                url=root.release.url,
            ),
        )
        invalid = {
            "missing link": (root, replace(second, previous_release_commit="e" * 40)),
            "fork and multiple tips": (root, second, make_manifest("e" * 40, RELEASE)),
            "cycle": (root, second, cycle_left, cycle_right),
            "duplicate commit": (root, replace(root, fork_base_sha="e" * 40)),
            "duplicate tag": (root, duplicate_tag),
            "multiple roots": (root, replace(second, previous_release_commit=None)),
        }
        for name, chain in invalid.items():
            with self.subTest(name=name), self.assertRaises(ValueError):
                validate_chain(chain)
        with self.assertRaisesRegex(ValueError, "seed"):
            validate_chain((root,), expected_seed=replace(root, fork_base_sha="e" * 40))

    def test_pr153_seed_matches_the_historical_conflict_set(self) -> None:
        repo = Path(__file__).parents[2]
        seed_path = repo / ".github/upstream-sync-manifests" / (
            "b3a6d7f67cf056e18472c2b9ec26d3999ed40b7b.json"
        )
        seed_text = seed_path.read_text()
        seed = parse_manifest(seed_text)
        process = subprocess.run(
            [
                "git",
                "merge-tree",
                "--write-tree",
                seed.fork_base_sha,
                seed.release.commit,
            ],
            cwd=repo,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
        conflicts = tuple(
            sorted(
                {
                    line.split("\t", 1)[1]
                    for line in process.stdout.splitlines()
                    if "\t" in line
                    and line.split("\t", 1)[0].endswith((" 1", " 2", " 3"))
                }
            )
        )

        self.assertEqual(
            (process.returncode, serialize_manifest(seed), seed.conflict_paths),
            (1, seed_text, conflicts),
        )
        self.assertEqual(validate_chain((seed,), expected_seed=seed), seed)

    def test_pull_request_body_is_pure_and_bounds_conflict_details(self) -> None:
        conflicts = tuple(f"conflict-{index:02}.txt" for index in range(22))
        manifest = replace(
            make_manifest(),
            selection_mode="manual",
            preparation_mode="conflicting",
            conflict_paths=conflicts,
        )

        body = render_pull_request_body(manifest)

        self.assertEqual(body, render_pull_request_body(manifest))
        for immutable_input in (
            manifest.release.tag,
            manifest.release.commit,
            manifest.fork_base_sha,
            manifest.selection_mode,
            manifest.preparation_mode,
        ):
            self.assertIn(immutable_input, body)
        self.assertIn("22 total", body)
        self.assertIn('    "conflict-19.txt"', body)
        self.assertNotIn("conflict-20.txt", body)
        self.assertIn("Omitted conflicts: 2", body)
        self.assertLess(len(body), 4_000)

    def test_pull_request_body_escapes_each_path_as_one_reversible_line(self) -> None:
        path = "safe`\n# injected heading"
        manifest = replace(
            make_manifest(),
            preparation_mode="conflicting",
            conflict_paths=(path,),
        )

        body = render_pull_request_body(manifest)
        encoded_lines = [line.strip() for line in body.splitlines() if line.startswith("    ")]

        self.assertEqual([json.loads(line) for line in encoded_lines], [path])
        self.assertNotIn("\n# injected heading", body)
        self.assertNotIn(f"`{path}`", body)

    def test_pull_request_body_budget_keeps_only_complete_extreme_paths(self) -> None:
        paths = tuple(f"{index:02}-" + "x" * 4_080 for index in range(20))
        manifest = replace(
            make_manifest(),
            preparation_mode="conflicting",
            conflict_paths=paths,
        )

        body = render_pull_request_body(manifest)
        encoded_lines = [line.strip() for line in body.splitlines() if line.startswith("    ")]
        displayed = [json.loads(line) for line in encoded_lines]

        self.assertLessEqual(len(body), MAX_PULL_REQUEST_BODY_CHARACTERS)
        self.assertGreater(len(displayed), 0)
        self.assertLess(len(displayed), len(paths))
        self.assertEqual(displayed, list(paths[: len(displayed)]))
        self.assertIn(f"Omitted conflicts: {len(paths) - len(displayed)}", body)


if __name__ == "__main__":
    unittest.main()
