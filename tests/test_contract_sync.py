#!/usr/bin/env python3
"""The CLI contract sync wrapper describes itself and refuses a pin with no provenance."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
SYNC = REPO / "contracts" / "sync_from_inferenceinfra.py"
EXTRACT = REPO / "contracts" / "wally-cli-v1.openapi.json"


def _check(*extra: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SYNC), "--check", *extra],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )


class ContractSyncTests(unittest.TestCase):
    def test_help_lists_from_and_check(self):
        result = subprocess.run(
            [sys.executable, str(SYNC), "--help"],
            cwd=REPO,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("--from", result.stdout)
        self.assertIn("--check", result.stdout)

    def test_committed_pin_passes_check(self):
        """The pin that ships is a good one. This is the hermetic CI gate."""
        result = _check()
        self.assertEqual(0, result.returncode, result.stderr + result.stdout)
        self.assertIn("Wally CLI lock OK", result.stdout)

    def _assert_refused(self, document: dict, missing: str) -> None:
        """--check must refuse `document` and name the field it wanted."""
        with tempfile.TemporaryDirectory() as directory:
            extract = Path(directory) / "wally-cli-v1.openapi.json"
            extract.write_text(json.dumps(document), encoding="utf-8")
            result = _check("--extract", str(extract))
        self.assertNotEqual(
            0, result.returncode, f"expected a refusal for missing {missing}"
        )
        self.assertIn("x-runanywhere-source", result.stderr)

    def test_check_refuses_extract_with_no_provenance(self):
        """Runs the refusal path every time, whatever the committed pin holds.

        The previous version of this test branched on the committed extract:
        once the pin was stamped -- the normal state -- the refusal path stopped
        executing, so dropping the provenance requirement from check_local()
        would not have failed anything.
        """
        document = json.loads(EXTRACT.read_text(encoding="utf-8"))
        document.pop("x-runanywhere-source", None)
        self._assert_refused(document, "the whole x-runanywhere-source block")

    def test_check_refuses_each_missing_provenance_field(self):
        """Every field check_local() requires is load-bearing, not just one."""
        for field in ("repository", "commit", "branch", "artifact"):
            with self.subTest(field=field):
                document = json.loads(EXTRACT.read_text(encoding="utf-8"))
                source = dict(document.get("x-runanywhere-source") or {})
                # A present-but-empty value must be refused too: that is exactly
                # what a half-stamped extract carries.
                source[field] = ""
                document["x-runanywhere-source"] = source
                self._assert_refused(document, field)


if __name__ == "__main__":
    unittest.main()
