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
    "interpreters": ["/lib64/ld-linux-x86-64.so.2"],
}


def elf(
    needed: list[str],
    versions: dict[str, list[str]],
    interp: str | None = "/lib64/ld-linux-x86-64.so.2",
) -> bytes:
    """A 64-bit little-endian ELF with DT_NEEDED entries, a verneed table, and
    (unless `interp` is None) a one-entry PT_INTERP program header -- the real
    allowed loader by default, so every existing fixture stays a realistic
    dynamically-linked executable; pass `interp=None` or a bogus path to
    exercise the PT_INTERP checks themselves.

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

    interp_bytes = interp.encode("ascii") + b"\0" if interp is not None else b""

    header_size = 64
    blobs = [bytes(strtab), dynamic, bytes(verneed), interp_bytes]
    offsets = []
    cursor = header_size
    for blob in blobs:
        offsets.append(cursor)
        cursor += len(blob)

    # A single PT_INTERP entry, placed between the data blobs and the section
    # table so the section table -- what the truncation tests below corrupt --
    # stays the last thing in the file, same as before this had one.
    if interp is not None:
        phoff = cursor
        phentsize = 56
        phnum = 1
        phdr = struct.pack(
            "<IIQQQQQQ", ABI.PT_INTERP, 4, offsets[3], 0, 0, len(interp_bytes), len(interp_bytes), 1
        )
        cursor += len(phdr)
    else:
        phoff = 0
        phentsize = 0
        phnum = 0
        phdr = b""
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
        "<HHIQQQIHHHHHH", 3, 62, 1, 0, phoff, shoff, 0, 64, phentsize, phnum, 64, len(sections), 0
    )
    assert len(header) == header_size
    return header + b"".join(blobs) + phdr + b"".join(sections)


class LinuxAbiTests(unittest.TestCase):
    def tree(self, files: dict[str, bytes]) -> dict[str, bytes]:
        return files

    def test_reads_needed_libraries_and_symbol_versions(self) -> None:
        needed, versions, interp = ABI.read_elf(
            elf(["libc.so.6", "libfoo.so.1"], {"libc.so.6": ["GLIBC_2.34", "GLIBC_2.2.5"]}),
            "wally",
        )
        self.assertEqual(needed, {"libc.so.6", "libfoo.so.1"})
        self.assertEqual(versions, {"GLIBC_2.34", "GLIBC_2.2.5"})
        self.assertEqual(interp, "/lib64/ld-linux-x86-64.so.2")

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
        self.assertEqual(ABI.check_files(root, POLICY), [])

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
        problems = ABI.check_files(root, POLICY)
        self.assertEqual(len(problems), 2, problems)
        self.assertTrue(any("GLIBC_2.38" in p for p in problems))
        self.assertTrue(any("GLIBCXX_3.4.32" in p for p in problems))

    def test_versions_compare_numerically_not_as_text(self) -> None:
        # "2.4" sorts after "2.35" as text; it is the older version.
        root = self.tree({"b/bin/wally": elf(["libc.so.6"], {"libc.so.6": ["GLIBC_2.4"]})})
        self.assertEqual(ABI.check_files(root, POLICY), [])
        cxxabi = self.tree(
            {"b/bin/wally": elf(["libstdc++.so.6"], {"libstdc++.so.6": ["CXXABI_1.3.14"]})}
        )
        self.assertEqual(len(ABI.check_files(cxxabi, POLICY)), 1)

    def test_an_unshipped_library_off_the_allow_list_fails(self) -> None:
        # v0.6.0 again: libgomp.so.1 was needed and not shipped.
        root = self.tree({"b/bin/wally": elf(["libc.so.6", "libgomp.so.1"], {})})
        problems = ABI.check_files(root, POLICY)
        self.assertEqual(len(problems), 1)
        self.assertIn("libgomp.so.1", problems[0])

    def test_other_libraries_own_version_tags_are_not_ceilings(self) -> None:
        root = self.tree(
            {"b/bin/wally": elf(["libc.so.6"], {"libssl.so.3": ["OPENSSL_3.0.0"]})}
        )
        self.assertEqual(ABI.check_files(root, POLICY), [])

    def test_glibc_abi_dt_relr_is_rejected_below_its_glibc_floor(self) -> None:
        # GLIBC_ABI_DT_RELR carries no version number for VERSION_TAG to
        # parse, so a bottle needing it silently passed against a 2.35 floor
        # (2.36 added the loader support DT_RELR needs) until this was fixed.
        root = self.tree(
            {"b/bin/wally": elf(["libc.so.6"], {"libc.so.6": ["GLIBC_ABI_DT_RELR"]})}
        )
        problems = ABI.check_files(root, POLICY)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("GLIBC_ABI_DT_RELR", problems[0])

    def test_glibc_abi_dt_relr_passes_once_the_floor_covers_it(self) -> None:
        policy = dict(POLICY, glibc_max="2.36")
        root = self.tree(
            {"b/bin/wally": elf(["libc.so.6"], {"libc.so.6": ["GLIBC_ABI_DT_RELR"]})}
        )
        self.assertEqual(ABI.check_files(root, policy), [])

    def test_numeric_glibc_tags_still_pass_alongside_the_marker_check(self) -> None:
        root = self.tree(
            {
                "b/bin/wally": elf(
                    ["libc.so.6"], {"libc.so.6": ["GLIBC_2.34", "GLIBC_ABI_DT_RELR"]}
                )
            }
        )
        problems = ABI.check_files(root, POLICY)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("GLIBC_ABI_DT_RELR", problems[0])

    def test_an_unrecognised_non_numeric_tag_in_a_checked_family_is_not_a_ceiling(self) -> None:
        # Only known markers are treated as a floor; an arbitrary non-numeric
        # tag in a checked family (none exist today, but the parser must not
        # invent a requirement for one) is silently ignored, same as before.
        root = self.tree(
            {"b/bin/wally": elf(["libc.so.6"], {"libc.so.6": ["GLIBC_SOMETHING_ELSE"]})}
        )
        self.assertEqual(ABI.check_files(root, POLICY), [])

    def test_the_shipped_executables_standard_interpreter_passes(self) -> None:
        # elf()'s default interp is the real x86-64 loader, so this is just
        # the same shape as every other passing test above, spelled out.
        root = self.tree(
            {"b/bin/wally": elf(["libc.so.6"], {}, interp="/lib64/ld-linux-x86-64.so.2")}
        )
        self.assertEqual(ABI.check_files(root, POLICY), [])

    def test_a_nonstandard_interpreter_fails(self) -> None:
        # A dynamic loader path off the allow-list would fail before any
        # DT_NEEDED library above is even tried; check_linux_abi must catch
        # it rather than only checking what loads after the loader.
        root = self.tree(
            {"b/bin/wally": elf(["libc.so.6"], {}, interp="/opt/custom/ld.so")}
        )
        problems = ABI.check_files(root, POLICY)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("/opt/custom/ld.so", problems[0])

    def test_a_shipped_executable_missing_pt_interp_fails(self) -> None:
        root = self.tree({"b/bin/wally": elf(["libc.so.6"], {}, interp=None)})
        problems = ABI.check_files(root, POLICY)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("no PT_INTERP", problems[0])

    def test_a_pt_interp_shorter_than_its_declared_path_is_rejected(self) -> None:
        # The loader only ever reads p_filesz bytes from a PT_INTERP segment.
        # Shrinking p_filesz to stop mid-path -- while the real NUL-terminated
        # string is still sitting in the file just past it -- is exactly what
        # a scan for the first NUL from p_offset, ignoring p_filesz entirely,
        # would still happily accept as the real interpreter.
        data = bytearray(elf(["libc.so.6"], {}))
        phoff = struct.unpack_from("<Q", data, 0x20)[0]
        struct.pack_into("<Q", data, phoff + 32, 4)  # p_filesz: "/lib", no NUL in range
        with self.assertRaises(ABI.AbiError) as raised:
            ABI.read_elf(bytes(data), "wally")
        self.assertIn("PT_INTERP", str(raised.exception))

    def test_a_shared_library_without_pt_interp_is_normal(self) -> None:
        # Every real .so this bottle ships is dynamically linked (has
        # DT_NEEDED) but is never exec'd directly, so it correctly has no
        # PT_INTERP; only the file under bin/ must have one.
        root = self.tree(
            {
                "b/bin/wally": elf(["libc.so.6", "libgomp.so.1"], {}),
                "b/lib/libgomp.so.1": elf(["libc.so.6"], {}, interp=None),
            }
        )
        self.assertEqual(ABI.check_files(root, POLICY), [])

    def test_an_archive_with_no_elf_is_not_a_bottle(self) -> None:
        root = self.tree({"b/README.md": b"hello"})
        self.assertEqual(len(ABI.check_files(root, POLICY)), 1)

    def test_a_truncated_elf_is_an_error_not_a_pass(self) -> None:
        whole = elf(["libc.so.6"], {"libc.so.6": ["GLIBC_2.34"]})
        # Cut inside the header and inside the section table: each must raise,
        # none may read as "needs nothing".
        for cut in (40, len(whole) - 10):
            with self.subTest(cut=cut), self.assertRaises(ABI.AbiError):
                ABI.read_elf(whole[:cut], "wally")

    def test_a_section_running_past_the_end_is_an_error(self) -> None:
        # The section table is intact; the verneed table it points at is not.
        # Move the table's offset past the file's end, which a slice would
        # otherwise read as an empty (and passing) table.
        data = bytearray(elf(["libc.so.6"], {"libc.so.6": ["GLIBC_2.38"]}))
        shoff = struct.unpack_from("<Q", data, 0x28)[0]
        verneed_header = shoff + 3 * 64
        struct.pack_into("<Q", data, verneed_header + 0x18, len(data) + 100)
        with self.assertRaises(ABI.AbiError) as raised:
            ABI.read_elf(bytes(data), "wally")
        self.assertIn("past the end", str(raised.exception))

    def test_an_elf_without_a_section_table_is_refused(self) -> None:
        data = bytearray(elf(["libc.so.6"], {}))
        struct.pack_into("<Q", data, 0x28, 0)  # e_shoff
        struct.pack_into("<H", data, 0x3C, 0)  # e_shnum
        with self.assertRaises(ABI.AbiError) as raised:
            ABI.read_elf(bytes(data), "wally")
        self.assertIn("no section header table", str(raised.exception))

    def test_the_policy_is_read_from_versions_toml(self) -> None:
        policy = ABI.read_linux_abi_policy()
        self.assertIn("libc.so.6", policy["system_libraries"])
        self.assertNotIn("libgomp.so.1", policy["system_libraries"])
        self.assertIn("/lib64/ld-linux-x86-64.so.2", policy["interpreters"])
        for key in ("glibc_max", "glibcxx_max", "cxxabi_max"):
            ABI.version_key(str(policy[key]))

    def test_the_archive_is_read_without_extracting_it(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = pathlib.Path(temporary) / "wally-0.0.1-linux-x86_64.tar.gz"
            data = elf(["libc.so.6"], {"libc.so.6": ["GLIBC_2.34"]})
            with tarfile.open(archive, "w:gz") as bundle:
                member = tarfile.TarInfo("wally-linux-x86_64/bin/wally")
                member.size = len(data)
                bundle.addfile(member, io.BytesIO(data))
                link = tarfile.TarInfo("wally-linux-x86_64/lib/escape")
                link.type = tarfile.SYMTYPE
                link.linkname = "/etc/passwd"
                bundle.addfile(link)
            files = ABI.archive_files(archive)
            self.assertEqual(list(files), ["wally-linux-x86_64/bin/wally"])
            self.assertEqual(ABI.main([str(archive)]), 0)
            self.assertEqual(sorted(p.name for p in pathlib.Path(temporary).iterdir()),
                             [archive.name])

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
