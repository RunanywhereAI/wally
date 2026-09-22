#!/usr/bin/env python3
"""Hermetic production CLI -> installed-tool fixture -> packaged local server.

The executable links the real CLI/server and substitutes a deterministic SDK
LLM plugin. It cannot establish real-model inference quality; device smoke
covers that independently.
"""
import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Console(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.path == "/v1/me":
            body = {"email": "harness@example.test"}
        elif self.path == "/v1/models":
            body = {"data": [{"id": "qwen3-0.6b", "context_window": 32768,
                              "max_output_tokens": 4096}]}
        elif self.path == "/v1/models/catalog":
            body = {"models": []}
        else:
            body = {}
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(body).encode())

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b"{}")


def main():
    binary, child = map(pathlib.Path, sys.argv[1:3])
    console = ThreadingHTTPServer(("127.0.0.1", 0), Console)
    threading.Thread(target=console.serve_forever, daemon=True).start()
    origin = f"http://127.0.0.1:{console.server_port}"
    try:
        with tempfile.TemporaryDirectory(prefix="wally-local-harness-") as tmp:
            root = pathlib.Path(tmp)
            home, models_home, bins, profile = [root / name for name in ("home", "models home", "bin", "profile")]
            for directory in (home, models_home, bins, profile):
                directory.mkdir()
            for name in ("opencode", "dsh", "hermes", "openclaw", "claude"):
                target = bins / (name + (".exe" if os.name == "nt" else ""))
                shutil.copy2(child, target)
            weights_dir = models_home / "Models" / "LlamaCpp" / "qwen3-0.6b"
            weights_dir.mkdir(parents=True)
            weights = weights_dir / "Qwen3-0.6B-Q8_0.gguf"
            weights.write_bytes(b"GGUF fixture; the registered test plugin owns loading")
            env = {k: v for k, v in os.environ.items()
                   if not k.startswith(("RUNANYWHERE_", "WALLY_", "ANTHROPIC_", "CLAUDE_", "HERMES_", "OPENCLAW_", "OPENCODE_"))}
            env.update({"HOME": str(home), "USERPROFILE": str(home), "WALLY_PROFILE_DIR": str(profile),
                        "RUNANYWHERE_HOME": str(home / "empty-models"),
                        "PATH": str(bins) + os.pathsep + env.get("PATH", ""),
                        "RUNANYWHERE_BASE_URL": origin, "WALLY_CONSOLE_URL": origin,
                        "NO_COLOR": "1", "OPENCODE_CONFIG_CONTENT": '{"theme":"existing-user-theme"}',
                        "WALLY_TEST_CHILD_REPORT": str(root / "child.json"),
                        "WALLY_TEST_BACKEND_REPORT": str(root / "backend.json")})

            def invoke(args, expected=0, extra=None):
                for report in (root / "child.json", root / "backend.json"):
                    report.unlink(missing_ok=True)
                command_env = dict(env, **(extra or {}))
                result = subprocess.run([str(binary), "--home", str(models_home), *args],
                                        env=command_env, text=True, capture_output=True, timeout=20)
                assert result.returncode == expected, (args, result.returncode, result.stdout, result.stderr)
                backend = json.loads((root / "backend.json").read_text())
                assert backend["stopped"] and backend["environment_restored"], backend
                child_report = json.loads((root / "child.json").read_text()) if (root / "child.json").exists() else None
                return result, backend, child_report

            for tool, model in (("opencode", "qwen3"), ("opencode", "qwen3-0.6b"),
                                ("deepseek", "qwen3"), ("openclaw", "qwen3"),
                                ("hermes", "qwen3"), ("claude-code", "qwen3")):
                extra = {"WALLY_TEST_CHILD_EXIT": "37"} if tool == "deepseek" else None
                expected = 37 if tool == "deepseek" else 0
                _, backend, captured = invoke([tool, "-m", model, "--", "prompt with spaces"], expected, extra)
                assert captured["model"] == model, captured
                assert "prompt with spaces" in captured["args"], captured
                assert pathlib.Path(backend["model_path"]) == weights, backend
                assert backend["initialized"] == 1 and backend["created"] == backend["destroyed"] == 1, backend
                assert backend["generated"] == (1 if tool == "claude-code" else 3), backend
                if "limits" in captured:
                    limits = captured["limits"]
                    assert 0 < limits["output"] < limits["context"], captured
                    # The server must give the engine the context advertised to
                    # the child. This catches kits that silently discard it.
                    assert backend.get("config", {}).get("context_length") == limits["context"], backend
                for path in captured["temporary_files"]:
                    assert not pathlib.Path(path).exists(), captured
                address = urllib.parse.urlsplit(captured["url"])
                with socket.socket() as connection:
                    connection.settimeout(1)
                    assert connection.connect_ex((address.hostname, address.port)) != 0, captured
                print(f"PASS local {tool} ({model}): HTTP, launch arguments, teardown")

            result, backend, captured = invoke(["opencode", "-m", "qwen3"], 1,
                                               {"WALLY_TEST_FAIL_LOAD": "1"})
            assert captured is None and backend["destroyed"] == backend["created"] == 1, backend
            assert "would not start" in result.stderr and "models pull" in result.stderr, result.stderr
            print("PASS backend failure prevents child and releases model")

            weights.unlink()
            (weights_dir / ".rac-manifest.binpb").write_bytes(b"incomplete manifest fixture")
            result, backend, captured = invoke(["opencode", "-m", "qwen3"], 1)
            assert "incomplete" in result.stderr and "models pull" in result.stderr, result.stderr
            assert backend["created"] == 0 and captured is None
            shutil.rmtree(weights_dir)
            result, backend, captured = invoke(["opencode", "-m", "qwen3"], 1)
            assert "not downloaded" in result.stderr and "models pull" in result.stderr, result.stderr
            assert backend["created"] == 0 and captured is None
            print("PASS incomplete and missing models have pull guidance")

            # Empty local cache is not evidence that a signed-in cloud model is
            # unavailable, even when its id also appears in the local catalog.
            (profile / "credentials.json").write_text(json.dumps({"console_url": origin,
                "email": "harness@example.test", "access_token": "test-cloud-token",
                "refresh_token": "", "expires_at": int(time.time()) + 3600}))
            _, backend, captured = invoke(["opencode", "-m", "qwen3-0.6b"],
                                          extra={"WALLY_TEST_PASSTHROUGH": "1"})
            assert backend["created"] == 0
            provider = json.loads(captured["inherited_config"])["provider"]["runanywhere"]
            assert provider["options"] == {"baseURL": origin + "/v1", "apiKey": "test-cloud-token"}
            print("PASS uncached hosted model preserves cloud launch")
    finally:
        console.shutdown()
        console.server_close()


if __name__ == "__main__":
    main()
