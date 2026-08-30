import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
import zipfile

from validation_contracts import ContractError
from validation_release import ReleaseArtifact
from validation_release_files import verify_release_files


SOURCE_SHA = "a" * 40


def _write_tar(path: Path, binary: bytes, signature: bytes) -> None:
    with tarfile.open(path, mode="w:gz") as archive:
        for name, contents in (("codex", binary), ("codex.sigstore", signature)):
            member = tarfile.TarInfo(name)
            member.size = len(contents)
            archive.addfile(member, io.BytesIO(contents))


def _write_zip(path: Path, binary: bytes) -> None:
    with zipfile.ZipFile(path, mode="w") as archive:
        archive.writestr("codex-rs/target/release/codex.exe", binary)


def _artifact(directory: Path, platform: str, index: int) -> ReleaseArtifact:
    windows = platform.startswith("windows-")
    archive = directory / (
        f"codex-{platform}.zip" if windows else f"codex-{platform}.tar.gz"
    )
    signature = b"sigstore-" + platform.encode()
    if windows:
        _write_zip(archive, b"windows-binary")
        signature_digest = None
    else:
        _write_tar(archive, b"linux-binary", signature)
        signature_digest = hashlib.sha256(signature).hexdigest()
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    provenance = {
        "sourceSha": SOURCE_SHA,
        "archive": archive.name,
        "archiveSha256": digest,
        "signatureSha256": signature_digest or "not-applicable",
        "builder": f"runner/{index}",
        "command": "cargo build --release --target target --bin codex",
        "provenance": "source,toolchain,package,checksum recorded in this run",
        "signatureBoundary": "release-certification",
    }
    provenance_path = directory / f"provenance-{platform}.json"
    provenance_path.write_text(
        json.dumps(provenance, indent=2) + "\n", encoding="utf-8"
    )
    return ReleaseArtifact(
        name=archive.name,
        digest=digest,
        platform=platform,
        packaging="zip" if windows else "tar.gz",
        producer="release-build",
        provenance_digest=hashlib.sha256(provenance_path.read_bytes()).hexdigest(),
        signature_digest=signature_digest,
    )


class ValidationReleaseTests(unittest.TestCase):
    def test_certification_verifies_archive_provenance_and_embedded_signature_bytes(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            artifacts = tuple(
                _artifact(directory, platform, index)
                for index, platform in enumerate(
                    ("linux-x64", "linux-arm64", "windows-x64", "windows-arm64")
                )
            )

            self.assertEqual(
                artifacts,
                verify_release_files(directory, artifacts, source_sha=SOURCE_SHA),
            )

    def test_certification_rejects_tampered_downloaded_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            artifact = _artifact(directory, "linux-x64", 0)
            archive = directory / artifact.name
            archive.write_bytes(archive.read_bytes() + b"tampered")

            with self.assertRaises(ContractError):
                verify_release_files(directory, (artifact,), source_sha=SOURCE_SHA)


if __name__ == "__main__":
    unittest.main()
