#!/usr/bin/env python3
"""scripts/ci/check-agents-sync.sh holds CLAUDE.md to the single line
`@AGENTS.md`, and actor.sh rejects a command it does not know.

Both run for real against a throwaway git repo or the real script. No build,
no network.
"""

from __future__ import annotations

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SYNC = ROOT / "scripts" / "ci" / "check-agents-sync.sh"


class AgentsSyncTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        (self.tmp / "scripts" / "ci").mkdir(parents=True)
        shutil.copy(SYNC, self.tmp / "scripts" / "ci")
        (self.tmp / "AGENTS.md").write_text("# rules\n", encoding="utf-8")
        for tree in (".claude/skills", ".agents/skills"):
            skill = self.tmp / tree / "wally-architecture"
            skill.mkdir(parents=True)
            (skill / "SKILL.md").write_text("skill\n", encoding="utf-8")
        self.git("init", "-q")
        self.git("add", "-A")

    def git(self, *args: str) -> None:
        subprocess.run(["git", *args], cwd=self.tmp, check=True)

    def sync(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", "scripts/ci/check-agents-sync.sh", *args],
            cwd=self.tmp, capture_output=True, text=True,
        )

    def write_claude(self, text: str) -> None:
        (self.tmp / "CLAUDE.md").write_text(text, encoding="utf-8")
        self.git("add", "CLAUDE.md")

    def test_import_line_passes(self) -> None:
        self.write_claude("@AGENTS.md\n")
        self.assertEqual(self.sync().returncode, 0)

    def test_extra_content_fails(self) -> None:
        self.write_claude("@AGENTS.md\nmore rules\n")
        result = self.sync()
        self.assertEqual(result.returncode, 1)
        self.assertIn("must contain only @AGENTS.md", result.stderr)

    def test_symlink_fails(self) -> None:
        os.symlink("AGENTS.md", self.tmp / "CLAUDE.md")
        self.git("add", "CLAUDE.md")
        result = self.sync()
        self.assertEqual(result.returncode, 1)
        self.assertIn("is a symlink", result.stderr)

    def test_fix_writes_import_line(self) -> None:
        os.symlink("AGENTS.md", self.tmp / "CLAUDE.md")
        self.assertEqual(self.sync("--fix").returncode, 0)
        claude = self.tmp / "CLAUDE.md"
        self.assertFalse(claude.is_symlink())
        self.assertEqual(claude.read_text(encoding="utf-8"), "@AGENTS.md\n")


class ActorTests(unittest.TestCase):
    def test_unknown_command_exits_2(self) -> None:
        result = subprocess.run(
            ["bash", str(ROOT / "actor.sh"), "no-such-command"],
            cwd=ROOT, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown command: no-such-command", result.stderr)


if __name__ == "__main__":
    unittest.main()
