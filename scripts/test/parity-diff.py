#!/usr/bin/env python3
"""Differential parity check between two wally binaries.

    parity-diff.py --baseline <wally A> --candidate <wally B> [--only NAME]

Runs stateful scenarios — sign-in against the fake console from
tests/test_account_cli.py, default-model preferences, JSON output, local
read-only commands — against each binary in its own fresh, identically shaped
home, then compares every step's exit code, stdout and stderr, and every file
left under the profile directory (content and permission bits). Timestamps
near "now" are normalised to <TS>, the temporary home to $HOME, and the fake
console's port to <PORT>, so two correct binaries produce identical transcripts.

This complements tests/golden (parse/help surface, captured once): it compares
successful behaviour and persisted state, live, against a reference binary.
Exit 0 when every scenario matches; 1 with a diff otherwise.
"""
import argparse
import difflib
import importlib.util
import json
import os
import pathlib
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[2]

spec = importlib.util.spec_from_file_location("account_cli", ROOT / "tests" / "test_account_cli.py")
account_cli = importlib.util.module_from_spec(spec)
spec.loader.exec_module(account_cli)
ConsoleHandler = account_cli.ConsoleHandler

# Each scenario: a list of steps; a step is (argv, extra_env). Steps share one
# home. The step SNAPSHOT records every file under the home at that point.
SNAPSHOT = (["<snapshot>"], {})
SCENARIOS = {
    "account_flow": [
        (["account", "whoami"], {}),
        (["account", "usage"], {}),
        (["account", "login", "--no-browser"], {}),
        SNAPSHOT,
        (["account", "whoami"], {}),
        (["account", "whoami", "--json"], {}),
        (["--json", "account", "whoami"], {}),
        (["account", "usage"], {}),
        (["account", "usage", "--json"], {}),
        (["--json", "account", "usage"], {}),
        (["account", "logout"], {}),
        (["account", "whoami"], {}),
        (["account", "logout"], {}),
    ],
    "default_model": [
        (["models", "default"], {}),
        (["models", "default", "--json"], {}),
        (["models", "default", "glm-5.3-flash"], {}),
        SNAPSHOT,
        (["models", "default"], {}),
        (["models", "default", "--json"], {}),
        (["models", "default"], {"WALLY_DEFAULT_MODEL": "qwen3-0.6b"}),
        (["models", "default", "bad<id>"], {}),
        (["models", "default", "--clear"], {}),
        (["models", "default", "--clear"], {}),
        (["models", "default"], {}),
    ],
    "local_state": [
        (["models", "list"], {}),
        (["models", "list", "--json"], {}),
        (["models", "list", "--all"], {}),
        (["models", "list", "--all", "--json"], {}),
        (["models", "show", "qwen3-0.6b"], {}),
        (["models", "show", "qwen3-0.6b", "--json"], {}),
        (["models", "rm", "qwen3-0.6b"], {}),
        (["backends"], {}),
        (["backends", "--json"], {}),
        (["version"], {}),
        (["version", "--json"], {}),
        (["about"], {}),
        (["about", "--json"], {}),
        (["info"], {}),
        (["info", "--json"], {}),
    ],
}


def start_console():
    server = ThreadingHTTPServer(("127.0.0.1", 0), ConsoleHandler)
    ConsoleHandler.console_origin = f"http://127.0.0.1:{server.server_port}"
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def base_env(home, console):
    return {
        "PATH": "/usr/bin:/bin", "HOME": home, "USERPROFILE": home,
        "RUNANYWHERE_HOME": f"{home}/ra", "WALLY_PROFILE_DIR": f"{home}/profile",
        "XDG_STATE_HOME": f"{home}/state", "XDG_CONFIG_HOME": f"{home}/config",
        "XDG_DATA_HOME": f"{home}/data", "RUNANYWHERE_BASE_URL": "http://127.0.0.1:9",
        "WALLY_CONSOLE_URL": console, "TERM": "dumb", "LANG": "C", "LC_ALL": "C",
    }


