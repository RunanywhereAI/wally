#!/usr/bin/env python3
"""Hermetic tests for scripts/release/verify-release-assets.py."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import pathlib
import stat
import tarfile
import tempfile
import unittest
import zipfile


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "verify_release_assets", ROOT / "scripts" / "release" / "verify-release-assets.py"
)
assert SPEC is not None and SPEC.loader is not None
VERIFY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFY)


class ReleaseAssetTests(unittest.TestCase):
    def sidecar(self, archive: pathlib.Path, *, filename: str | None = None) -> pathlib.Path:
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        sidecar = archive.with_name(archive.name + ".sha256")
        sidecar.write_text(f"{digest}  {filename or archive.name}\n", encoding="ascii")
        return sidecar

    def add_tar_file(self, bundle: tarfile.TarFile, name: str, contents: bytes, mode: int) -> None:
        member = tarfile.TarInfo(name)
        member.size = len(contents)
        member.mode = mode
        bundle.addfile(member, io.BytesIO(contents))

    def test_valid_macos_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-1.2.3-macos-arm64.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                self.add_tar_file(bundle, "wally-macos-arm64/README.md", b"readme", 0o644)
                self.add_tar_file(bundle, "wally-macos-arm64/bin/wally", b"binary", 0o755)
            VERIFY.verify(archive, self.sidecar(archive))

    def test_valid_linux_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-1.2.3-linux-x86_64.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                self.add_tar_file(bundle, "wally-linux-x86_64/README.md", b"readme", 0o644)
                self.add_tar_file(bundle, "wally-linux-x86_64/bin/wally", b"binary", 0o755)
            VERIFY.verify(archive, self.sidecar(archive))

    def test_valid_windows_arm64_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-1.2.3-windows-arm64.zip"
            with zipfile.ZipFile(archive, "w") as bundle:
                bundle.writestr("wally-windows-arm64/README.md", b"readme")
                bundle.writestr("wally-windows-arm64/bin/wally.exe", b"binary")
            VERIFY.verify(archive, self.sidecar(archive))

    def test_rejects_dev_bottle_name(self) -> None:
        # The -dev suffix is no longer published; the verifier must reject it
        # so a stale dev artifact cannot accidentally land in a production release.
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-0.5.3-macos-arm64-dev.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                self.add_tar_file(bundle, "wally-macos-arm64/README.md", b"readme", 0o644)
                self.add_tar_file(bundle, "wally-macos-arm64/bin/wally", b"binary", 0o755)
            with self.assertRaisesRegex(VERIFY.VerificationError, "unsupported release asset name"):
                VERIFY.verify(archive, self.sidecar(archive))

    def test_valid_windows_archive_with_backslash_members(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-1.2.3-windows-x86_64.zip"
            with zipfile.ZipFile(archive, "w") as bundle:
                bundle.writestr("wally-windows-x86_64\\README.md", b"readme")
                bundle.writestr("wally-windows-x86_64\\bin\\wally.exe", b"binary")
            VERIFY.verify(archive, self.sidecar(archive))

    def test_rejects_traversal(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-1.2.3-windows-x86_64.zip"
            with zipfile.ZipFile(archive, "w") as bundle:
                bundle.writestr("wally-windows-x86_64/README.md", b"readme")
                bundle.writestr("wally-windows-x86_64/bin/wally.exe", b"binary")
                bundle.writestr("wally-windows-x86_64/../outside", b"bad")
            with self.assertRaisesRegex(VERIFY.VerificationError, "unsafe archive member"):
                VERIFY.verify(archive, self.sidecar(archive))

    def test_rejects_wrong_checksum_filename(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-1.2.3-macos-arm64.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                self.add_tar_file(bundle, "wally-macos-arm64/README.md", b"readme", 0o644)
                self.add_tar_file(bundle, "wally-macos-arm64/bin/wally", b"binary", 0o755)
            with self.assertRaisesRegex(VERIFY.VerificationError, "sidecar names"):
                VERIFY.verify(archive, self.sidecar(archive, filename="different.tar.gz"))

    def test_rejects_non_executable_macos_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-1.2.3-macos-arm64.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                self.add_tar_file(bundle, "wally-macos-arm64/README.md", b"readme", 0o644)
                self.add_tar_file(bundle, "wally-macos-arm64/bin/wally", b"binary", 0o644)
            with self.assertRaisesRegex(VERIFY.VerificationError, "executable mode bit"):
                VERIFY.verify(archive, self.sidecar(archive))


if __name__ == "__main__":
    unittest.main()
