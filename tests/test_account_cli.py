#!/usr/bin/env python3
"""Hermetic CLI test for browser-approved cloud authentication."""

import json
import os
import pathlib
import stat
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ACCESS_TOKEN = "account-e2e-access-secret"
REFRESH_TOKEN = "account-e2e-refresh-secret"
EMAIL = "developer@example.test"

# The shape InferenceInfra's CliUsageResponse actually returns. `cached_tokens`
# is 0 in the 24h window on purpose: SGLang does not report cached tokens for
# glm-5.3, so zero is what a real console sends today and the row has to survive
# it honestly rather than disappear. `timeline`, `models` and `recent` are
# present because the console sends them; `wally usage` ignores all three.
USAGE_WINDOWS = [
    {
        "window": "1h",
        "seconds": 3_600,
        "totals": {
            "requests": 9,
            "prompt_tokens": 18_450,
            "completion_tokens": 1_902,
            "cached_tokens": 512,
            "cost_micros": 74_000,
        },
    },
    {
        "window": "24h",
        "seconds": 86_400,
        "totals": {
            "requests": 214,
            "prompt_tokens": 412_900,
            "completion_tokens": 31_204,
            "cached_tokens": 0,
            "cost_micros": 1_830_000,
        },
    },
]

USAGE_BODY = {
    "credit": {
        "balance_micros": 18_420_000,
        "granted_micros": 25_000_000,
        "spent_micros": 6_580_000,
    },
    "totals": {
        "requests": 214,
        "prompt_tokens": 412_900,
        "completion_tokens": 31_204,
        "cached_tokens": 0,
        "cost_micros": 1_830_000,
    },
    "windows": USAGE_WINDOWS,
    "timeline": [{"date": "2026-09-04", "requests": 214, "prompt_tokens": 412_900,
                  "completion_tokens": 31_204, "cost_micros": 1_830_000}],
    "models": [{"model": "glm-5.3", "requests": 214, "prompt_tokens": 412_900,
                "completion_tokens": 31_204, "cached_tokens": 0, "cost_micros": 1_830_000}],
    "recent": [{"request_id": "req-1", "model": "glm-5.3", "harness": "opencode",
                "started_at": "2026-09-04T02:10:00Z", "prompt_tokens": 1_900,
                "completion_tokens": 140, "cached_tokens": 0, "cost_micros": 8_600,
                "ttft_ms": 240, "status_code": 200, "error_code": ""}],
}


class ConsoleHandler(BaseHTTPRequestHandler):
    requests = []
    console_origin = ""
    # False stands in for every console deployed before windowed totals, which
    # is all of them until /v1/cli/usage ships.
    serves_windows = True

    def log_message(self, _format, *_args):
        return

    def read_json(self):
        length = int(self.headers.get("Content-Length", "0"))
        return json.loads(self.rfile.read(length).decode("utf-8") or "{}")

    def reply(self, status, body=None):
        encoded = b"" if body is None else json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_POST(self):
        body = self.read_json()
        self.requests.append(("POST", self.path, self.headers.get("Authorization"), body))
        if self.path == "/v1/auth/cli/start":
            self.reply(
                200,
                {
                    "request_code": "ABCD-EFGH",
                    "poll_secret": "poll-secret",
                    "verification_url": self.console_origin + "/device?code=ABCD-EFGH",
                    "expires_in": 60,
                    "interval": 1,
                },
            )
        elif self.path == "/v1/auth/cli/poll":
            self.reply(
                200,
                {
                    "status": "approved",
                    "access_token": ACCESS_TOKEN,
                    "refresh_token": REFRESH_TOKEN,
                    "email": EMAIL,
                    "expires_in": 3600,
                },
            )
        elif self.path == "/v1/auth/cli/revoke":
            self.reply(204)
        else:
            self.reply(404, {"error": "unknown path"})

    def do_GET(self):
        self.requests.append(("GET", self.path, self.headers.get("Authorization"), None))
        if self.path == "/v1/auth/me":
            self.reply(200, {"email": EMAIL})
        elif self.path.startswith("/v1/cli/usage"):
            body = dict(USAGE_BODY)
            if not self.serves_windows:
                body.pop("windows")
            self.reply(200, body)
        else:
            self.reply(404, {"error": "unknown path"})


