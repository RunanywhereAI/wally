#!/usr/bin/env python3
"""Hermetic CLI test for browser-approved cloud authentication."""

import json
import os
import pathlib
import re
import stat
import subprocess
import urllib.parse
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
# present because the console sends them; `wally account usage` ignores all three.
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


EXPORT_FIRST = "/v1/cli/usage/requests?since=*&until=*&limit=100"
# --follow asks for the route's maximum page so the walk is as few pages as it can be.
EXPORT_FOLLOW_FIRST = "/v1/cli/usage/requests?since=*&until=*&limit=200"
EXPORT_FOLLOW_SECOND = EXPORT_FOLLOW_FIRST + "&cursor=p2"
# Every page of a filtered walk repeats the filters; the fake refuses one that does not.
EXPORT_FILTERS = "&model=glm-5.3-flash&status_code=200&response_request_id=resp-a"
EXPORT_FILTERED_FIRST = EXPORT_FOLLOW_FIRST + EXPORT_FILTERS
EXPORT_FILTERED_SECOND = EXPORT_FILTERED_FIRST + "&cursor=p2"
# A model the fake console refuses with the contract's ApiError, as the real one
# refuses a query it cannot serve.
REFUSED_MODEL = "refused-model"
REFUSAL = "cursor does not belong to this window or filter set"


def export_record(request_id, status=200, error=None, ttft=310, provider="self_hosted_sglang"):
    return {
        "request_id": request_id, "response_request_id": "resp-" + request_id,
        "model": "glm-5.3-flash", "provider": provider,
        "status_code": status, "error_code": error, "finish_reason": "stop",
        "stream": True, "ts_start": "2026-09-25T08:00:00.123456+00:00",
        "ts_end": None, "recorded_at": "2026-09-25T08:00:02+00:00",
        "prompt_tokens": 1_200, "cached_tokens": 1_000, "noncached_prompt_tokens": 200,
        "completion_tokens": 40, "reasoning_tokens": 0, "ttft_ms": ttft,
        "cost_micros": 1_500, "pricing_version": "v1",
    }


EXPORT_TOTALS = {
    "requests": 3, "prompt_tokens": 3_600, "cached_tokens": 3_000,
    "noncached_prompt_tokens": 600, "completion_tokens": 120, "reasoning_tokens": 0,
    "cost_micros": 4_500,
}

# Keyed by the cursor that asks for the page; None is the first.
EXPORT_PAGES = {
    None: {"as_of": "2026-09-25T08:34:18Z", "totals": EXPORT_TOTALS,
           "requests": [export_record("a"), export_record("b", 500, "upstream_error", None)],
           "next_cursor": "p2"},
    "p2": {"as_of": "2026-09-25T08:34:18Z", "totals": EXPORT_TOTALS,
           # A provider this build has never heard of must not cost the page.
           "requests": [export_record("c", provider="bedrock")], "next_cursor": None},
}


class ConsoleHandler(BaseHTTPRequestHandler):
    requests = []
    console_origin = ""
    # False stands in for every console deployed before windowed totals, which
    # is all of them until /v1/cli/usage ships.
    serves_windows = True
    # Everything but the cursor that the first page of the current walk was
    # asked with. A later page must repeat all of it.
    export_query = None

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

    def refuse(self, message):
        # The contract's ApiError, the one error shape the control plane sends.
        self.reply(400, {"code": "invalid_request", "message": message})

    def reply_requests_page(self):
        query = {k: v[0] for k, v in urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query).items()}
        missing = [k for k in ("since", "until", "limit") if k not in query]
        if missing:
            self.refuse(f"missing {missing}")
            return
        if query.get("model") == REFUSED_MODEL:
            self.refuse(REFUSAL)
            return
        cursor = query.pop("cursor", None)
        # The real console refuses a cursor sent with a different window or
        # filter set rather than reinterpret it; so does this one, which is what
        # makes the CLI's paging observable.
        if cursor is not None and query != ConsoleHandler.export_query:
            self.refuse(REFUSAL)
            return
        if cursor is None:
            ConsoleHandler.export_query = query
        page = EXPORT_PAGES.get(cursor)
        if page is None:
            self.refuse("unknown cursor")
            return
        self.reply(200, page)

    def do_POST(self):
        body = self.read_json()
        self.requests.append(("POST", self.path, self.headers.get("Authorization"), body))
        if self.path == "/auth/cli/start":
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
        elif self.path == "/auth/cli/poll":
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
        elif self.path == "/auth/cli/revoke":
            self.reply(204)
        else:
            self.reply(404, {"error": "unknown path"})

    def do_GET(self):
        self.requests.append(("GET", self.path, self.headers.get("Authorization"), None))
        if self.path == "/v1/me":
            self.reply(200, {"email": EMAIL})
        elif self.path == "/v1/models":
            # Login primes the model cache from here; a minimal catalog is enough.
            self.reply(200, {"data": [{"id": "glm-5.3-flash"}]})
        elif self.path.startswith("/v1/cli/usage/requests"):
            self.reply_requests_page()
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


