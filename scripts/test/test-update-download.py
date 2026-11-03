"""Exercise the built Unix Wally updater against a loopback installer server.

The served scripts create and remove a scratch directory, record their arguments,
and print fixture output. No release is downloaded or existing installation changed.
Run on macOS and Linux: python3 scripts/test/test-update-download.py ./build/wally
"""

import hashlib
import os
import pathlib
import platform
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit


SCRIPT = b"""printf 'executed\\n' > "$WALLY_UPDATE_TEST_MARKER"
scratch=$(mktemp -d) || exit 1
rmdir "$scratch" || exit 1
printf '%s\\n' "$TMPDIR" > "$WALLY_UPDATE_TEST_TMPDIR"
printf '%s\\n' "$@" > "$WALLY_UPDATE_TEST_ARGS"
if [ "$1" = --check ]; then
    if [ "$WALLY_UPDATE_TEST_MODE" != empty ]; then printf '999.0.0\\n'; fi
else
    printf 'fixture installer ran\\n' >&2
fi
if [ "$WALLY_UPDATE_TEST_MODE" = installer-failure ]; then exit 7; fi
"""


class InstallerHandler(BaseHTTPRequestHandler):
    def log_message(self, _format, *_args):
        pass

    def do_GET(self):
        mode = parse_qs(urlsplit(self.path).query)["mode"][0]
        self.server.request_target = self.path
        self.server.requested.set()
        # Background cases hold the response until their download is visible.
        if not self.server.release.wait(10):
            return
        self.send_response(503 if mode == "http-failure" else 200)
        self.send_header("Content-Length", str(len(SCRIPT) + (100 if mode == "partial" else 0)))
        self.end_headers()
        self.wfile.write(SCRIPT)
        self.wfile.flush()
        # A complete marker-writing statement is received, but curl must
        # reject the transfer because the declared response is incomplete.
        self.close_connection = True


class Fixture:
    def __init__(self, root, mode, server, tempdir="usable"):
        self.root = root
        self.server = server
        self.server.request_target = None
        self.request_target = f"/installer%20script.sh?mode={mode}&quote='literal'"
        self.downloads = root / "temporary downloads"
        self.state = root / "state" / "runanywhere"
        self.marker = root / "executed"
        self.arguments = root / "arguments"
        self.installer_tmpdir = root / "installer-tempdir"
        self.downloads.mkdir()
        self.state.mkdir(parents=True)
        # Preserve runtime library paths, but isolate configuration and curl's
        # HOME and prevent an inherited update override or proxy from escaping.
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("WALLY_", "RUNANYWHERE_", "XDG_"))
                    and key.lower() not in {"http_proxy", "https_proxy", "all_proxy"}}
        self.env.pop("HOMEBREW_PREFIX", None)
        self.env.update(
            HOME=str(root), XDG_CONFIG_HOME=str(root / "config"),
            XDG_DATA_HOME=str(root / "data"), XDG_STATE_HOME=str(root / "state"),
            RUNANYWHERE_HOME=str(root / "models"), WALLY_PROFILE_DIR=str(root / "profile"),
            TMPDIR=str(self.downloads), NO_PROXY="127.0.0.1", no_proxy="127.0.0.1",
            WALLY_UPDATE_SCRIPT_URL=f"http://127.0.0.1:{server.server_port}{self.request_target}",
            WALLY_UPDATE_TEST_MODE=mode, WALLY_UPDATE_TEST_MARKER=str(self.marker),
            WALLY_UPDATE_TEST_ARGS=str(self.arguments),
            WALLY_UPDATE_TEST_TMPDIR=str(self.installer_tmpdir),
        )
        self.fallback = tempdir != "usable"
        if self.fallback:
            unusable = root / "unusable temporary directory"
            if tempdir == "not-directory":
                unusable.write_text("not a directory\n")
            self.env["TMPDIR"] = str(unusable)

    def assert_request_target(self):
        """Check that the encoded path and complete query reached the server."""
        assert self.server.request_target == self.request_target, self.server.request_target

    def assert_installer_tmpdir(self):
        """The installer can create its own scratch directory in the selected location."""
        expected = "/tmp" if self.fallback else str(self.downloads)
        assert self.installer_tmpdir.read_text().strip() == expected

    def assert_clean(self):
        assert not list(self.downloads.iterdir()), "installer temporary file leaked"
        assert not list(self.state.glob("update-check.*")), "cache temporary file leaked"


def invoke(binary, env, *args):
    return subprocess.run([binary, *args], env=env, input="", capture_output=True,
                          text=True, timeout=20, check=False)


def wait_until(predicate, message):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(message)


def foreground(binary, fixture, mode, args, version):
    """Check CLI status as well as whether any received script bytes ran."""
    fixture.env["CI"] = "1"
    if mode == "connection-failure":
        with socket.socket() as closed_port:
            closed_port.bind(("127.0.0.1", 0))
            port = closed_port.getsockname()[1]
        fixture.env["WALLY_UPDATE_SCRIPT_URL"] = f"http://127.0.0.1:{port}/installer.sh"
    result = invoke(binary, fixture.env, *args)
    if mode != "connection-failure":
        fixture.assert_request_target()
    failed_download = mode in {"http-failure", "connection-failure", "partial"}
    expected = 1 if failed_download or mode == "installer-failure" else 0
    assert result.returncode == expected, (args, mode, result.returncode, result.stderr)
    if failed_download:
        assert "curl:" in result.stderr, result.stderr
        assert not fixture.marker.exists(), "failed download executed script bytes"
        assert "fixture installer ran" not in result.stderr, result.stderr
    else:
        assert fixture.marker.read_text() == "executed\n"
        fixture.assert_installer_tmpdir()
        assert fixture.arguments.read_text().splitlines() == [f"--version={version}"]
        assert "fixture installer ran" in result.stderr, result.stderr
    fixture.assert_clean()


