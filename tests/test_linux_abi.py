#!/usr/bin/env python3
"""Hermetic tests for scripts/release/check-linux-abi.py.

The ELF files are built here byte by byte, with only the sections the checker
reads (.dynstr, .dynamic, .gnu.version_r), so the tests need no compiler and run
the same on macOS and Linux.
"""

from __future__ import annotations

import importlib.util
import io
import pathlib
import struct
import tarfile
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_linux_abi", ROOT / "scripts" / "release" / "check-linux-abi.py"
)
assert SPEC is not None and SPEC.loader is not None
ABI = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ABI)

POLICY = {
    "glibc_max": "2.35",
    "glibcxx_max": "3.4.30",
    "cxxabi_max": "1.3.13",
    "system_libraries": ["libc.so.6", "libstdc++.so.6", "libm.so.6"],
}


def elf(needed: list[str], versions: dict[str, list[str]]) -> bytes:
    """A 64-bit little-endian ELF with DT_NEEDED entries and a verneed table.

    `versions` maps a library file name to the version tags required from it.
    """
    strtab = bytearray(b"\0")

    def intern(text: str) -> int:
        offset = len(strtab)
        strtab.extend(text.encode("ascii") + b"\0")
        return offset

    dynamic = b"".join(struct.pack("<qQ", 1, intern(name)) for name in needed)
    dynamic += struct.pack("<qQ", 0, 0)

    verneed = bytearray()
    files = list(versions.items())
    for index, (library, tags) in enumerate(files):
        file_name = intern(library)
        next_entry = 16 + 16 * len(tags) if index < len(files) - 1 else 0
        verneed += struct.pack("<HHIII", 1, len(tags), file_name, 16, next_entry)
        for position, tag in enumerate(tags):
            next_aux = 16 if position < len(tags) - 1 else 0
            verneed += struct.pack("<IHHII", 0, 0, 0, intern(tag), next_aux)

    header_size = 64
    blobs = [bytes(strtab), dynamic, bytes(verneed)]
    offsets = []
    cursor = header_size
    for blob in blobs:
        offsets.append(cursor)
        cursor += len(blob)
    shoff = cursor

    sections = [struct.pack("<IIQQQQIIQQ", 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)]
    sections.append(struct.pack("<IIQQQQIIQQ", 0, 3, 0, 0, offsets[0], len(blobs[0]), 0, 0, 1, 0))
    sections.append(struct.pack("<IIQQQQIIQQ", 0, 6, 0, 0, offsets[1], len(blobs[1]), 1, 0, 8, 16))
    sections.append(
        struct.pack(
            "<IIQQQQIIQQ", 0, 0x6FFFFFFE, 0, 0, offsets[2], len(blobs[2]), 1, len(files), 4, 0
        )
    )

    ident = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9)
    header = ident + struct.pack(
        "<HHIQQQIHHHHHH", 3, 62, 1, 0, 0, shoff, 0, 64, 0, 0, 64, len(sections), 0
    )
    assert len(header) == header_size
    return header + b"".join(blobs) + b"".join(sections)


