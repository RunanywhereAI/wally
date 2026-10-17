"""Hermetic end-to-end coverage for `wally decisions`.

The session comes from a real `wally account login` against the fake console
rather than a credentials file written here: Windows keeps the credential in a
DPAPI-sealed credentials.dat that only the CLI can write, so a seeded
credentials.json is never read there."""

import base64
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


FIXTURES = pathlib.Path(__file__).resolve().parent / "fixtures"
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
        self.reply_bytes(status, encoded, extra_headers)

    def reply_bytes(self, status, encoded, extra_headers=(), declared_length=None):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded) if declared_length is None else declared_length))
        if self.path == "/v1/decisions":
            marker = self.requests[-1][2]["input"]
            if marker != "missing-id":
                request_id = "x" * 257 if marker == "invalid-id" else f"decision-{marker}-{self.attempts[marker]}"
                self.send_header("X-Request-ID", request_id)
        else:
            self.send_header("X-Request-ID", "not-a-decisions-request")
        for name, value in extra_headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(encoded)
        if declared_length is not None:
            self.close_connection = True

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
        if marker == "passthrough":
            self.reply_bytes(200, b'{"object":"decisions","model":"eve","prompt_format_version":3,'
                             b'"answers":{},"usage":{"prompt_tokens":12,"total_tokens":12,'
                             b'"completion_tokens":0,"reasoning_tokens":null},'
                             b'"future_metadata":{"nullable":null,"precise":1.234567890123456789e-100},'
                             b'"future_number":184467440737095516160,"request_id":"body-id","http_status":418}')
            return
        if marker == "transport-failure":
            self.close_connection = True
            return
        if marker in {"malformed", "body-read-failure"}:
            self.reply_bytes(200, b"{", declared_length=99 if marker == "body-read-failure" else None)
            return
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
        # The gateway's refusals of images it will not take (InferenceInfra
        # #3453): this CLI no longer checks image sizes, so these must reach
        # the person as written.
        if marker == "image-too-big":
            self.reply(400, {"error": {"code": "bad_request", "param": "images",
                                      "message": "images[0] is 9000x8000, over 64,000,000 pixels: scale it below that",
                                      "type": "invalid_request_error"}})
            return
        if marker == "body-too-big":
            self.reply(400, {"error": {"code": "payload_too_large",
                                      "message": "The request body is larger than 25165824 bytes, the most a decision with images may be.",
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


def invoke(binary, env, *args, stdin=""):
    """Run a CLI case with nonterminal input rather than an interactive route."""
    return subprocess.run(
        [binary, *args], input="" if stdin is None else stdin, env=env, capture_output=True,
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

            signed_out = invoke(binary, env, "decisions", "--input", "x", "--ask", "Works?", "--json")
            assert signed_out.returncode == 1, signed_out.stderr
            assert "Sign in first: wally account login" in signed_out.stderr
            document = json.loads(signed_out.stdout)
            assert document["request_id"] is None and document["http_status"] is None

            # Chat surfaces refuse a decision model before anything else, so
            # these hold with no coding tool installed and no session.
            for command, model in (
                (["opencode"], "eve"), (["opencode", "--cloud"], "eve"),
                (["claude-code"], "eve"), (["hermes"], "eve"),
                (["opencode"], "pplx-decider-v1"), (["claude-code"], "qwev"),
            ):
                refused = invoke(binary, env, *command, "-m", model)
                assert refused.returncode == 2, (command, model, refused.stderr)
                assert f"wally decisions -m {model}" in refused.stderr, refused.stderr

            login = invoke(binary, env, "account", "login", "--no-browser")
            assert login.returncode == 0, login.stderr

            human = invoke(
                binary, env, "decisions", "--input", "ticket", "--ask", "Is this a bug?",
                "--choice", "Owner=frontend,payments", "--temperature", "0.7",
            )
            assert human.returncode == 0, human.stderr
            assert "Is this a bug?" in human.stdout and "Owner" in human.stdout
            sent = ConsoleHandler.requests[-1][2]
            assert sent["model"] == "eve" and sent["temperature"] == 0.7
            assert [question["id"] for question in sent["questions"]] == ["q1", "q2"]

            raw = invoke(binary, env, "--json", "decide", "--input", "json", "--ask", "Okay?")
            assert raw.returncode == 0, raw.stderr
            document = json.loads(raw.stdout)
            assert document["object"] == "decisions" and raw.stdout.count("\n") == 1
            assert document["http_status"] == 200, document
            assert document["request_id"] == "decision-json-1", document

            preserved = invoke(binary, env, "decisions", "--input", "passthrough",
                               "--ask", "Okay?", "--json")
            assert preserved.returncode == 0, preserved.stderr
            document = json.loads(preserved.stdout)
            assert document["future_metadata"]["nullable"] is None, document
            assert document["future_number"] == 184467440737095516160, document
            assert document["usage"]["reasoning_tokens"] is None, document
            assert "1.234567890123456789e-100" in preserved.stdout, preserved.stdout
            assert document["request_id"] == "decision-passthrough-1", document
            assert document["http_status"] == 200, document
            assert preserved.stdout.count('"request_id"') == 1
            assert preserved.stdout.count('"http_status"') == 1

            # No model or network: invalid bootstrap configuration, then a
            # missing local artifact. Both early returns must emit null fields.
            local_artifact = pathlib.Path(profile, "empty-local-model")
            local_artifact.mkdir()
            for mode, model, diagnostic in (("invalid", str(local_artifact), "invalid --environment"),
                                            ("development", "clef-flash-9b", "not downloaded")):
                local_env = dict(env, RUNANYWHERE_ENVIRONMENT=mode, RUNANYWHERE_API_KEY="")
                failed = invoke(binary, local_env, "--home", profile, "decisions",
                                "--local", "-m", model,
                                "--input", "local", "--ask", "Okay?", "--json")
                if mode == "development" and failed.returncode == 2 and "does not resolve" in failed.stderr:
                    # Public Windows ARM64 kits have no GGUF decision backend.
                    print("skip missing-model startup case: no GGUF decision catalog")
                    continue
                assert failed.returncode == 1, failed.stderr
                assert diagnostic in failed.stderr, failed.stderr
                document = json.loads(failed.stdout)
                assert document["request_id"] is None and document["http_status"] is None
                assert document["error"]["message"], document

            for marker, status in (("too-long", 400), ("forbidden", 403), ("new-code", 400)):
                refused = invoke(binary, env, "decisions", "--input", marker,
                                 "--ask", "Okay?", "--json")
                assert refused.returncode == 1, refused.stderr
                document = json.loads(refused.stdout)
                assert refused.stdout.count("\n") == 1
                assert document["http_status"] == status, document
                assert document["request_id"] == f"decision-{marker}-1", document
                assert document["error"]["message"], document
                if marker == "new-code":
                    assert "type" not in document["error"] and "code" not in document["error"]
                assert f"HTTP {status}" in refused.stderr, refused.stderr

            for marker in ("missing-id", "invalid-id"):
                result = invoke(binary, env, "decisions", "--input", marker,
                                "--ask", "Okay?", "--json")
                document = json.loads(result.stdout)
                assert result.returncode == 0 and document["request_id"] is None, document
                assert document["http_status"] == 200, document

            for marker in ("malformed", "body-read-failure", "transport-failure"):
                failed = invoke(binary, env, "decisions", "--input", marker,
                                "--ask", "Okay?", "--json", "--no-retry")
                assert failed.returncode == 1, failed.stderr
                document = json.loads(failed.stdout)
                assert failed.stdout.count("\n") == 1
                assert document["error"]["message"], document
                if marker == "transport-failure":
                    assert document["request_id"] is None and document["http_status"] is None
                else:
                    assert document["http_status"] == 200, document
                    assert document["request_id"] == f"decision-{marker}-1", document
                    if marker == "body-read-failure":
                        assert "could not reach Wally Cloud" in document["error"]["message"], document

            retried = invoke(binary, env, "decisions", "--input", "retry", "--ask", "Okay?", "--json")
            assert retried.returncode == 0 and "HTTP 429; retrying" in retried.stderr
            assert ConsoleHandler.attempts["retry"] == 2
            assert json.loads(retried.stdout)["request_id"] == "decision-retry-2"

            no_retry = invoke(
                binary, env, "decisions", "--input", "no-retry", "--ask", "Okay?",
                "--no-retry", "--json",
            )
            assert no_retry.returncode == 1 and "retrying" not in no_retry.stderr
            assert ConsoleHandler.attempts["no-retry"] == 1
            assert json.loads(no_retry.stdout)["http_status"] == 429
            assert json.loads(no_retry.stdout)["request_id"] == "decision-no-retry-1"

            refreshed = invoke(binary, env, "decisions", "--input", "refresh", "--ask", "Okay?", "--json")
            assert refreshed.returncode == 0, refreshed.stderr
            paths = [request[0] for request in ConsoleHandler.requests]
            assert paths[-3:] == ["/v1/decisions", "/auth/cli/refresh", "/v1/decisions"]
            assert ConsoleHandler.requests[-1][1] == f"Bearer {NEW_ACCESS}"
            assert json.loads(refreshed.stdout)["request_id"] == "decision-refresh-2"

            forbidden = invoke(binary, env, "decisions", "--input", "forbidden", "--ask", "Okay?")
            assert forbidden.returncode == 1
            assert "eve is not available on this account" in forbidden.stderr, forbidden.stderr
            # Entitlement is account-level; re-login cannot change it, so the
            # message must not send anyone down that path.
            assert "wally account login" not in forbidden.stderr, forbidden.stderr
            assert "--local" in forbidden.stderr, forbidden.stderr

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
                "model": "eve", "input": "typo", "temprature": 0.2,
                "questions": [{"id": "q1", "type": "yes_no", "question": "Okay?"}],
            }))
            typo = invoke(binary, env, "decisions", "--request", str(typo_file), "--json")
            assert typo.returncode == 2 and "temprature" in typo.stderr, typo.stderr
            document = json.loads(typo.stdout)
            assert document["request_id"] is None and document["http_status"] is None
            assert len(ConsoleHandler.requests) == before, "a refused request file was sent"

            custom = {
                "model": "eve", "input": "custom",
                "questions": [{"id": "custom-id", "type": "choice", "question": "Route?",
                               "options": [{"name": "a", "description": "Alpha"},
                                           {"name": "b", "description": "Beta"}]}],
            }
            request_file = pathlib.Path(profile, "request.json")
            request_file.write_text(json.dumps(custom))
            result = invoke(binary, env, "decisions", "--request", str(request_file))
            assert result.returncode == 0 and "Route?" in result.stdout

            # Images: read from the file's bytes, sent as data URLs in order,
            # and refused before anything is sent when they cannot be.
            png = pathlib.Path(profile, "receipt.png")
            png.write_bytes(b"\x89PNG\r\n\x1a\n" + b"\0" * 16)
            webp = pathlib.Path(profile, "photo.bin")
            webp.write_bytes(b"RIFF\0\0\0\0WEBPVP8 " + b"\0" * 8)
            pictured = invoke(
                binary, env, "decisions", "--input", "images", "--ask", "Okay?",
                "--image", str(png), "--image", str(webp),
            )
            assert pictured.returncode == 0, pictured.stderr
            images = ConsoleHandler.requests[-1][2]["images"]
            assert [image.split(",", 1)[0] for image in images] == [
                "data:image/png;base64", "data:image/webp;base64",
            ], images
            # A HEIC photo goes as a JPEG made from it on macOS, and is refused
            # with the reason elsewhere.
            heic = pathlib.Path(profile, "IMG_0001.HEIC")
            heic.write_bytes((FIXTURES / "quadrants-orientation-6-gps.heic").read_bytes())
            before = len(ConsoleHandler.requests)
            photographed = invoke(
                binary, env, "decisions", "--input", "photo", "--ask", "Okay?", "--image", str(heic),
            )
            if sys.platform == "darwin":
                assert photographed.returncode == 0, photographed.stderr
                images = ConsoleHandler.requests[-1][2]["images"]
                assert len(images) == 1 and images[0].startswith("data:image/jpeg;base64,/9j/"), images
            else:
                assert photographed.returncode == 2, photographed.stderr
                assert "HEIC photo" in photographed.stderr and "macOS only" in photographed.stderr
                assert len(ConsoleHandler.requests) == before, "a refused HEIC request was sent"

            # Full size: a photo goes byte for byte, and only the gateway
            # judges its size, in its own words.
            photo = pathlib.Path(profile, "photo.jpg")
            photo.write_bytes(b"\xff\xd8\xff\xe0" + b"\x07" * 3_000_000)
            sent = invoke(binary, env, "decisions", "--input", "full", "--ask", "Okay?", "--image", str(photo))
            assert sent.returncode == 0, sent.stderr
            images = ConsoleHandler.requests[-1][2]["images"]
            assert images == ["data:image/jpeg;base64," + base64.b64encode(photo.read_bytes()).decode()]
            for marker, message in (
                ("image-too-big", "over 64,000,000 pixels"),
                ("body-too-big", "larger than 25165824 bytes"),
            ):
                refused = invoke(binary, env, "decisions", "--input", marker, "--ask", "Okay?", "--image", str(png))
                assert refused.returncode == 1 and message in refused.stderr, (marker, refused.stderr)

            # A HEIC cut short is refused before anything is sent, everywhere.
            cut = pathlib.Path(profile, "IMG_0002.HEIC")
            cut.write_bytes((FIXTURES / "quadrants-grid.heic").read_bytes()[:-1])
            before = len(ConsoleHandler.requests)
            refused = invoke(binary, env, "decisions", "--input", "cut", "--ask", "Okay?", "--image", str(cut))
            assert refused.returncode == 2 and "incomplete or damaged" in refused.stderr, refused.stderr
            assert len(ConsoleHandler.requests) == before, "a truncated HEIC was sent"

            before = len(ConsoleHandler.requests)
            gif = pathlib.Path(profile, "a.gif")
            gif.write_bytes(b"GIF89a")
            for args, message in (
                (["--image", str(gif)], "not a PNG, JPEG, WebP or HEIC"),
                (["--grouping", "joint"], "only apply to a local model"),
                (["--grouping", "sideways"], "--grouping must be 'joint' or 'per-question'"),
                (["--max-pixels", "10", "--min-pixels", "20"], "must not exceed --max-pixels"),
                (["--max-length", "0"], "--max-length must be greater than 0"),
                (["--image", str(png)] * 9, "at most 8"),
            ):
                refused = invoke(binary, env, "decisions", "--input", "x", "--ask", "Okay?", *args)
                assert refused.returncode == 2 and message in refused.stderr, (args, refused.stderr)
            refused = invoke(binary, env, "decisions", "--request", str(request_file), "--image", str(png))
            assert refused.returncode == 2 and "--image" in refused.stderr, refused.stderr
            assert len(ConsoleHandler.requests) == before, "a refused image request was sent"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    print("decisions CLI passed")


if __name__ == "__main__":
    main()