def background(binary, fixture, mode, server, fresh=False):
    """Reach the real notice through TTYs, then wait for download cleanup."""
    import pty

    fixture.env.pop("CI", None)
    cache = fixture.state / "update-check"
    cache.write_text("0.0.1\n")
    if not fresh:
        old = time.time() - 2 * 24 * 60 * 60
        os.utime(cache, (old, old))
    initial_mtime = cache.stat().st_mtime_ns
    server.requested.clear()
    server.release.clear()
    master, slave = pty.openpty()
    try:
        with subprocess.Popen([binary, "--no-color", "--help"], env=fixture.env,
                              stdin=slave, stderr=slave, stdout=subprocess.PIPE) as process:
            try:
                stdout, _ = process.communicate(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate()
                raise
            assert process.returncode == 0, stdout
        if fresh:
            assert not server.requested.wait(5), "fresh cache triggered a refresh"
            assert cache.read_text() == "0.0.1\n"
            assert cache.stat().st_mtime_ns == initial_mtime, "fresh cache was touched"
            fixture.assert_clean()
            return
        assert server.requested.wait(10), "interactive help did not request a refresh"
        fixture.assert_request_target()
        if not fixture.fallback:
            wait_until(lambda: list(fixture.downloads.iterdir())
                       or list(fixture.state.glob("update-check.*")),
                       "background download never started")
        server.release.set()
        if fixture.fallback:
            wait_until(lambda: cache.read_text() == "999.0.0\n",
                       "background refresh did not fall back from unusable TMPDIR")
        wait_until(lambda: not list(fixture.downloads.iterdir())
                   and not list(fixture.state.glob("update-check.*")),
                   "background refresh did not clean up")
        assert cache.read_text() == ("999.0.0\n" if mode == "success" else "0.0.1\n"), (
            mode, cache.read_text())
        assert cache.stat().st_mtime > old, "failed checks must still be throttled"
        if mode in {"http-failure", "partial"}:
            assert not fixture.marker.exists(), "failed refresh download executed script bytes"
        else:
            assert fixture.marker.read_text() == "executed\n"
            fixture.assert_installer_tmpdir()
            assert fixture.arguments.read_text().splitlines() == ["--check"]
        fixture.assert_clean()
    finally:
        server.release.set()
        os.close(slave)
        os.close(master)
        wait_until(lambda: not list(fixture.downloads.iterdir())
                   and not list(fixture.state.glob("update-check.*")),
                   "background refresh did not clean up before fixture removal")


def main():
    if len(sys.argv) != 2 or sys.platform not in {"darwin", "linux"}:
        raise SystemExit("usage (macOS/Linux): test-update-download.py /path/to/built/wally")
    binary = str(pathlib.Path(sys.argv[1]).resolve(strict=True))
    print(f"platform: {platform.platform()}")
    print(f"binary SHA-256: {hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest()}")
    server = ThreadingHTTPServer(("127.0.0.1", 0), InstallerHandler)
    server.requested = threading.Event()
    server.release = threading.Event()
    server.release.set()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    failures = []
    checks = 0

    def check(name, run):
        nonlocal checks
        checks += 1
        with tempfile.TemporaryDirectory(prefix="wally update ") as directory:
            try:
                run(pathlib.Path(directory))
            except (AssertionError, OSError, subprocess.SubprocessError) as error:
                failures.append(name)
                print(f"FAIL {name}: {error}", flush=True)
            else:
                print(f"ok {name}", flush=True)

    try:
        with tempfile.TemporaryDirectory(prefix="wally update version ") as directory:
            fixture = Fixture(pathlib.Path(directory), "success", server)
            result = invoke(binary, fixture.env, "--version")
            assert result.returncode == 0, result.stderr
            version_match = re.search(r"\d+\.\d+\.\d+(?:[-+][\w.-]+)?", result.stdout)
            assert version_match, result.stdout
            version = version_match.group()
        for args in (["update"], ["-u"], ["--update"]):
            for mode in ("http-failure", "connection-failure", "partial", "success", "installer-failure"):
                check(f"{' '.join(args)} / {mode}",
                      lambda root: foreground(binary, Fixture(root, mode, server), mode, args, version))
        for mode in ("http-failure", "partial", "success", "installer-failure", "empty"):
            check(f"background / {mode}",
                  lambda root: background(binary, Fixture(root, mode, server), mode, server))
        for tempdir in ("missing", "not-directory"):
            check(f"update / {tempdir} TMPDIR",
                  lambda root: foreground(binary, Fixture(root, "success", server, tempdir),
                                          "success", ["update"], version))
            check(f"background / {tempdir} TMPDIR",
                  lambda root: background(binary, Fixture(root, "success", server, tempdir),
                                         "success", server))
        check("background / fresh cache",
              lambda root: background(binary, Fixture(root, "success", server),
                                      "success", server, fresh=True))
    finally:
        server.release.set()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
    if failures:
        raise SystemExit(f"{len(failures)} update download checks failed")
    print(f"all {checks} update download checks passed")


if __name__ == "__main__":
    main()
