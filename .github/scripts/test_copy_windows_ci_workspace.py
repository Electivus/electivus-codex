import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


@unittest.skipUnless(os.name == "nt", "Windows workspace copy uses robocopy")
class CopyWindowsCiWorkspaceTests(unittest.TestCase):
    def test_copies_checkout_and_refuses_existing_destination(self) -> None:
        pwsh = shutil.which("pwsh")
        if pwsh is None:
            self.skipTest("PowerShell 7 is required")

        script = Path(__file__).with_name("copy-windows-ci-workspace.ps1")
        with tempfile.TemporaryDirectory() as temp_dir:
            temp = Path(temp_dir)
            source = temp / "source"
            destination = temp / "destination"
            (source / "codex-rs").mkdir(parents=True)
            (source / ".github/scripts").mkdir(parents=True)
            marker = source / "codex-rs/marker.txt"
            marker.write_bytes(b"nextest-workspace\n")

            command = [
                pwsh,
                "-NoLogo",
                "-NoProfile",
                "-File",
                str(script),
                "-Source",
                str(source),
                "-Destination",
                str(destination),
            ]
            first = subprocess.run(command, check=False, capture_output=True, text=True)
            self.assertEqual(0, first.returncode, first.stdout + first.stderr)
            self.assertEqual(
                marker.read_bytes(),
                (destination / "codex-rs/marker.txt").read_bytes(),
            )

            second = subprocess.run(command, check=False, capture_output=True, text=True)
            self.assertNotEqual(0, second.returncode)
            self.assertIn(
                "Stable Windows CI workspace already exists",
                second.stdout + second.stderr,
            )


if __name__ == "__main__":
    unittest.main()
