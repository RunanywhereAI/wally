#!/usr/bin/env python3
"""Fail a Linux release archive whose ELF files need more than the supported systems have.

    check-linux-abi.py <wally-X.Y.Z-linux-x86_64.tar.gz>

Two things decide whether the bottle starts on a user's machine, and neither is
visible until it runs there:

- The newest glibc / libstdc++ symbol version any ELF file in it asks for. A
  bottle built on Ubuntu 24.04 asked for GLIBC_2.38 and GLIBCXX_3.4.32, so v0.6.0
  failed to start on 22.04 with a loader error the installer then hid.
- Every shared library it needs that it does not ship. v0.6.0 needed the
  compiler's libgomp.so.1, which a clean Ubuntu or Debian does not have.

The ceilings and the allowed system libraries live in versions.toml
`[linux_abi]`, so moving the supported floor is one reviewed edit there. The ELF
reading is done here in the standard library rather than by shelling out to
readelf, so this runs the same on a release runner, a Mac, and in its tests.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import struct
import sys
import tarfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
VERSIONS = ROOT / "versions.toml"

ELF_MAGIC = b"\x7fELF"
ELFCLASS64 = 2
ELFDATA2LSB = 1
SHT_DYNAMIC = 6
SHT_NOBITS = 8
SHT_GNU_VERNEED = 0x6FFFFFFE
DT_NULL = 0
DT_NEEDED = 1
PT_INTERP = 3

# The symbol-version families whose ceiling is checked. Anything else a
# library versions (OPENSSL_3.0.0, CURL_OPENSSL_4, a bundled library's own
# tags) is pinned by the soname being allowed at all.
FAMILIES = {"GLIBC": "glibc_max", "GLIBCXX": "glibcxx_max", "CXXABI": "cxxabi_max"}
VERSION_TAG = re.compile(r"^(GLIBC|GLIBCXX|CXXABI)_([0-9]+(?:\.[0-9]+)*)$")

# glibc also defines synthetic ABI markers instead of a normal per-symbol
# version: a verneed entry that names a capability rather than a release, so
# VERSION_TAG never matches it and it silently passed as an unrecognised tag.
# GLIBC_ABI_DT_RELR is the one seen in the wild: a binary linked with
# -Wl,-z,pack-relative-relocs (DT_RELR) needs it, and only the loader from the
# glibc release that added DT_RELR support -- 2.36 -- provides it.
GLIBC_ABI_MARKERS = {"GLIBC_ABI_DT_RELR": "2.36"}


class AbiError(RuntimeError):
    pass


def version_key(text: str) -> tuple[int, ...]:
    return tuple(int(part) for part in text.split("."))


def read_linux_abi_policy(path: pathlib.Path = VERSIONS) -> dict[str, object]:
    """`[linux_abi]` from versions.toml: three ceilings and the system sonames."""
    section: dict[str, object] = {}
    in_section = False
    text = path.read_text(encoding="utf-8")
    # The allow-list is a TOML array that may span lines; join it back first.
    for raw in re.sub(r"\[\s*\n(.*?)\]", lambda m: "[" + m.group(1).replace("\n", " ") + "]",
                      text, flags=re.S).splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("[") and line.endswith("]") and "=" not in line:
            in_section = line == "[linux_abi]"
            continue
        if not in_section or "=" not in line:
            continue
        key, value = (part.strip() for part in line.split("=", 1))
        if value.startswith("["):
            section[key] = re.findall(r'"([^"]+)"', value)
        else:
            section[key] = value.strip('"')
    missing = [k for k in (*FAMILIES.values(), "system_libraries", "interpreters") if k not in section]
    if missing:
        raise AbiError(f"versions.toml [linux_abi] is missing {missing}")
    return section


def _cstring(blob: bytes, offset: int) -> str:
    end = blob.index(b"\0", offset)
    return blob[offset:end].decode("ascii", "replace")


def read_interp(data: bytes, name: str) -> str | None:
    """PT_INTERP's path, or None if this ELF has no program header table.

    This is the loader the kernel execs before any DT_NEEDED library is even
    tried, so a missing or nonstandard one fails earlier than the DT_NEEDED
    checks below can ever see.
    """
    phoff = struct.unpack_from("<Q", data, 0x20)[0]
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    if phoff == 0 or phnum == 0:
        return None
    for index in range(phnum):
        pos = phoff + index * phentsize
        if pos + 56 > len(data):
            raise AbiError(f"{name}: program header {index} runs past the end of the file")
        p_type, _p_flags, p_offset, _p_vaddr, _p_paddr, p_filesz, _p_memsz, _p_align = struct.unpack_from(
            "<IIQQQQQQ", data, pos
        )
        if p_type == PT_INTERP:
            # p_filesz is what the loader actually reads from this segment; a
            # NUL found past it would never be seen by the real loader, so the
            # path and its terminator must both fit inside [p_offset, p_offset
            # + p_filesz) before it is trusted.
            if p_offset >= len(data) or p_filesz == 0 or p_offset + p_filesz > len(data):
                raise AbiError(f"{name}: PT_INTERP segment runs past the end of the file")
            try:
                end = data.index(b"\0", p_offset, p_offset + p_filesz)
            except ValueError as exc:
                raise AbiError(f"{name}: PT_INTERP is not NUL-terminated within its segment") from exc
            return data[p_offset:end].decode("ascii", "replace")
    return None


def read_elf(data: bytes, name: str) -> tuple[set[str], set[str], str | None]:
    """(needed sonames, required symbol versions, PT_INTERP path) of one
    64-bit little-endian ELF.

    A file that cannot be walked is an error, never an empty answer: an empty
    answer would read as "needs nothing" and pass.
    """
    try:
        return _read_elf(data, name)
    except (struct.error, ValueError, IndexError) as exc:
        raise AbiError(f"{name}: malformed ELF ({exc})") from exc


def _read_elf(data: bytes, name: str) -> tuple[set[str], set[str], str | None]:
    if data[:4] != ELF_MAGIC:
        raise AbiError(f"{name}: not an ELF file")
    if data[4] != ELFCLASS64 or data[5] != ELFDATA2LSB:
        raise AbiError(f"{name}: only 64-bit little-endian ELF is supported")
    shoff = struct.unpack_from("<Q", data, 0x28)[0]
    shentsize, shnum = struct.unpack_from("<HH", data, 0x3A)
    if shoff == 0 or shnum == 0:
        # The dynamic and version tables are found through the section table.
        # Without one this reads nothing, and nothing would pass as "needs
        # nothing"; refuse instead.
        raise AbiError(f"{name}: no section header table, so its needs cannot be read")
    sections = [
        struct.unpack_from("<IIQQQQIIQQ", data, shoff + i * shentsize) for i in range(shnum)
    ]
    for index, (_, sh_type, _, _, offset, size, *_rest) in enumerate(sections):
        # A slice past the end of the file is silently short, not an error, so a
        # truncated table would be read as a smaller one. Check every section
        # that occupies file bytes.
        if sh_type != SHT_NOBITS and offset + size > len(data):
            raise AbiError(f"{name}: section {index} runs past the end of the file")

    def section_bytes(index: int) -> bytes:
        _, _, _, _, offset, size, *_ = sections[index]
        return data[offset : offset + size]

    needed: set[str] = set()
    versions: set[str] = set()
    for sh_name, sh_type, _, _, offset, size, link, info, _, entsize in sections:
        if sh_type == SHT_DYNAMIC:
            strings = section_bytes(link)
            for pos in range(offset, offset + size, entsize or 16):
                tag, value = struct.unpack_from("<qQ", data, pos)
                if tag == DT_NULL:
                    break
                if tag == DT_NEEDED:
                    needed.add(_cstring(strings, value))
        elif sh_type == SHT_GNU_VERNEED:
            strings = section_bytes(link)
            pos = offset
            for _ in range(info):
                _, vn_cnt, _, vn_aux, vn_next = struct.unpack_from("<HHIII", data, pos)
                aux = pos + vn_aux
                for _ in range(vn_cnt):
                    _, _, _, vna_name, vna_next = struct.unpack_from("<IHHII", data, aux)
                    versions.add(_cstring(strings, vna_name))
                    aux += vna_next
                pos += vn_next
    return needed, versions, read_interp(data, name)


def archive_files(archive: pathlib.Path) -> dict[str, bytes]:
    """Every regular file in a .tar.gz, by member name, read without extracting.

    Nothing is written to disk, so there is no path to sanitize and no reliance
    on tarfile's extraction filters, which not every runner's Python has.
    """
    files: dict[str, bytes] = {}
    with tarfile.open(archive, "r:gz") as bundle:
        for member in bundle.getmembers():
            if member.isfile():
                handle = bundle.extractfile(member)
                if handle is not None:
                    files[member.name] = handle.read()
    return files


def check_files(files: dict[str, bytes], policy: dict[str, object]) -> list[str]:
    """Every problem in a bottle's files, one line each; empty when it passes."""
    ceilings = {family: str(policy[key]) for family, key in FAMILIES.items()}
    system = set(policy["system_libraries"])  # type: ignore[arg-type]
    interpreters = set(policy["interpreters"])  # type: ignore[arg-type]
    elves = {name: data for name, data in sorted(files.items()) if data[:4] == ELF_MAGIC}
    if not elves:
        return ["no ELF files found; this is not a Linux bottle"]
    shipped = {pathlib.PurePosixPath(name).name for name in elves}

    problems: list[str] = []
    for relative, data in elves.items():
        needed, versions, interp = read_elf(data, relative)
        # PT_INTERP is only meaningful on the file the kernel execs directly
        # (package-wally.sh always stages that at <platform>/bin/<name>);
        # every shared library under lib/ is dynamically linked too but is
        # never handed to a loader itself, so it correctly has none. Where an
        # interpreter is present, though -- on bin/wally or otherwise -- it
        # must resolve to a loader this bottle can actually rely on.
        if interp is not None:
            if interp not in interpreters:
                problems.append(
                    f"{relative}: PT_INTERP is {interp!r}, not one of the allowed "
                    f"loaders {sorted(interpreters)}"
                )
        elif needed and pathlib.PurePosixPath(relative).parent.name == "bin":
            problems.append(
                f"{relative}: is a shipped executable but has no PT_INTERP; it would "
                "fail to start before any DT_NEEDED library is even tried"
            )
        for tag in sorted(versions):
            match = VERSION_TAG.match(tag)
            if match:
                if version_key(match.group(2)) > version_key(ceilings[match.group(1)]):
                    problems.append(
                        f"{relative}: needs {tag}, above the {match.group(1)}_{ceilings[match.group(1)]} "
                        "the supported Linux floor provides"
                    )
                continue
            marker_floor = GLIBC_ABI_MARKERS.get(tag)
            if marker_floor and version_key(marker_floor) > version_key(ceilings["GLIBC"]):
                problems.append(
                    f"{relative}: needs {tag}, which requires glibc {marker_floor}, above the "
                    f"GLIBC_{ceilings['GLIBC']} the supported Linux floor provides"
                )
        for soname in sorted(needed - shipped - system):
            problems.append(
                f"{relative}: needs {soname}, which the bottle does not ship and a clean "
                "system is not guaranteed to have"
            )
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("archive", type=pathlib.Path)
    parser.add_argument("--versions", type=pathlib.Path, default=VERSIONS)
    args = parser.parse_args(argv)
    try:
        policy = read_linux_abi_policy(args.versions)
        problems = check_files(archive_files(args.archive), policy)
    except (AbiError, OSError, tarfile.TarError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        return 1
    print(
        f"{args.archive.name}: every ELF within GLIBC_{policy['glibc_max']}, "
        f"GLIBCXX_{policy['glibcxx_max']}, CXXABI_{policy['cxxabi_max']}; "
        "every external library is on the allow-list"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
