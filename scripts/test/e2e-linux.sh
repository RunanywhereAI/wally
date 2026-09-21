#!/usr/bin/env bash
# =============================================================================
# e2e-linux.sh
#
# Host-side port of runanywhere-sdks/core/tests/scripts/run-cli-e2e-linux.sh.
# That original built wally inside the SDK Docker image (RAC_BUILD_CLI=ON).
# WALLY is a kit consumer, so this drives an already-built binary instead:
#
#   scripts/test/e2e-linux.sh [path-to-wally]
#
# Always runs modelless contract checks. Inference + hermetic pull run when
# WALLY_TEST_MODEL_DIR is set (same layout as SDK download-test-models.sh).
# Set WALLY_E2E_REQUIRE_MODELS=1 to fail closed if that directory is missing.
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${1:-${WALLY_BIN:-}}"
if [[ -z "$BIN" ]]; then
  if [[ -x "$ROOT/build/wally" ]]; then
    BIN="$ROOT/build/wally"
  elif [[ -x "$ROOT/build/wally-cxx" ]]; then
    BIN="$ROOT/build/wally-cxx"
  elif [[ -x "$ROOT/build/wally.exe" ]]; then
    BIN="$ROOT/build/wally.exe"
  fi
fi
if [[ -z "$BIN" || ! -e "$BIN" ]]; then
  echo "usage: e2e-linux.sh <path-to-wally>" >&2
  exit 1
fi

MODEL_DIR="${WALLY_TEST_MODEL_DIR:-${RAC_TEST_MODEL_DIR:-}}"
LOG_DIR="${WALLY_TEST_LOG_DIR:-$ROOT/build/cli-e2e-logs}"
HOME_DIR="${RUNANYWHERE_HOME:-$(mktemp -d "${TMPDIR:-/tmp}/wally-e2e.XXXXXX")}"
cleanup() { [[ -z "${WALLY_KEEP_HOME:-}" ]] && rm -rf "$HOME_DIR"; }
trap cleanup EXIT
mkdir -p "$LOG_DIR" "$HOME_DIR"

wally() { "$BIN" --home "$HOME_DIR" "$@"; }

pass=0
fail=0
skip=0
failed_names=()
skipped_names=()
check() {
  local name="$1"
  local log="$LOG_DIR/${name}.log"
  echo -n "  ${name}... "
  if "$name" >"$log" 2>&1; then
    echo "PASS ($log)"
    pass=$((pass + 1))
  else
    echo "FAIL ($log)"
    fail=$((fail + 1))
    failed_names+=("$name")
  fi
}

# TEMP(llm-only cut): the non-LLM modality commands (tts/stt/vad/voice) are
# commented out of src/app.cpp and not registered for this release, so their
# checks below cannot pass or meaningfully fail -- they would just error out
# on "no such command". Report them as skipped instead of running them, so a
# green summary here is never mistaken for coverage of that surface. Switch
# the call sites back to `check` when register_vlm/register_stt/register_tts/
# register_voice are uncommented in src/app.cpp.
skip_case() {
  local name="$1"
  local reason="$2"
  echo "  ${name}... SKIP ($reason)"
  skip=$((skip + 1))
  skipped_names+=("$name")
}

smoke_version() { wally version | grep -E 'wally|[0-9]+\.[0-9]+'; }

smoke_backends() {
  local out
  out="$(wally backends)"
  echo "$out"
  echo "$out" | grep -qiE "llamacpp|llama"
  echo "$out" | grep -qiE "sherpa|onnx"
}

smoke_list_all() { wally models list --all; }

smoke_info_json() {
  wally --json info | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d.get("wally") or d.get("version")'
}

smoke_unknown() {
  set +e
  wally definitely-not-a-command
  local code=$?
  set -e
  test "$code" -ne 0
}

hermetic_pull_rm() {
  local silero="$MODEL_DIR/ONNX/silero-vad/silero_vad.onnx"
  test -f "$silero"
  local stage
  stage="$(mktemp -d)"
  cp "$silero" "$stage/silero_vad.onnx"
  (cd "$stage" && python3 -m http.server 8077 >/dev/null 2>&1 </dev/null &)
  local i
  for i in $(seq 1 10); do
    curl -sf http://127.0.0.1:8077/ >/dev/null 2>&1 && break
    sleep 1
  done
  wally --no-progress models pull http://127.0.0.1:8077/silero_vad.onnx
  wally models list | grep -q silero_vad
  wally models rm silero_vad --force
  ! wally models list | grep -q silero_vad
  pkill -f "http.server 8077" || true
  rm -rf "$stage"
}

stage_canonical() {
  mkdir -p "$HOME_DIR/Models/LlamaCpp" "$HOME_DIR/Models/ONNX"
  cp -R "$MODEL_DIR/LlamaCpp/qwen3-0.6b" "$HOME_DIR/Models/LlamaCpp/" 2>/dev/null || true
  cp -R "$MODEL_DIR/ONNX/silero-vad" "$HOME_DIR/Models/ONNX/" 2>/dev/null || true
}

