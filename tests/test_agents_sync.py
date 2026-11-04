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
            cwd=self.tmp, capture_output=True, text=True, check=False,
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


def actor(*args: str, **env: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(ROOT / "actor.sh"), *args],
        cwd=ROOT, capture_output=True, text=True, check=False,
        env={**os.environ, "NO_COLOR": "1", **env},
    )


class ActorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)

    def test_unknown_command_exits_2(self) -> None:
        result = actor("no-such-command")
        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown command: no-such-command", result.stderr)

    def test_kit_keeps_a_kit_it_did_not_download(self) -> None:
        kit = self.tmp / "my-kit"
        (kit / "share" / "runanywhere").mkdir(parents=True)
        (kit / "share" / "runanywhere" / "SCHEMA_LOCK").write_text("lock\n", encoding="utf-8")
        (kit / "IMPORTANT.txt").write_text("keep\n", encoding="utf-8")
        result = actor("kit", WALLY_KIT_DIR=str(kit))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((kit / "IMPORTANT.txt").exists())

    def test_kit_refuses_a_directory_that_is_not_a_kit(self) -> None:
        (self.tmp / "IMPORTANT.txt").write_text("keep\n", encoding="utf-8")
        result = actor("kit", WALLY_KIT_DIR=str(self.tmp))
        self.assertEqual(result.returncode, 2)
        self.assertTrue((self.tmp / "IMPORTANT.txt").exists())

    def test_clean_refuses_a_directory_that_is_not_a_build_tree(self) -> None:
        (self.tmp / "IMPORTANT.txt").write_text("keep\n", encoding="utf-8")
        result = actor("clean", WALLY_BUILD_DIR=str(self.tmp))
        self.assertEqual(result.returncode, 2)
        self.assertIn("not a build tree", result.stderr)
        self.assertTrue((self.tmp / "IMPORTANT.txt").exists())

    def test_clean_removes_a_configured_build_tree(self) -> None:
        build = self.tmp / "build"
        build.mkdir()
        (build / "CMakeCache.txt").write_text("cache\n", encoding="utf-8")
        result = actor("clean", WALLY_BUILD_DIR=str(build))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(build.exists())

    def test_test_does_not_require_a_kit(self) -> None:
        result = actor("test", WALLY_KIT_DIR=str(self.tmp), WALLY_BUILD_DIR=str(self.tmp / "nobuild"))
        self.assertIn("not configured", result.stderr)

    def test_sdk_does_not_move_a_user_checkout(self) -> None:
        sdk = self.tmp / "runanywhere-sdks"
        sdk.mkdir()
        (sdk / "Package.swift").write_text("// swift\n", encoding="utf-8")
        git = ["git", "-C", str(sdk), "-c", "user.name=t", "-c", "user.email=t@t"]
        subprocess.run([*git, "init", "-q", "-b", "my-feature"], check=True)
        subprocess.run([*git, "add", "-A"], check=True)
        subprocess.run([*git, "commit", "-q", "-m", "local work"], check=True)
        before = subprocess.run([*git, "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout

        result = actor("sdk", WALLY_SDK_SWIFT_PATH=str(sdk), WALLY_APPLE_MLX_HOST="ON")

        self.assertEqual(result.returncode, 0, result.stderr)
        after = subprocess.run([*git, "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout
        branch = subprocess.run([*git, "branch", "--show-current"], capture_output=True, text=True, check=True).stdout
        self.assertEqual(before, after)
        self.assertEqual(branch.strip(), "my-feature")
        self.assertFalse((sdk / ".git" / "shallow").exists())


if __name__ == "__main__":
    unittest.main()
