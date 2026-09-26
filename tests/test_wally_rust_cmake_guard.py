#!/usr/bin/env python3
"""cmake/WallyRust.cmake picks the Cargo profile and wally_link_probe's
LINK_PROBE_CONFIG from CMAKE_BUILD_TYPE, which multi-config generators
(Xcode, Visual Studio) leave empty at configure time. It must reject those
generators outright rather than silently build a mismatched release binary.

This drives WallyRust.cmake directly (not a reimplementation of the check),
through a throwaway fixture project that never touches the kit or the
network: the guard is the first substantive statement in the file, so it
fires (or does not) before anything kit-dependent runs.
"""

from __future__ import annotations

import pathlib
import shutil
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]

_FIXTURE = """\
cmake_minimum_required(VERSION 3.27)
project(wally_rust_guard_fixture LANGUAGES NONE)
set(CMAKE_MODULE_PATH "{repo_root}/cmake" ${{CMAKE_MODULE_PATH}})
include(WallyRust)
"""


@unittest.skipUnless(shutil.which("cmake"), "cmake not installed")
class WallyRustMultiConfigGuardTests(unittest.TestCase):
    def configure(self, generator: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as tmp:
            source = pathlib.Path(tmp) / "src"
            source.mkdir()
            (source / "CMakeLists.txt").write_text(_FIXTURE.format(repo_root=ROOT), encoding="utf-8")
            return subprocess.run(
                ["cmake", "-S", str(source), "-B", str(pathlib.Path(tmp) / "build"), "-G", generator],
                text=True, capture_output=True, check=False,
            )

    @unittest.skipUnless(shutil.which("ninja"), "ninja not installed")
    def test_multi_config_generator_is_rejected(self) -> None:
        result = self.configure("Ninja Multi-Config")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("requires a single-config generator", result.stdout + result.stderr)

    def test_single_config_generator_is_not_rejected_by_the_guard(self) -> None:
        # Ninja is single-config, so the guard must not fire; the fixture still
        # fails past it (it never includes the modules that define
        # wally_define_engine_macros() etc.), which is expected and fine here
        # -- only the multi-config message must be absent.
        result = self.configure("Ninja")
        self.assertNotIn("requires a single-config generator", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
