"""Help must work without installed coding tools, models, or credentials."""

import errno
import os
from pathlib import Path
import re
import select
import subprocess
import sys
import tempfile
import time
import unittest


WALLY = str(Path(sys.argv.pop(1)).resolve())
ANSI = re.compile(r"\x1b\[[0-9;]*m")


class HelpCliTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="wally-help-")
        self.addCleanup(self.directory.cleanup)
        self.env = dict(os.environ)
        self.env.update(
            HOME=self.directory.name,
            USERPROFILE=self.directory.name,
            WALLY_PROFILE_DIR=self.directory.name,
            RUNANYWHERE_HOME=self.directory.name,
            XDG_CONFIG_HOME=self.directory.name,
            TERM="xterm-256color",
        )
        self.env.pop("NO_COLOR", None)

    def run_help(self, *args, env=None):
        result = subprocess.run(
            [WALLY, *args], env=env or self.env,
            capture_output=True, text=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        return result.stdout

    def test_harness_help_does_not_launch_or_require_tool(self):
        env = dict(self.env, PATH=self.directory.name)
        for command in ("opencode", "claude-code", "claude-desktop", "hermes", "openclaw", "deepseek"):
            with self.subTest(command=command):
                help_text = self.run_help(command, "--help", env=env)
                self.assertIn(f"Usage: wally {command}", help_text)
                self.assertIn("Examples:", help_text)
                self.assertEqual(help_text, self.run_help("help", command, env=env))
                self.assertEqual(help_text, self.run_help(command, "-h", env=env))

    def test_piped_help_is_plain_and_examples_are_copyable(self):
        help_text = self.run_help("--help")
        self.assertNotIn("\x1b", help_text)
        self.assertIn("  # Download a local model", help_text)
        self.assertIn("\n  wally opencode -m qwen3-0.6b\n", help_text)
        self.assertIn("\n  wally account login && wally opencode --cloud -m glm-5.3-flash\n", help_text)
        for line in help_text.splitlines():
            self.assertEqual(line, line.rstrip())
            self.assertLessEqual(len(line), 80)

    @unittest.skipIf(os.name == "nt", "PTY color checks require a Unix terminal")
    def test_terminal_color_and_opt_outs(self):
        import pty

        def terminal_help(env, *args):
            master, slave = pty.openpty()
            process = subprocess.Popen([WALLY, *args, "--help"], env=env, stdout=slave, stderr=subprocess.PIPE)
            os.close(slave)
            try:
                # Drain while the child runs: terminal buffers can be smaller
                # than the help page. Bound a regression that launches a tool.
                deadline = time.monotonic() + 15
                chunks = []
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not select.select([master], [], [], remaining)[0]:
                        raise subprocess.TimeoutExpired([WALLY, *args, "--help"], 15)
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if not chunk:
                        break
                    chunks.append(chunk)
                process.wait(timeout=15)
                stderr = process.stderr.read().decode()
                self.assertEqual(process.returncode, 0, stderr)
                self.assertEqual(stderr, "")
                return b"".join(chunks).decode().replace("\r\n", "\n")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                process.stderr.close()
                os.close(master)

        plain = self.run_help("--help")
        colored = terminal_help(self.env)
        self.assertIn("\x1b[1;36m", colored)
        self.assertEqual(ANSI.sub("", colored), plain)
        for env, args in (
            (dict(self.env, TERM="dumb"), ()),
            (dict(self.env, NO_COLOR=""), ()),
            (self.env, ("--no-color",)),
        ):
            with self.subTest(term=env.get("TERM"), no_color="NO_COLOR" in env, args=args):
                self.assertEqual(terminal_help(env, *args), plain)


if __name__ == "__main__":
    unittest.main()
