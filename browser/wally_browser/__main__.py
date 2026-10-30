"""What `wally browser-use` runs. Jev Ultrafast owns the loop. Eve decides. DeepSeek types."""

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

from jev_ultrafast import Agent

CHROME_PORT = 9334
# This profile keeps cookies and logged-in sessions between runs.
CHROME_PROFILE = Path.home() / ".cache" / "wally-browser" / "chrome"
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"


def chrome_on_our_port(url: str) -> bool:
    """The listener is Chrome, on this machine, on the port this command uses."""
    try:
        with urllib.request.urlopen(url + "/json/version", timeout=0.5) as response:
            data = json.loads(response.read().decode())
    except (OSError, ValueError, json.JSONDecodeError):
        return False
    browser = data.get("Browser") or ""
    socket_url = data.get("webSocketDebuggerUrl") or ""
    return "Chrome" in browser and socket_url.startswith(f"ws://127.0.0.1:{CHROME_PORT}")


def chrome_ready() -> None:
    """Browser Harness attaches to a Chrome with remote debugging. Start one if none is up.

    The profile at CHROME_PROFILE persists login sessions. Debugging is bound to
    127.0.0.1. An existing listener is reused only when /json/version says it is
    Chrome on that port.
    """
    if os.environ.get("BU_CDP_URL") or os.environ.get("BU_CDP_WS"):
        return
    url = f"http://127.0.0.1:{CHROME_PORT}"

    def listening() -> bool:
        try:
            socket.create_connection(("127.0.0.1", CHROME_PORT), timeout=0.3).close()
            return True
        except OSError:
            return False

    if not listening():
        binary = CHROME if Path(CHROME).exists() else shutil.which("google-chrome")
        if not binary:
            raise RuntimeError("Chrome is not installed.")
        CHROME_PROFILE.mkdir(parents=True, exist_ok=True)
        subprocess.Popen(
            [binary, f"--remote-debugging-port={CHROME_PORT}", "--remote-debugging-address=127.0.0.1",
             f"--user-data-dir={CHROME_PROFILE}", "--no-first-run", "--no-default-browser-check", "about:blank"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
        )
        deadline = time.monotonic() + 20
        while not listening():
            if time.monotonic() > deadline:
                raise RuntimeError(f"Chrome did not open port {CHROME_PORT}.")
            time.sleep(0.1)
    if not chrome_on_our_port(url):
        raise RuntimeError(f"Port {CHROME_PORT} is open, but it is not this command's Chrome.")
    os.environ["BU_CDP_URL"] = url


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="wally browser-use")
    parser.add_argument("goal")
    parser.add_argument("--start-url")
    args = parser.parse_args(argv)
    if not args.start_url:
        sys.stderr.write("pass --start-url\n")
        return 2
    decider = os.environ.get("WALLY_BROWSER_DECISION_MODEL", "eve")
    chrome_ready()
    try:
        with Agent(args.start_url, args.goal) as agent:
            last = None
            shown = 0
            for state in agent.run():
                last = state
                if len(state["history"]) == shown and state["status"] == "ready":
                    continue
                count = len(state["history"])
                done = state["history"][-1] if state["history"] else None
                line = f"{state['elapsed_ms']:>5} ms  {count} actions  {state['status']}"
                if state["status"] in {"done", "blocked"} and count == shown:
                    line += f"  stopped  {state['page']['url']}"
                elif done:
                    line += f"  {done['kind']}  {done['action']}"
                    if done.get("text"):
                        line += f"  {done['text']!r}  text {done.get('text_latency_ms', 0)} ms"
                    line += f"  {decider} {done.get('latency_ms', 0)} ms"
                shown = count
                print(line, flush=True)
    except KeyboardInterrupt:
        sys.stderr.write("\nstopped\n")
        return 130
    except (RuntimeError, ValueError) as error:
        sys.stderr.write(f"browser-use stopped: {error}\n")
        return 1
    if last is None:
        return 1
    if last["status"] == "done":
        print("Done. The page stays open. Continue from here.")
        print(last["page"]["url"])
        return 0
    print("Stopped. The page stays open.")
    print(last["page"]["url"])
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
