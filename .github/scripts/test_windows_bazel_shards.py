from pathlib import Path
import subprocess
import sys
import unittest

from windows_bazel_shards import select_targets
from windows_bazel_shards import shard_for_target


class WindowsBazelShardsTests(unittest.TestCase):
    def test_four_shards_are_deterministic_disjoint_and_exhaustive(self) -> None:
        targets = [f"//codex-rs/package-{index}:tests" for index in range(100)]
        selections = [select_targets(targets, shard, 4) for shard in range(1, 5)]

        self.assertTrue(all(selections))
        self.assertEqual(sorted(targets), sorted(target for row in selections for target in row))
        self.assertEqual(len(targets), len({target for row in selections for target in row}))
        self.assertEqual(selections, [select_targets(reversed(targets), shard, 4) for shard in range(1, 5)])
        for shard, row in enumerate(selections, 1):
            self.assertTrue(all(shard_for_target(target, 4) == shard for target in row))

    def test_invalid_or_empty_inputs_fail_closed(self) -> None:
        cases = (
            ([], 1, 4, "returned no Windows test targets"),
            (["//a:test", "//a:test"], 1, 4, "duplicate Windows test targets"),
            (["//a:test"], 0, 4, "shard must be between"),
            (["//a:test"], 1, 0, "shard must be between"),
        )
        for targets, shard, shard_count, message in cases:
            with self.subTest(targets=targets, shard=shard, shard_count=shard_count):
                with self.assertRaisesRegex(ValueError, message):
                    select_targets(targets, shard, shard_count)

    def test_cli_emits_lf_only_on_windows(self) -> None:
        script = Path(__file__).with_name("windows_bazel_shards.py")
        result = subprocess.run(
            [sys.executable, str(script), "--shard", "1", "--shard-count", "1"],
            input=b"//a:test\n//b:test\n",
            check=True,
            capture_output=True,
        )
        self.assertEqual(b"//a:test\n//b:test\n", result.stdout)


if __name__ == "__main__":
    unittest.main()
