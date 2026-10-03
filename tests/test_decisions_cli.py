"""Hermetic end-to-end coverage for `wally decisions`.

The session comes from a real `wally account login` against the fake console
rather than a credentials file written here: Windows keeps the credential in a
DPAPI-sealed credentials.dat that only the CLI can write, so a seeded
credentials.json is never read there."""

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ACCESS = "access-token"
REFRESH = "refresh-token"
NEW_ACCESS = "new-access-token"


class ConsoleHandler(BaseHTTPRequestHandler):
    requests = []
    attempts = {}

    def log_message(self, _format, *_args):
        return

    def read_json(self):
        size = int(self.headers.get("Content-Length", "0"))
        return json.loads(self.rfile.read(size).decode() or "{}")

    def reply(self, status, body, extra_headers=()):
        encoded = json.dumps(body, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        for name, value in extra_headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(encoded)

    def do_GET(self):
        # Not recorded: the assertions below read the last POST, and a
        # finished decisions call is followed by a catalog GET for its price.
        if self.path == "/v1/models":
            # Login primes the hosted-model cache from here.
            self.reply(200, {"data": [{"id": "glm-5.3-flash"}]})
            return
        self.reply(404, {"error": {"message": "unknown", "type": "invalid_request_error"}})

    def do_POST(self):
        body = self.read_json()
        authorization = self.headers.get("Authorization")
        self.requests.append((self.path, authorization, body))
        if self.path == "/auth/cli/start":
            origin = f"http://127.0.0.1:{self.server.server_port}"
            self.reply(200, {
                "request_code": "ABCD-EFGH", "poll_secret": "poll-secret",
                "verification_url": origin + "/device?code=ABCD-EFGH",
                "expires_in": 60, "interval": 1,
            })
            return
        if self.path == "/auth/cli/poll":
            self.reply(200, {
                "status": "approved", "access_token": ACCESS, "refresh_token": REFRESH,
                "email": "decisions@example.test", "plan": "beta", "expires_in": 3600,
            })
            return
        if self.path == "/auth/cli/refresh":
            self.reply(200, {
                "access_token": NEW_ACCESS, "refresh_token": REFRESH,
                "email": "decisions@example.test", "plan": "beta", "expires_in": 3600,
            })
            return
        if self.path != "/v1/decisions":
            self.reply(404, {"error": {"message": "unknown", "type": "invalid_request_error"}})
            return
        marker = body["input"]
        self.attempts[marker] = self.attempts.get(marker, 0) + 1
        if marker in {"retry", "no-retry"} and self.attempts[marker] == 1:
            self.reply(
                429,
                {"error": {"code": "capacity_exceeded", "message": "busy",
                           "type": "capacity"}},
                (("Retry-After", "0"),),
            )
            return
        if marker == "refresh" and authorization == f"Bearer {ACCESS}":
            self.reply(401, {"error": {"code": "expired_api_key", "message": "expired",
                                      "type": "authentication_error"}})
            return
        if marker == "new-code":
            self.reply(400, {"error": {"code": "a_code_from_a_newer_contract",
                                      "message": "this model does not serve prompt format 2",
                                      "type": "a_type_from_a_newer_contract"}})
            return
        if marker == "too-long":
            self.reply(400, {"error": {"code": "bad_request",
                                      "message": "question q1 is 9001 tokens; the model takes 8192",
                                      "type": "invalid_request_error"}})
            return
        if marker == "forbidden":
            self.reply(403, {"error": {"code": "model_not_entitled",
                                      "message": "not entitled",
                                      "type": "permission_error"}})
            return
        answers = {}
        for question in body["questions"]:
            labels = (
                [option["name"] for option in question["options"]]
                if question["type"] == "choice"
                else [str(i) for i, _ in enumerate(question["levels"])]
                if question["type"] == "score"
                else ["yes", "no"]
            )
            probabilities = {label: (0.8 if index == 0 else 0.2 / (len(labels) - 1))
                             for index, label in enumerate(labels)}
            answers[question["id"]] = {
                "type": question["type"], "probabilities": probabilities, "label_mass": 0.9,
            }
        self.reply(200, {
            "object": "decisions", "model": body["model"], "prompt_format_version": 3,
            "answers": answers,
            "usage": {"prompt_tokens": 12, "completion_tokens": 0, "total_tokens": 12},
        })


def invoke(binary, env, *args, stdin=None):
    return subprocess.run(
        [binary, *args], input=stdin, env=env, capture_output=True,
        text=True, timeout=15, check=False,
    )


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: test_decisions_cli.py /path/to/wally")
    binary = sys.argv[1]
    server = ThreadingHTTPServer(("127.0.0.1", 0), ConsoleHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="wally-decisions-") as profile:
            origin = f"http://127.0.0.1:{server.server_port}"
            env = dict(os.environ, WALLY_PROFILE_DIR=profile, WALLY_CONSOLE_URL=origin)

            signed_out = invoke(binary, env, "decisions", "--input", "x", "--ask", "Works?")
            assert signed_out.returncode == 1, signed_out.stderr
            assert "Sign in first: wally account login" in signed_out.stderr

            # Chat surfaces refuse a decision model before anything else, so
            # these hold with no coding tool installed and no session.
            for command in (["opencode"], ["opencode", "--cloud"], ["claude-code"], ["hermes"]):
                refused = invoke(binary, env, *command, "-m", "pplx-decider-v1")
                assert refused.returncode == 2, (command, refused.stderr)
                assert "wally decisions -m pplx-decider-v1" in refused.stderr, refused.stderr

            login = invoke(binary, env, "account", "login", "--no-browser")
            assert login.returncode == 0, login.stderr

            human = invoke(
                binary, env, "decisions", "--input", "ticket", "--ask", "Is this a bug?",
                "--choice", "Owner=frontend,payments", "--temperature", "0.7",
            )
            assert human.returncode == 0, human.stderr
            assert "Is this a bug?" in human.stdout and "Owner" in human.stdout
            sent = ConsoleHandler.requests[-1][2]
            assert sent["model"] == "pplx-decider-v1" and sent["temperature"] == 0.7
            assert [question["id"] for question in sent["questions"]] == ["q1", "q2"]

            raw = invoke(binary, env, "--json", "decide", "--input", "json", "--ask", "Okay?")
            assert raw.returncode == 0, raw.stderr
            document = json.loads(raw.stdout)
            assert document["object"] == "decisions" and raw.stdout.count("\n") == 1

            retried = invoke(binary, env, "decisions", "--input", "retry", "--ask", "Okay?")
            assert retried.returncode == 0 and "HTTP 429; retrying" in retried.stderr
            assert ConsoleHandler.attempts["retry"] == 2

            no_retry = invoke(
                binary, env, "decisions", "--input", "no-retry", "--ask", "Okay?",
                "--no-retry",
            )
            assert no_retry.returncode == 1 and "retrying" not in no_retry.stderr
            assert ConsoleHandler.attempts["no-retry"] == 1

            refreshed = invoke(binary, env, "decisions", "--input", "refresh", "--ask", "Okay?")
            assert refreshed.returncode == 0, refreshed.stderr
            paths = [request[0] for request in ConsoleHandler.requests]
            assert paths[-3:] == ["/v1/decisions", "/auth/cli/refresh", "/v1/decisions"]
            assert ConsoleHandler.requests[-1][1] == f"Bearer {NEW_ACCESS}"

            forbidden = invoke(binary, env, "decisions", "--input", "forbidden", "--ask", "Okay?")
            assert forbidden.returncode == 1
            assert "not entitled to pplx-decider-v1" in forbidden.stderr, forbidden.stderr
            assert "wally account login" in forbidden.stderr, forbidden.stderr

            too_long = invoke(binary, env, "decisions", "--input", "too-long", "--ask", "Okay?")
            assert too_long.returncode == 1
            assert "question q1 is 9001 tokens" in too_long.stderr, too_long.stderr

            new_code = invoke(binary, env, "decisions", "--input", "new-code", "--ask", "Okay?")
            assert new_code.returncode == 1
            assert "does not serve prompt format 2" in new_code.stderr, new_code.stderr

            scored = invoke(
                binary, env, "decisions", "--input", "score",
                "--score", "Severity?=none,minor,major,outage",
            )
            assert scored.returncode == 0, scored.stderr
            rows = [line.split()[0] for line in scored.stdout.splitlines() if line.startswith("  ")]
            assert rows == ["none", "minor", "major", "outage"], scored.stdout

            before = len(ConsoleHandler.requests)
            typo_file = pathlib.Path(profile, "typo.json")
            typo_file.write_text(json.dumps({
                "model": "pplx-decider-v1", "input": "typo", "temprature": 0.2,
                "questions": [{"id": "q1", "type": "yes_no", "question": "Okay?"}],
            }))
            typo = invoke(binary, env, "decisions", "--request", str(typo_file))
            assert typo.returncode == 2 and "temprature" in typo.stderr, typo.stderr
            assert len(ConsoleHandler.requests) == before, "a refused request file was sent"

            custom = {
                "model": "pplx-decider-v1", "input": "custom",
                "questions": [{"id": "custom-id", "type": "choice", "question": "Route?",
                               "options": [{"name": "a", "description": "Alpha"},
                                           {"name": "b", "description": "Beta"}]}],
            }
            request_file = pathlib.Path(profile, "request.json")
            request_file.write_text(json.dumps(custom))
            result = invoke(binary, env, "decisions", "--request", str(request_file))
            assert result.returncode == 0 and "Route?" in result.stdout
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    print("decisions CLI passed")


if __name__ == "__main__":
    main()