def normaliser(home, console):
    real = os.path.realpath(home)
    now = int(time.time())

    def norm(text):
        text = text.replace(real, "$HOME").replace(home, "$HOME").replace(console, "http://127.0.0.1:<PORT>")

        def ts(match):
            value = int(match.group(0))
            window = 3 * 86400
            if now - window <= value <= now + window or now * 1000 - window * 1000 <= value <= now * 1000 + window * 1000:
                return "<TS>"
            return match.group(0)

        return re.sub(r"\b\d{10,13}\b", ts, text)

    return norm


def snapshot(home, norm):
    files = {}
    profile = pathlib.Path(home) / "profile"
    for path in sorted(pathlib.Path(home).rglob("*")):
        rel = str(path.relative_to(home))
        mode = stat.S_IMODE(path.lstat().st_mode)
        if path.is_dir():
            files[rel + "/"] = f"dir {mode:o}"
        elif path.is_file():
            if profile in path.parents:
                files[rel] = f"file {mode:o}\n" + norm(path.read_text(errors="replace"))
            else:
                # SDK-created state (device ids, caches) differs run to run; only
                # its presence and mode are compared.
                files[rel] = f"file {mode:o} (content not compared)"
    return files


def run_scenario(binary, steps, console):
    home = tempfile.mkdtemp(prefix="wally-parity-")
    try:
        norm = normaliser(home, console)
        transcript = []
        for argv, extra in steps:
            if (argv, extra) == SNAPSHOT:
                transcript.append("=== files at this point\n" + "\n".join(f"{k}: {v}" for k, v in snapshot(home, norm).items()))
                continue
            env = base_env(home, console)
            env.update(extra)
            try:
                p = subprocess.run([binary, *argv], env=env, capture_output=True,
                                   stdin=subprocess.DEVNULL, timeout=60)
                code, out, err = p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")
            except subprocess.TimeoutExpired:
                code, out, err = "timeout", "", ""
            transcript.append(f"$ wally {' '.join(argv)}  {extra or ''}\n[exit {code}]\n--- stdout\n{norm(out)}--- stderr\n{norm(err)}")
        state = snapshot(home, norm)
        return transcript, state
    finally:
        shutil.rmtree(home, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--baseline", required=True)
    ap.add_argument("--candidate", required=True)
    ap.add_argument("--only")
    ap.add_argument("--keep", help="write both transcripts under this directory")
    a = ap.parse_args()
    server = start_console()
    console = ConsoleHandler.console_origin
    failed = 0
    for name, steps in SCENARIOS.items():
        if a.only and a.only != name:
            continue
        results = {}
        for label, binary in (("baseline", a.baseline), ("candidate", a.candidate)):
            ConsoleHandler.requests.clear()
            transcript, state = run_scenario(binary, steps, console)
            requests = [f"{m} {p} auth={'yes' if auth else 'no'} body={json.dumps(b, sort_keys=True)}"
                        for m, p, auth, b in ConsoleHandler.requests]
            results[label] = ("\n".join(transcript)
                              + "\n=== files\n" + "\n".join(f"{k}: {v}" for k, v in state.items())
                              + "\n=== console requests\n" + "\n".join(requests) + "\n")
        if a.keep:
            os.makedirs(a.keep, exist_ok=True)
            for label, text in results.items():
                pathlib.Path(a.keep, f"{name}.{label}.txt").write_text(text)
        if results["baseline"] == results["candidate"]:
            print(f"ok    {name} ({len(steps)} steps)")
        else:
            failed += 1
            print(f"DIFF  {name}")
            sys.stdout.writelines(difflib.unified_diff(
                results["baseline"].splitlines(True), results["candidate"].splitlines(True),
                "baseline", "candidate", n=2))
    server.shutdown()
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
