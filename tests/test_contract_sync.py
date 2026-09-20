#!/usr/bin/env python3
"""The CLI contract sync wrapper describes itself and refuses a pin with no provenance."""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
SYNC = REPO / "contracts" / "sync_from_inferenceinfra.py"
EXTRACT = REPO / "contracts" / "wally-cli-v1.openapi.json"


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

    def test_check_without_from_requires_provenance(self):
        document = json.loads(EXTRACT.read_text(encoding="utf-8"))
        if not document.get("x-runanywhere-source", {}).get("commit"):
            result = subprocess.run(
                [sys.executable, str(SYNC), "--check"],
                cwd=REPO,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(0, result.returncode)
            self.assertIn("x-runanywhere-source", result.stderr)
            return
        result = subprocess.run(
            [sys.executable, str(SYNC), "--check"],
            cwd=REPO,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(0, result.returncode, result.stderr + result.stdout)
        self.assertIn("Wally CLI lock OK", result.stdout)


if __name__ == "__main__":
    unittest.main()