llm_one_shot() {
  stage_canonical
  local out
  out="$(wally run qwen3-0.6b 'Reply with exactly: OK' --no-think --max-tokens 32)"
  echo "LLM said: $out"
  test -n "$out"
}

# TEMP(llm-only cut): tts/stt are commented out of src/app.cpp for this
# release. Kept here, unreachable via `check`, so this comes back verbatim
# once register_tts/register_stt are uncommented -- nothing else has to
# change.
tts_stt_roundtrip() {
  wally --no-progress pull piper || wally --no-progress pull piper-en
  wally tts --text "RunAnywhere runs models on device." --output /tmp/wally-e2e-tts.wav
  test -s /tmp/wally-e2e-tts.wav
  wally --no-progress pull whisper-tiny
  local transcript
  transcript="$(wally stt --input /tmp/wally-e2e-tts.wav)"
  echo "Transcript: $transcript"
  echo "$transcript" | grep -iE "run|anywhere|models|device"
}

# TEMP(llm-only cut): vad is commented out of src/app.cpp for this release.
# Kept here, unreachable via `check`, so this comes back verbatim once
# register_vad is uncommented -- nothing else has to change.
vad_segments() {
  wally --no-progress pull piper || wally --no-progress pull piper-en
  wally tts --text "Testing voice activity detection." --output /tmp/wally-e2e-vad.wav
  wally --json vad --input /tmp/wally-e2e-vad.wav | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d.get("segments") or d.get("speech") or isinstance(d, (dict, list))'
}

# TEMP(llm-only cut): voice is commented out of src/app.cpp for this release.
# Kept here, unreachable via `check`, so this comes back verbatim once
# register_voice is uncommented -- nothing else has to change.
voice_turn() {
  wally --no-progress pull piper || wally --no-progress pull piper-en
  wally --no-progress pull whisper-tiny
  wally tts --text "Hello there." --output /tmp/wally-e2e-turn.wav
  wally --json voice --input /tmp/wally-e2e-turn.wav --output /tmp/wally-e2e-reply.wav | python3 -c 'import json,sys; json.load(sys.stdin)'
  test -s /tmp/wally-e2e-reply.wav
}

serve_health() {
  stage_canonical
  wally serve qwen3-0.6b --port 8090 >/tmp/wally-e2e-serve.log 2>&1 </dev/null &
  local pid=$!
  local i
  for i in $(seq 1 30); do
    curl -sf http://127.0.0.1:8090/health >/dev/null 2>&1 && break
    sleep 1
  done
  curl -sf http://127.0.0.1:8090/health
  curl -sf http://127.0.0.1:8090/v1/models | grep -qi qwen
  kill "$pid" || true
  for i in $(seq 1 15); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
  done
  if kill -0 "$pid" 2>/dev/null; then
    echo "server did not exit after SIGTERM"
    kill -9 "$pid"
    exit 1
  fi
}

echo "=========================================="
echo "  wally e2e (host)"
echo "=========================================="
echo "wally:      $BIN"
echo "home:      $HOME_DIR"
echo "model dir: ${MODEL_DIR:-<unset>}"
echo "logs:      $LOG_DIR"

echo
echo "==> Modelless smoke"
check smoke_version
check smoke_backends
check smoke_list_all
check smoke_info_json
check smoke_unknown

if [[ -z "$MODEL_DIR" ]]; then
  if [[ "${WALLY_E2E_REQUIRE_MODELS:-0}" == "1" ]]; then
    echo "WALLY_TEST_MODEL_DIR is required (WALLY_E2E_REQUIRE_MODELS=1)" >&2
    exit 1
  fi
  echo
  echo "  skip  inference (set WALLY_TEST_MODEL_DIR to enable)"
else
  echo
  echo "==> Hermetic pull / rm (loopback HTTP, no WAN)"
  check hermetic_pull_rm
  echo
  echo "==> Real inference (canonical-layout models)"
  check llm_one_shot
  skip_case tts_stt_roundtrip "tts/stt disabled for llm-only cut"
  skip_case vad_segments "vad disabled for llm-only cut"
  skip_case voice_turn "voice disabled for llm-only cut"
  check serve_health
fi

echo
echo "Summary: $pass passed, $fail failed, $skip skipped"
if [[ "$skip" -gt 0 ]]; then
  echo "Skipped: ${skipped_names[*]}"
fi
if [[ "$fail" -gt 0 ]]; then
  echo "Failed: ${failed_names[*]}"
  echo "Logs: $LOG_DIR"
  exit 1
fi
echo "All wally e2e cases passed"
echo "Logs: $LOG_DIR"