class LinuxAbiTests(unittest.TestCase):
    def tree(self, files: dict[str, bytes]) -> pathlib.Path:
        directory = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(directory))
        for name, data in files.items():
            path = directory / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        return directory

    def test_reads_needed_libraries_and_symbol_versions(self) -> None:
        needed, versions = ABI.read_elf(
            elf(["libc.so.6", "libfoo.so.1"], {"libc.so.6": ["GLIBC_2.34", "GLIBC_2.2.5"]}),
            "wally",
        )
        self.assertEqual(needed, {"libc.so.6", "libfoo.so.1"})
        self.assertEqual(versions, {"GLIBC_2.34", "GLIBC_2.2.5"})

    def test_a_bottle_within_the_floor_passes(self) -> None:
        root = self.tree(
            {
                "wally-linux-x86_64/bin/wally": elf(
                    ["libc.so.6", "libstdc++.so.6", "libgomp.so.1"],
                    {
                        "libc.so.6": ["GLIBC_2.35", "GLIBC_2.2.5"],
                        "libstdc++.so.6": ["GLIBCXX_3.4.30", "CXXABI_1.3.13"],
                    },
                ),
                "wally-linux-x86_64/lib/libgomp.so.1": elf(
                    ["libc.so.6"], {"libc.so.6": ["GLIBC_2.34"]}
                ),
                "wally-linux-x86_64/README.md": b"not an ELF",
            }
        )
        self.assertEqual(ABI.check_tree(root, POLICY), [])

    def test_a_symbol_version_past_the_floor_fails(self) -> None:
        # v0.6.0: built on Ubuntu 24.04, asked for GLIBC_2.38 and GLIBCXX_3.4.32.
        root = self.tree(
            {
                "b/bin/wally": elf(
                    ["libc.so.6", "libstdc++.so.6"],
                    {"libc.so.6": ["GLIBC_2.38"], "libstdc++.so.6": ["GLIBCXX_3.4.32"]},
                )
            }
        )
        problems = ABI.check_tree(root, POLICY)
        self.assertEqual(len(problems), 2, problems)
        self.assertTrue(any("GLIBC_2.38" in p for p in problems))
        self.assertTrue(any("GLIBCXX_3.4.32" in p for p in problems))

    def test_versions_compare_numerically_not_as_text(self) -> None:
        # "2.4" sorts after "2.35" as text; it is the older version.
        root = self.tree({"b/bin/wally": elf(["libc.so.6"], {"libc.so.6": ["GLIBC_2.4"]})})
        self.assertEqual(ABI.check_tree(root, POLICY), [])
        cxxabi = self.tree(
            {"b/bin/wally": elf(["libstdc++.so.6"], {"libstdc++.so.6": ["CXXABI_1.3.14"]})}
        )
        self.assertEqual(len(ABI.check_tree(cxxabi, POLICY)), 1)

    def test_an_unshipped_library_off_the_allow_list_fails(self) -> None:
        # v0.6.0 again: libgomp.so.1 was needed and not shipped.
        root = self.tree({"b/bin/wally": elf(["libc.so.6", "libgomp.so.1"], {})})
        problems = ABI.check_tree(root, POLICY)
        self.assertEqual(len(problems), 1)
        self.assertIn("libgomp.so.1", problems[0])

    def test_other_libraries_own_version_tags_are_not_ceilings(self) -> None:
        root = self.tree(
            {"b/bin/wally": elf(["libc.so.6"], {"libssl.so.3": ["OPENSSL_3.0.0"]})}
        )
        self.assertEqual(ABI.check_tree(root, POLICY), [])

    def test_an_archive_with_no_elf_is_not_a_bottle(self) -> None:
        root = self.tree({"b/README.md": b"hello"})
        self.assertEqual(len(ABI.check_tree(root, POLICY)), 1)

    def test_a_truncated_elf_is_an_error_not_a_pass(self) -> None:
        whole = elf(["libc.so.6"], {"libc.so.6": ["GLIBC_2.34"]})
        # Cut inside the header, inside the section table, and inside the data
        # a section points at: each must raise, none may read as "needs nothing".
        for cut in (40, len(whole) - 10, 70):
            with self.subTest(cut=cut), self.assertRaises(ABI.AbiError):
                ABI.read_elf(whole[:cut], "wally")

    def test_the_policy_is_read_from_versions_toml(self) -> None:
        policy = ABI.read_linux_abi_policy()
        self.assertIn("libc.so.6", policy["system_libraries"])
        self.assertNotIn("libgomp.so.1", policy["system_libraries"])
        for key in ("glibc_max", "glibcxx_max", "cxxabi_max"):
            ABI.version_key(str(policy[key]))

    def test_the_command_checks_a_real_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-0.0.1-linux-x86_64.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                data = elf(["libc.so.6", "libgomp.so.1"], {"libc.so.6": ["GLIBC_2.38"]})
                member = tarfile.TarInfo("wally-linux-x86_64/bin/wally")
                member.size = len(data)
                member.mode = 0o755
                bundle.addfile(member, io.BytesIO(data))
            self.assertEqual(ABI.main([str(archive)]), 1)


if __name__ == "__main__":
    unittest.main()
