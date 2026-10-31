#!/usr/bin/env bash
# Real-app round trip for `wally openwork` on macOS: launches the installed
# OpenWork on wally's private profile, asks its engine which models it offers,
# sends one prompt per provider, quits the app and checks nothing is left
# running. Opt-in: needs WALLY_E2E_OPENWORK=1, OpenWork.app, and a model
# (WALLY_E2E_OPENWORK_MODEL, a local id or a hosted one on a signed-in
# account). WALLY_E2E_OPENWORK_HOSTED names a hosted model to prompt as well.
#
#   WALLY_E2E_OPENWORK=1 WALLY_E2E_OPENWORK_MODEL=qwen3-4b-instruct-2507 \
#       bash scripts/test/e2e-openwork.sh ./build/wally
set -euo pipefail

WALLY="${1:?usage: e2e-openwork.sh <wally binary>}"
if [[ "${WALLY_E2E_OPENWORK:-0}" != "1" ]]; then
    echo "  skip  openwork (WALLY_E2E_OPENWORK is not 1)"
    exit 0
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "  skip  openwork (macOS only)"
    exit 0
fi
MODEL="${WALLY_E2E_OPENWORK_MODEL:?set WALLY_E2E_OPENWORK_MODEL}"
HOSTED="${WALLY_E2E_OPENWORK_HOSTED:-}"
STATE="${XDG_STATE_HOME:-${HOME}/.local/state}/runanywhere/openwork"
ENGINES="${STATE}/config/engine-instances.json"
LOG="$(mktemp -t wally-openwork-e2e)"

if pgrep -f "OpenWork.app/Contents/MacOS/OpenWork" >/dev/null; then
    echo "  FAIL  openwork: quit every OpenWork window first, so the instance this run starts can be told apart"
    exit 1
fi

# Only a folder this run creates counts against it.
HOME_WORKSPACE_EXISTED=0
[[ -e "${HOME}/OpenWork Chat" ]] && HOME_WORKSPACE_EXISTED=1

rm -f "${ENGINES}"
"${WALLY}" openwork -m "${MODEL}" >"${LOG}" 2>&1 &
WALLY_PID=$!
cleanup() {
    osascript -e 'tell application "OpenWork" to quit' >/dev/null 2>&1 || true
    for _ in $(seq 1 30); do
        kill -0 "${WALLY_PID}" 2>/dev/null || return 0
        sleep 1
    done
    echo "  FAIL  openwork: wally was still running 30 s after the app was asked to quit"
    kill "${WALLY_PID}" 2>/dev/null || true
    return 1
}
trap cleanup EXIT

for _ in $(seq 1 120); do
    [[ -s "${ENGINES}" ]] && break
    sleep 1
done
if [[ ! -s "${ENGINES}" ]]; then
    echo "  FAIL  openwork: the engine never started"
    cat "${LOG}"
    exit 1
fi

python3 - "${ENGINES}" "${STATE}/workspace" "${MODEL}" "${HOSTED}" <<'PY'
import json, sys, time, urllib.parse, urllib.request

engines, workspace, model, hosted = sys.argv[1:5]
# The file is written before the engine is registered in it.
for _ in range(120):
    entries = json.load(open(engines)).get("entries") or []
    if entries:
        break
    time.sleep(1)
assert entries, "the engine never registered"
entry = entries[0]
base, auth = entry["url"], entry["authProbe"]
query = "?directory=" + urllib.parse.quote(workspace)

def call(path, body=None, timeout=30):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(base + path + query, data=data,
                                     headers={"Authorization": auth, "Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response)

for _ in range(60):
    try:
        providers = call("/config/providers")
        break
    except OSError:
        time.sleep(1)
by_id = {p["id"]: p for p in providers["providers"]}
connected = call("/provider")["connected"]
selected = call("/config").get("model", "")
ours = [pid for pid in ("runanywhere", "runanywhere-local") if pid in by_id]
assert ours, f"no RunAnywhere provider in {sorted(by_id)}"
for pid in ours:
    assert pid in connected, f"{pid} is not connected: {connected}"
    print(f"  ok    openwork lists {len(by_id[pid]['models'])} models under {pid}")
assert selected.endswith("/" + model), f"selected {selected!r}, launched {model!r}"
print(f"  ok    openwork selects {selected}")

targets = [(selected.split("/", 1)[0], model)]
if hosted:
    targets.append(("runanywhere", hosted))
for provider, model_id in targets:
    session = call("/session", {})["id"]
    reply = call(f"/session/{session}/message", {
        "model": {"providerID": provider, "modelID": model_id},
        "parts": [{"type": "text", "text": "Reply with exactly the word pong and nothing else."}],
    }, timeout=300)
    error = reply.get("info", {}).get("error")
    text = " ".join(p.get("text", "") for p in reply.get("parts", []) if p.get("type") == "text")
    assert not error and text.strip().strip(".").lower() == "pong", f"{provider}/{model_id}: error={error} text={text!r}"
    print(f"  ok    openwork answered on {provider}/{model_id}: {text.strip()[:40]!r}")
PY

trap - EXIT
cleanup || exit 1
# The engine is stopped by the app on its way out, which takes a few seconds.
for _ in $(seq 1 20); do
    pgrep -f "OpenWork.app/Contents/Resources/sidecars/opencode" >/dev/null || break
    sleep 1
done
if pgrep -f "OpenWork.app/Contents/Resources/sidecars/opencode" >/dev/null; then
    echo "  FAIL  openwork: its engine is still running after the app quit"
    exit 1
fi
if [[ "${HOME_WORKSPACE_EXISTED}" == 0 && -e "${HOME}/OpenWork Chat" ]]; then
    echo "  FAIL  openwork: the default workspace landed in the real home"
    exit 1
fi
echo "  ok    openwork quit cleanly"
