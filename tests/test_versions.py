#!/usr/bin/env python3
"""Regression coverage for the exact SDK workflow checkout pin."""

from __future__ import annotations

import importlib.util
import pathlib
import re
import subprocess
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("check_versions", ROOT / "scripts/ci/check-versions.py")
assert SPEC is not None and SPEC.loader is not None
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


def workflow(ref: str | None, other_ref: str = "unrelated-release") -> str:
    reference = "" if ref is None else f"          ref: {ref}\n"
    return (
        "jobs:\n"
        "  macos:\n"
        "    steps:\n"
        "      - uses: actions/checkout@v4\n"
        "        with:\n"
        f"          ref: {other_ref}\n"
        "      - name: Checkout SDK\n"
        "        uses: actions/checkout@v4\n"
        "        with:\n"
        "          repository: RunanywhereAI/runanywhere-sdks\n"
        + reference
        + "          path: .deps/runanywhere-sdks\n"
    )


class SdkCheckoutRefTests(unittest.TestCase):
    def check(self, text: str, expected: str) -> list[str]:
        return CHECK.check_sdk_checkout_refs(text, expected, "fixture.yml")

    def test_full_tag_sha_and_prerelease_refs_match(self):
        for ref in ("v0.20.31", "v0.20.38-rc.2+build.4", "cpp-desktop-v0.20.38-rc.1",
                    "c432e10df5d9e49c501ebf21b64dc9657f641a02"):
            with self.subTest(ref=ref):
                self.assertEqual(self.check(workflow(ref), ref), [])

    def test_mismatched_refs_cannot_be_skipped_or_truncated(self):
        for found, expected in (("v0.20.31", "v0.20.30"),
                                ("v0.20.38-rc.2", "v0.20.38-rc.1"),
                                ("a" * 40, "b" * 40), ("v0.20.31", "0.20.31")):
            with self.subTest(found=found, expected=expected):
                errors = self.check(workflow(found), expected)
                self.assertEqual(len(errors), 1)
                self.assertIn(repr(found), errors[0])
                self.assertIn(repr(expected), errors[0])

    def test_missing_empty_or_null_sdk_ref_fails(self):
        for ref in (None, "", "''", '""', "null", "~"):
            with self.subTest(ref=ref):
                errors = self.check(workflow(ref), "v0.20.31")
                self.assertEqual(len(errors), 1)
                self.assertIn("missing an explicit ref", errors[0])

    def test_other_checkout_ref_cannot_supply_missing_sdk_ref(self):
        errors = self.check(workflow(None, "v0.20.31"), "v0.20.31")
        self.assertEqual(len(errors), 1)
        self.assertIn("missing an explicit ref", errors[0])

    def test_other_repository_refs_do_not_mismatch_sdk_pin(self):
        for unrelated in ("v9.8.7", "main", "${{ github.sha }}", "c" * 40):
            with self.subTest(unrelated=unrelated):
                self.assertEqual(self.check(workflow("v0.20.31", unrelated), "v0.20.31"), [])

    def test_quoted_values_comments_and_input_order(self):
        for ref in ("'v0.20.31' # checked release", '"v0.20.31" # checked release'):
            text = workflow(ref).replace(
                "          repository: RunanywhereAI/runanywhere-sdks\n",
                "          repository: 'RunanywhereAI/runanywhere-sdks' # SDK\n",
            )
            self.assertEqual(self.check(text, "v0.20.31"), [])
        text = workflow("v0.20.31").replace(
            "          repository: RunanywhereAI/runanywhere-sdks\n          ref: v0.20.31",
            "          ref: v0.20.31\n          repository: RunanywhereAI/runanywhere-sdks",
        )
        self.assertEqual(self.check(text, "v0.20.31"), [])

    def test_every_sdk_checkout_is_checked(self):
        text = workflow("v0.20.31") + workflow("a" * 40).removeprefix("jobs:\n").replace("  macos:", "  other:")
        errors = self.check(text, "v0.20.31")
        self.assertEqual(len(errors), 1)
        self.assertIn("'" + "a" * 40 + "'", errors[0])

    def test_missing_sdk_checkout_fails_even_with_sdk_in_comments_or_script(self):
        text = (
            "jobs:\n  build:\n    steps:\n"
            "      - uses: actions/checkout@v4\n"
            "      - name: Script mentions runanywhere-sdks\n"
            "        run: |\n"
            "          # repository: RunanywhereAI/runanywhere-sdks\n"
            "          - uses: actions/checkout@v4\n"
            "            with:\n"
            "              repository: RunanywhereAI/runanywhere-sdks\n"
            "              ref: v0.20.31\n"
        )
        errors = self.check(text, "v0.20.31")
        self.assertEqual(len(errors), 1)
        self.assertIn("no explicit actions/checkout", errors[0])

    def test_adjacent_step_cannot_supply_the_sdk_ref(self):
        text = workflow(None) + (
            "      - uses: actions/checkout@v4\n"
            "        with:\n"
            "          repository: other/project\n"
            "          ref: v0.20.31\n"
        )
        errors = self.check(text, "v0.20.31")
        self.assertEqual(len(errors), 1)
        self.assertIn("missing an explicit ref", errors[0])

    def test_non_checkout_action_cannot_count_as_sdk_checkout(self):
        text = workflow("v0.20.31").replace("actions/checkout@v4", "example/action@v4")
        errors = self.check(text, "v0.20.31")
        self.assertEqual(len(errors), 1)
        self.assertIn("no explicit actions/checkout", errors[0])

    def test_duplicate_and_dynamic_ref_fail_closed(self):
        text = workflow("v0.20.31").replace("          ref: v0.20.31", "          ref: v0.20.31\n          ref: v0.20.31")
        self.assertIn("duplicate SDK checkout input", self.check(text, "v0.20.31")[0])
        dynamic = "${{ github.sha }}"
        self.assertNotEqual(self.check(workflow(dynamic), dynamic), [])

    def test_repository_version_gate_passes(self):
        result = subprocess.run([sys.executable, str(ROOT / "scripts/ci/check-versions.py")],
                                text=True, capture_output=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("versions.toml consistent", result.stdout)


class CMakeFileApiFloorTests(unittest.TestCase):
    """cmake_file_api(QUERY ...) in cmake/WallyRust.cmake only takes effect
    inside the run that asks for it from CMake 3.27 on; older CMake defers the
    reply to the next configure, so the first cargo build would run before it
    exists. cmake_minimum_required must stay at 3.27+ so a too-old CMake fails
    fast at configure instead of confusingly inside the first cargo build."""

    def test_cmake_minimum_required_is_at_least_3_27(self):
        found = re.search(r"cmake_minimum_required\(VERSION\s+([0-9.]+)\)", CHECK.CMAKELISTS.read_text())
        self.assertIsNotNone(found, "no cmake_minimum_required in CMakeLists.txt")
        self.assertGreaterEqual(
            tuple(int(part) for part in found.group(1).split(".")),
            (3, 27),
            "cmake_minimum_required regressed below the CMake file API's same-run floor",
        )

    def test_wally_rust_cmake_has_no_pre_3_27_fallback_branch(self):
        text = (CHECK.ROOT / "cmake/WallyRust.cmake").read_text()
        self.assertIn("cmake_file_api(QUERY API_VERSION 1 CODEMODEL 2)", text)
        self.assertNotIn("CMAKE_VERSION VERSION_GREATER_EQUAL 3.27", text)


if __name__ == "__main__":
    unittest.main()
