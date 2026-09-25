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
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
VERSIONS = ROOT / "versions.toml"

ELF_MAGIC = b"\x7fELF"
ELFCLASS64 = 2
ELFDATA2LSB = 1
SHT_DYNAMIC = 6
SHT_GNU_VERNEED = 0x6FFFFFFE
DT_NULL = 0
DT_NEEDED = 1

# The symbol-version families whose ceiling is checked. Anything else a
# library versions (OPENSSL_3.0.0, CURL_OPENSSL_4, a bundled library's own
# tags) is pinned by the soname being allowed at all.
FAMILIES = {"GLIBC": "glibc_max", "GLIBCXX": "glibcxx_max", "CXXABI": "cxxabi_max"}
VERSION_TAG = re.compile(r"^(GLIBC|GLIBCXX|CXXABI)_([0-9]+(?:\.[0-9]+)*)$")


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
    missing = [k for k in (*FAMILIES.values(), "system_libraries") if k not in section]
    if missing:
        raise AbiError(f"versions.toml [linux_abi] is missing {missing}")
    return section


def _cstring(blob: bytes, offset: int) -> str:
    end = blob.index(b"\0", offset)
    return blob[offset:end].decode("ascii", "replace")


def read_elf(data: bytes, name: str) -> tuple[set[str], set[str]]:
    """(needed sonames, required symbol versions) of one 64-bit little-endian ELF.

    A file that cannot be walked is an error, never an empty answer: an empty
    answer would read as "needs nothing" and pass.
    """
    try:
        return _read_elf(data, name)
    except (struct.error, ValueError, IndexError) as exc:
        raise AbiError(f"{name}: malformed ELF ({exc})") from exc


def _read_elf(data: bytes, name: str) -> tuple[set[str], set[str]]:
    if data[:4] != ELF_MAGIC:
        raise AbiError(f"{name}: not an ELF file")
    if data[4] != ELFCLASS64 or data[5] != ELFDATA2LSB:
        raise AbiError(f"{name}: only 64-bit little-endian ELF is supported")
    shoff = struct.unpack_from("<Q", data, 0x28)[0]
    shentsize, shnum = struct.unpack_from("<HH", data, 0x3A)
    sections = [
        struct.unpack_from("<IIQQQQIIQQ", data, shoff + i * shentsize) for i in range(shnum)
    ]

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
    return needed, versions


def check_tree(root: pathlib.Path, policy: dict[str, object]) -> list[str]:
    """Every problem found under `root`, one line each; empty when it passes."""
    ceilings = {family: str(policy[key]) for family, key in FAMILIES.items()}
    system = set(policy["system_libraries"])  # type: ignore[arg-type]
    files = [p for p in sorted(root.rglob("*")) if p.is_file() and not p.is_symlink()]
    elves = {p: p.read_bytes() for p in files if p.read_bytes()[:4] == ELF_MAGIC}
    if not elves:
        return ["no ELF files found; this is not a Linux bottle"]
    shipped = {p.name for p in elves}

    problems: list[str] = []
    for path, data in elves.items():
        relative = path.relative_to(root)
        needed, versions = read_elf(data, str(relative))
        for tag in sorted(versions):
            match = VERSION_TAG.match(tag)
            if match and version_key(match.group(2)) > version_key(ceilings[match.group(1)]):
                problems.append(
                    f"{relative}: needs {tag}, above the {match.group(1)}_{ceilings[match.group(1)]} "
                    "the supported Linux floor provides"
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
        with tempfile.TemporaryDirectory() as temporary:
            with tarfile.open(args.archive, "r:gz") as bundle:
                bundle.extractall(temporary, filter="data")
            problems = check_tree(pathlib.Path(temporary), policy)
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
