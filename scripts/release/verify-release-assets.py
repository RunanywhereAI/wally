#!/usr/bin/env python3
"""Fail-closed validation for an Wally release archive and SHA-256 sidecar."""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import re
import stat
import tarfile
import zipfile


ASSET = re.compile(
    r"^wally-(?P<version>[0-9]+\.[0-9]+\.[0-9]+)-"
    r"(?P<platform>macos-arm64|linux-x86_64|windows-x86_64|windows-arm64)"
    r"\.(?P<suffix>tar\.gz|zip)$"
)
MAX_MEMBERS = 100_000
MAX_UNCOMPRESSED_BYTES = 4 * 1024 * 1024 * 1024


class VerificationError(RuntimeError):
    pass


def normalized_member(name: str) -> str:
    normalized = name.replace("\\", "/").rstrip("/")
    path = pathlib.PurePosixPath(normalized)
    if (
        not normalized
        or normalized.startswith("/")
        or path.is_absolute()
        or ".." in path.parts
        or not path.parts
        or path.parts[0].endswith(":")
    ):
        raise VerificationError(f"unsafe archive member: {name!r}")
    return normalized


def validate_members(
    members: list[tuple[str, int, bool, bool]], expected_root: str, executable: str
) -> None:
    if len(members) > MAX_MEMBERS:
        raise VerificationError(f"archive has more than {MAX_MEMBERS} members")

    seen: set[str] = set()
    roots: set[str] = set()
    sizes: dict[str, int] = {}
    executable_bits: dict[str, bool] = {}
    total = 0
    for original, size, is_regular, is_executable in members:
        name = normalized_member(original)
        if name in seen:
            raise VerificationError(f"duplicate archive member after normalization: {name}")
        seen.add(name)
        roots.add(pathlib.PurePosixPath(name).parts[0])
        if size < 0:
            raise VerificationError(f"negative member size: {name}")
        total += size
        if total > MAX_UNCOMPRESSED_BYTES:
            raise VerificationError("archive expands beyond the 4 GiB safety limit")
        if is_regular:
            sizes[name] = size
            executable_bits[name] = is_executable

    if roots != {expected_root}:
        raise VerificationError(
            f"archive root is {sorted(roots)!r}; expected exactly {expected_root!r}"
        )
    readme = f"{expected_root}/README.md"
    binary = f"{expected_root}/bin/{executable}"
    if sizes.get(readme, 0) <= 0:
        raise VerificationError(f"archive is missing non-empty {readme}")
    if sizes.get(binary, 0) <= 0:
        raise VerificationError(f"archive is missing non-empty {binary}")
    if executable == "wally" and not executable_bits.get(binary, False):
        raise VerificationError(f"{binary} has no executable mode bit")


def verify_tar(archive: pathlib.Path, expected_root: str) -> None:
    members: list[tuple[str, int, bool, bool]] = []
    try:
        with tarfile.open(archive, "r:gz") as bundle:
            for member in bundle.getmembers():
                if member.issym() or member.islnk() or member.isdev() or member.isfifo():
                    raise VerificationError(
                        f"links and special files are not allowed: {member.name!r}"
                    )
                members.append(
                    (member.name, member.size, member.isfile(), bool(member.mode & 0o111))
                )
    except (tarfile.TarError, OSError) as exc:
        raise VerificationError(f"could not read tar archive: {exc}") from exc
    validate_members(members, expected_root, "wally")


def verify_zip(archive: pathlib.Path, expected_root: str) -> None:
    members: list[tuple[str, int, bool, bool]] = []
    try:
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.infolist():
                unix_mode = member.external_attr >> 16
                if stat.S_IFMT(unix_mode) == stat.S_IFLNK:
                    raise VerificationError(f"symbolic links are not allowed: {member.filename!r}")
                is_directory = member.is_dir() or member.filename.endswith(("/", "\\"))
                members.append(
                    (member.filename, member.file_size, not is_directory, bool(unix_mode & 0o111))
                )
    except (zipfile.BadZipFile, OSError) as exc:
        raise VerificationError(f"could not read zip archive: {exc}") from exc
    validate_members(members, expected_root, "wally.exe")


def verify_sidecar(archive: pathlib.Path, sidecar: pathlib.Path) -> None:
    try:
        line = sidecar.read_text(encoding="ascii").strip()
    except (OSError, UnicodeError) as exc:
        raise VerificationError(f"could not read checksum sidecar: {exc}") from exc
    match = re.fullmatch(r"([0-9A-Fa-f]{64})[ \t]+\*?([^\r\n]+)", line)
    if match is None:
        raise VerificationError("checksum sidecar must contain one SHA-256 and filename")
    if match.group(2).strip() != archive.name:
        raise VerificationError(
            f"checksum sidecar names {match.group(2).strip()!r}, expected {archive.name!r}"
        )
    expected = match.group(1).lower()
    digest = hashlib.sha256()
    try:
        with archive.open("rb") as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(block)
    except OSError as exc:
        raise VerificationError(f"could not hash archive: {exc}") from exc
    actual = digest.hexdigest()
    if actual != expected:
        raise VerificationError(f"checksum mismatch: expected {expected}, got {actual}")


def verify(archive: pathlib.Path, sidecar: pathlib.Path) -> None:
    match = ASSET.fullmatch(archive.name)
    if match is None:
        raise VerificationError(f"unsupported release asset name: {archive.name!r}")
    if not archive.is_file() or archive.stat().st_size == 0:
        raise VerificationError(f"release asset is missing or empty: {archive}")
    verify_sidecar(archive, sidecar)
    platform = match.group("platform")
    expected_root = f"wally-{platform}"
    if match.group("suffix") == "tar.gz":
        verify_tar(archive, expected_root)
    else:
        verify_zip(archive, expected_root)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=pathlib.Path)
    parser.add_argument("sidecar", type=pathlib.Path)
    args = parser.parse_args()
    try:
        verify(args.archive, args.sidecar)
    except VerificationError as exc:
        parser.error(str(exc))
    print(f"verified {args.archive.name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