def run_failing(binary, arguments, environment, fragment):
    """The command must exit non-zero and say why, without touching the network."""
    result = subprocess.run(
        [binary, *arguments], env=environment, capture_output=True, text=True,
        timeout=15, check=False,
    )
    combined = result.stdout + result.stderr
    if result.returncode == 0:
        raise AssertionError(f"{' '.join(arguments)} should have failed:\n{combined}")
    if fragment not in combined:
        raise AssertionError(f"{' '.join(arguments)} did not say {fragment!r}:\n{combined}")
    if ACCESS_TOKEN in combined or REFRESH_TOKEN in combined:
        raise AssertionError(f"{' '.join(arguments)} exposed a cloud token")


def json_document(combined):
    line = next((l for l in combined.splitlines() if l.startswith("{")), "")
    if not line:
        raise AssertionError(f"no JSON document:\n{combined}")
    return json.loads(line)


def check_requests_export(binary, environment):
    # First page: two of three rows, the errored one explained, and a hint that
    # there is more. Nothing here may claim to have read the whole window.
    first = run(binary, ["account", "usage", "--requests"], environment)
    for fragment in ("2 of 3 settled", "resp-a", "resp-b", "error: upstream_error (stop)",
                     "more rows exist", "window ", "09-25 08:00:00 "):
        if fragment not in first:
            raise AssertionError(f"--requests did not report {fragment!r}:\n{first}")
    if "resp-c" in first:
        raise AssertionError(f"--requests read past its first page:\n{first}")
    if "$0.0045" not in first:
        raise AssertionError(f"the window spend is missing:\n{first}")
    # An absent latency is a dash, never an instant answer.
    row_b = next(l for l in first.splitlines() if l.rstrip().endswith("resp-b"))
    if "310ms" in row_b or " - " not in row_b:
        raise AssertionError(f"a missing latency was not a dash: {row_b!r}")

    # --follow: every page, one summary, one table, no leftover hint.
    followed = run(binary, ["account", "usage", "--requests", "--follow"], environment)
    for fragment in ("3 of 3 settled", "resp-a", "resp-b", "resp-c"):
        if fragment not in followed:
            raise AssertionError(f"--follow did not report {fragment!r}:\n{followed}")
    if "more rows exist" in followed:
        raise AssertionError(f"--follow left a next-page hint:\n{followed}")
    if followed.count("window ") != 1 or followed.count("started") != 1:
        raise AssertionError(f"--follow repeated its summary or header per page:\n{followed}")

    # JSON is one document. Without --follow it is the first page and says so;
    # with --follow it is every row and says there is nothing left.
    page = json_document(run(binary, ["--json", "account", "usage", "--requests"], environment))
    if (len(page["rows"]), page["has_more"], "next_cursor" in page) != (2, True, False):
        raise AssertionError(f"unexpected first JSON page: {page}")
    # An absent value is JSON null, never a sentinel such as -1 or "".
    if page["rows"][1]["ttft_ms"] is not None or page["rows"][0]["ttft_ms"] != 310:
        raise AssertionError(f"ttft did not round-trip as 310 / null: {page['rows']}")
    first_row = page["rows"][0]
    expected_row = {
        "ts_end": None, "recorded_at": "2026-09-25T08:00:02+00:00",
        "noncached_prompt_tokens": 200, "max_tokens_requested": None,
        "max_tokens_granted": None, "provider": "self_hosted_sglang", "tpot_ms": None,
        "error_code": None, "finish_reason": "stop",
    }
    for key, value in expected_row.items():
        if key not in first_row or first_row[key] != value:
            raise AssertionError(f"row {key} is {first_row.get(key, '<missing>')!r}, not {value!r}")
    whole = json_document(run(binary, ["--json", "account", "usage", "--requests", "--follow"], environment))
    if [r["request_id"] for r in whole["rows"]] != ["a", "b", "c"] or whole["has_more"]:
        raise AssertionError(f"--json --follow did not return every row: {whole}")
    if whole["row_count"] != 3 or whole["total_requests"] != 3 or "requests" in whole:
        raise AssertionError(f"--json --follow miscounted: {whole}")
    if whole["rows"][2]["provider"] != "bedrock":
        raise AssertionError(f"an unknown provider was not carried: {whole['rows'][2]}")

    # A filtered walk repeats every filter on every page; the fake console
    # refuses page two otherwise.
    filtered = run(binary, ["account", "usage", "--requests", "--follow", "--model", "glm-5.3-flash",
                            "--status", "200", "--response-request-id", "resp-a"], environment)
    if "3 of 3 settled" not in filtered:
        raise AssertionError(f"a filtered --follow did not reach the last page:\n{filtered}")

    # The console's own refusal is what the person reads, not a bare status.
    run_failing(binary, ["account", "usage", "--requests", "--model", REFUSED_MODEL], environment,
                f"Wally Cloud refused the usage export: {REFUSAL}")

    # Refused before anything is sent.
    tail = ["--requests"]
    run_failing(binary, ["account", "usage", "--follow"], environment, "only applies with --requests")
    run_failing(binary, ["account", "usage", "--days", "3"], environment, "only applies with --requests")
    run_failing(binary, ["account", "usage", *tail, "--follow", "--limit", "5"], environment, "--limit sets the size")
    # The parser refuses an out-of-range number by naming the flag, the value
    # and the range.
    for flag, value, bounds in (("--days", "0", "1 - 31"), ("--days", "32", "1 - 31"),
                                ("--status", "99", "100 - 599"), ("--limit", "0", "1 - 200"),
                                ("--limit", "201", "1 - 200")):
        run_failing(binary, ["account", "usage", *tail, flag, value], environment,
                    f"{flag}: Value {value} not in range [{bounds}]")
    # A filter the contract refuses is refused here, never dropped from the query.
    run_failing(binary, ["account", "usage", *tail, "--model", ""], environment,
                "the model filter must be a model id")
    run_failing(binary, ["account", "usage", *tail, "--model", "glm 5.3"], environment,
                "the model filter must be a model id")
    run_failing(binary, ["account", "usage", *tail, "--response-request-id", ""], environment,
                "the response request id filter must be 1-128 characters")
    run_failing(binary, ["account", "usage", *tail, "--response-request-id", "r" * 129], environment,
                "the response request id filter must be 1-128 characters")
    for gone in ("--since", "--until", "--cursor"):
        run_failing(binary, ["account", "usage", *tail, gone, "x"], environment, "not expected")


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

            login = run(binary, ["account", "login", "--no-browser"], environment)
            if "ABCD-EFGH" not in login or ConsoleHandler.console_origin not in login:
                raise AssertionError("login did not print the approval code and URL")

            files = list(pathlib.Path(profile).iterdir())
            # login also primes models.json (a non-secret cache); the credential
            # is the secret one whose mode must be 0600.
            credentials = [f for f in files if f.name in ("credentials.json", "credentials.dat")]
            if len(credentials) != 1:
                raise AssertionError("login did not create exactly one session file")
            if os.name != "nt":
                directory_mode = stat.S_IMODE(os.stat(profile).st_mode)
                file_mode = stat.S_IMODE(os.stat(credentials[0]).st_mode)
                if directory_mode != 0o700 or file_mode != 0o600:
                    raise AssertionError(
                        f"unsafe credential modes: {directory_mode:o}/{file_mode:o}"
                    )

            whoami = run(binary, ["account", "whoami"], environment)
            if EMAIL not in whoami or "session" not in whoami or "active" not in whoami:
                raise AssertionError("whoami did not report the active identity")
            if "plan" in whoami or "tokens" in whoami or "quota" in whoami:
                raise AssertionError("whoami exposed launch-out-of-scope billing fields")

            usage = run(binary, ["account", "usage"], environment)
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
            # parser accepts `wally --json account usage`, and reading only the
            # local flag printed a human table to something asking for one
            # document.
            for argv in (["account", "usage", "--json"], ["--json", "account", "usage"]):
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
                stale = run(binary, ["account", "usage"], environment)
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

            check_requests_export(binary, environment)

            run(binary, ["account", "logout"], environment)
            if list(pathlib.Path(profile).iterdir()):
                raise AssertionError("logout did not remove the local session")

        expected = [
            ("POST", "/auth/cli/start", None),
            ("POST", "/auth/cli/poll", None),
            ("GET", "/v1/models", f"Bearer {ACCESS_TOKEN}"),
            ("GET", "/v1/me", f"Bearer {ACCESS_TOKEN}"),
            # One read per invocation. The windows are totalled server-side, so
            # `days` and `limit` are held at the minimum the route accepts —
            # nothing below the balance renders `totals`, `timeline` or `recent`.
            ("GET", "/v1/cli/usage?days=1&limit=1", f"Bearer {ACCESS_TOKEN}"),
            ("GET", "/v1/cli/usage?days=1&limit=1", f"Bearer {ACCESS_TOKEN}"),
            ("GET", "/v1/cli/usage?days=1&limit=1", f"Bearer {ACCESS_TOKEN}"),
            ("GET", "/v1/cli/usage?days=1&limit=1", f"Bearer {ACCESS_TOKEN}"),
            # The per-request export. The window is "now" and moves between runs,
            # so it is compared as a shape. Nothing the CLI refuses up front
            # (a bad flag, a window past 31 days) may appear here at all.
            ("GET", EXPORT_FIRST, f"Bearer {ACCESS_TOKEN}"),  # --requests
            ("GET", EXPORT_FOLLOW_FIRST, f"Bearer {ACCESS_TOKEN}"),  # --follow, page 1
            ("GET", EXPORT_FOLLOW_SECOND, f"Bearer {ACCESS_TOKEN}"),  # --follow, page 2
            ("GET", EXPORT_FIRST, f"Bearer {ACCESS_TOKEN}"),  # --json
            ("GET", EXPORT_FOLLOW_FIRST, f"Bearer {ACCESS_TOKEN}"),  # --json --follow, page 1
            ("GET", EXPORT_FOLLOW_SECOND, f"Bearer {ACCESS_TOKEN}"),  # --json --follow, page 2
            ("GET", EXPORT_FILTERED_FIRST, f"Bearer {ACCESS_TOKEN}"),  # filtered --follow, page 1
            ("GET", EXPORT_FILTERED_SECOND, f"Bearer {ACCESS_TOKEN}"),  # filtered --follow, page 2
            ("GET", EXPORT_FIRST + "&model=" + REFUSED_MODEL, f"Bearer {ACCESS_TOKEN}"),  # refused
            ("POST", "/auth/cli/revoke", f"Bearer {ACCESS_TOKEN}"),
        ]
        window = re.compile(r"since=[^&]+&until=[^&]+")
        actual = [
            (method, window.sub("since=*&until=*", path), authorization)
            for method, path, authorization, _ in ConsoleHandler.requests
        ]
        if actual != expected:
            raise AssertionError(f"unexpected console request sequence: {actual!r}")
        # Looked up by path, not by index: a new call anywhere in the flow
        # renumbers the list and would otherwise silently assert the wrong body.
        bodies = {path: body for _, path, _, body in ConsoleHandler.requests}
        if bodies["/auth/cli/poll"] != {
            "request_code": "ABCD-EFGH",
            "poll_secret": "poll-secret",
        }:
            raise AssertionError("poll request did not use the server-issued secret")
        if bodies["/auth/cli/revoke"] != {"refresh_token": REFRESH_TOKEN}:
            raise AssertionError("logout did not request refresh-token revocation")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    print("account CLI browser flow passed")


if __name__ == "__main__":
    main()