def run(binary, arguments, environment):
    result = subprocess.run(
        [binary, *arguments],
        env=environment,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    combined = result.stdout + result.stderr
    if result.returncode != 0:
        raise AssertionError(
            f"{' '.join(arguments)} returned {result.returncode}:\n{combined}"
        )
    if ACCESS_TOKEN in combined or REFRESH_TOKEN in combined:
        raise AssertionError(f"{' '.join(arguments)} exposed a cloud token")
    return combined


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: test_account_cli.py /path/to/wally")
    binary = sys.argv[1]
    server = ThreadingHTTPServer(("127.0.0.1", 0), ConsoleHandler)
    ConsoleHandler.console_origin = f"http://127.0.0.1:{server.server_port}"
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    try:
        with tempfile.TemporaryDirectory(prefix="wally-account-e2e-") as profile:
            environment = os.environ.copy()
            environment["WALLY_PROFILE_DIR"] = profile
            environment["WALLY_CONSOLE_URL"] = ConsoleHandler.console_origin
            for name in (
                "RUNANYWHERE_API_KEY",
                "RUNANYWHERE_API_SECRET",
                "RUNANYWHERE_ENVIRONMENT",
            ):
                environment.pop(name, None)

            login = run(binary, ["login", "--no-browser"], environment)
            if "ABCD-EFGH" not in login or ConsoleHandler.console_origin not in login:
                raise AssertionError("login did not print the approval code and URL")

            files = list(pathlib.Path(profile).iterdir())
            if len(files) != 1:
                raise AssertionError("login did not create exactly one session file")
            if os.name != "nt":
                directory_mode = stat.S_IMODE(os.stat(profile).st_mode)
                file_mode = stat.S_IMODE(os.stat(files[0]).st_mode)
                if directory_mode != 0o700 or file_mode != 0o600:
                    raise AssertionError(
                        f"unsafe credential modes: {directory_mode:o}/{file_mode:o}"
                    )

            whoami = run(binary, ["whoami"], environment)
            if EMAIL not in whoami or "session" not in whoami or "active" not in whoami:
                raise AssertionError("whoami did not report the active identity")
            if "plan" in whoami or "tokens" in whoami or "quota" in whoami:
                raise AssertionError("whoami exposed launch-out-of-scope billing fields")

            usage = run(binary, ["usage"], environment)
            # What San asked for and nothing else: the balance, then input,
            # output, cache and money over two windows.
            for fragment in ("$18.42", "$25.00", "input", "output", "cache", "spend"):
                if fragment not in usage:
                    raise AssertionError(f"usage did not report {fragment!r}:\n{usage}")
            # Each row comes from its own window, not from `totals`. The 1h row
            # carrying the 24h numbers is the bug this pins.
            hour = next(l for l in usage.splitlines() if l.startswith("past 1h"))
            if hour.split() != ["past", "1h", "18,450", "1,902", "512", "$0.0740"]:
                raise AssertionError(f"unexpected 1h row: {hour!r}")
            # A zero cache column is the truth for glm-5.3, not a reason to drop
            # the row. The 24h row must carry a literal 0, not a blank.
            day = next(l for l in usage.splitlines() if l.startswith("past 24h"))
            if day.split() != ["past", "24h", "412,900", "31,204", "0", "$1.83"]:
                raise AssertionError(f"unexpected 24h row: {day!r}")
            # Everything the old report printed and San told us to delete.
            for banned in ("by day", "by model", "ttft", "req ", "#", "request_id"):
                if banned in usage:
                    raise AssertionError(f"usage still prints {banned!r}:\n{usage}")

            # The root flag and the command flag mean the same thing. The root
            # parser accepts `wally --json usage`, and reading only the local
            # flag printed a human table to something asking for one document.
            for argv in (["usage", "--json"], ["--json", "usage"]):
                combined = run(binary, argv, environment)
                # run() concatenates stderr, where status lines and SDK logs go.
                # The document is the one line that is a JSON object.
                line = next((l for l in combined.splitlines() if l.startswith("{")), "")
                if not line:
                    raise AssertionError(f"{argv} printed no JSON document:\n{combined}")
                document = json.loads(line)
                if document["windows"][0]["input_tokens"] != 18_450:
                    raise AssertionError(f"{argv} did not report the 1h window: {document}")

            # A console that has not shipped windowed totals yet — which is
            # every deployed one right now. Both rows must read as absent, and
            # neither may be filled in from the month-wide `totals` next to it.
            ConsoleHandler.serves_windows = False
            try:
                stale = run(binary, ["usage"], environment)
            finally:
                ConsoleHandler.serves_windows = True
            for label in ("past 1h", "past 24h"):
                row = next(l for l in stale.splitlines() if l.startswith(label))
                if row.split()[-4:] != ["-", "-", "-", "-"]:
                    raise AssertionError(f"{label} invented numbers the console never sent: {row!r}")
            if "412,900" in stale:
                raise AssertionError(f"a window was filled in from `totals`:\n{stale}")
            if "$18.42" not in stale:
                raise AssertionError(f"the balance is known and must still print:\n{stale}")

            run(binary, ["logout"], environment)
            if list(pathlib.Path(profile).iterdir()):
                raise AssertionError("logout did not remove the local session")

        expected = [
            ("POST", "/v1/auth/cli/start", None),
            ("POST", "/v1/auth/cli/poll", None),
            ("GET", "/v1/auth/me", f"Bearer {ACCESS_TOKEN}"),
            # One read per invocation. The windows are totalled server-side, so
            # `days` and `limit` are held at the minimum the route accepts —
            # nothing below the balance renders `totals`, `timeline` or `recent`.
            ("GET", "/v1/cli/usage?days=1&limit=1", f"Bearer {ACCESS_TOKEN}"),
            ("GET", "/v1/cli/usage?days=1&limit=1", f"Bearer {ACCESS_TOKEN}"),
            ("GET", "/v1/cli/usage?days=1&limit=1", f"Bearer {ACCESS_TOKEN}"),
            ("GET", "/v1/cli/usage?days=1&limit=1", f"Bearer {ACCESS_TOKEN}"),
            ("POST", "/v1/auth/cli/revoke", f"Bearer {ACCESS_TOKEN}"),
        ]
        actual = [(method, path, authorization) for method, path, authorization, _ in ConsoleHandler.requests]
        if actual != expected:
            raise AssertionError(f"unexpected console request sequence: {actual!r}")
        # Looked up by path, not by index: a new call anywhere in the flow
        # renumbers the list and would otherwise silently assert the wrong body.
        bodies = {path: body for _, path, _, body in ConsoleHandler.requests}
        if bodies["/v1/auth/cli/poll"] != {
            "request_code": "ABCD-EFGH",
            "poll_secret": "poll-secret",
        }:
            raise AssertionError("poll request did not use the server-issued secret")
        if bodies["/v1/auth/cli/revoke"] != {"refresh_token": REFRESH_TOKEN}:
            raise AssertionError("logout did not request refresh-token revocation")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    print("account CLI browser flow passed")


if __name__ == "__main__":
    main()
