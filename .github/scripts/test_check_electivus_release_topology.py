from pathlib import Path
import unittest

import check_electivus_release_topology as topology


class ElectivusReleaseTopologyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        repo = Path(__file__).resolve().parents[2]
        cls.sources = tuple(
            (repo / path).read_text(encoding="utf-8") for path in topology.SOURCES
        )

    def test_current_release_topology_is_complete(self) -> None:
        self.assertEqual([], topology.validate_topology(*self.sources))

    def test_material_boundary_mutations_fail_closed(self) -> None:
        release, windows, documentation = self.sources
        cases = (
            (
                "dedicated tag namespace",
                0,
                release.replace('electivus-v*.*.*', 'rust-v*.*.*', 1),
            ),
            (
                "hosted Linux matrix",
                0,
                release.replace("runner: ubuntu-24.04-arm", "runner: self-hosted", 1),
            ),
            (
                "Linux keyless signing",
                0,
                release.replace("id-token: write", "id-token: none", 1),
            ),
            (
                "unsigned Windows reuse",
                0,
                release.replace("publish_release: false", "publish_release: true", 1),
            ),
            (
                "upstream-compatible public filenames",
                0,
                release.replace(
                    '"${dest}/${binary}-${TARGET}"',
                    '"${dest}/electivus-${binary}-${TARGET}"',
                    1,
                ),
            ),
        )
        for expected, index, changed in cases:
            mutated = list(self.sources)
            mutated[index] = changed
            with self.subTest(expected=expected):
                self.assertIn(expected, "\n".join(topology.validate_topology(*mutated)))


if __name__ == "__main__":
    unittest.main()
