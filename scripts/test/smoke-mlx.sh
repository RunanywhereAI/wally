#!/usr/bin/env bash
# MLX smoke — port of runanywhere-sdks/wally/scripts/test/smoke-mlx-cli.sh
#
# Builds the Swift MLX host, then exercises backends, LLM, TTS, STT, and VLM
# against the product `wally` binary (not the SDK DevTools playground).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${WALLY_BIN:-$ROOT/build/wally}"
HOME_DIR="${WALLY_HOME:-${RUNANYWHERE_MLX_SMOKE_HOME:-/tmp/wally-mlx-smoke}}"
PULL="${WALLY_SMOKE_PULL:-${RUNANYWHERE_MLX_SMOKE_PULL:-1}}"

LLM_MODEL="${WALLY_SMOKE_LLM:-${RUNANYWHERE_MLX_SMOKE_LLM:-mlx-qwen3-0.6b-4bit}}"
VLM_MODEL="${WALLY_SMOKE_VLM:-${RUNANYWHERE_MLX_SMOKE_VLM:-mlx-fastvlm-0.5b-bf16}}"
STT_MODEL="${WALLY_SMOKE_STT:-${RUNANYWHERE_MLX_SMOKE_STT:-mlx-qwen3-asr-0.6b-8bit}}"
TTS_MODEL="${WALLY_SMOKE_TTS:-${RUNANYWHERE_MLX_SMOKE_TTS:-mlx-soprano-1.1-80m-5bit}}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "smoke-mlx: Darwin only" >&2
  exit 0
fi
if [[ "$(uname -m)" != "arm64" ]]; then
  echo "smoke-mlx: Apple Silicon only" >&2
  exit 0
fi

bash "$ROOT/scripts/build/build-mlx.sh"
if [[ ! -x "$BIN" ]]; then
  echo "error: missing $BIN (scripts/build/build-mlx.sh should install it as build/wally)" >&2
  exit 1
fi

mkdir -p "$HOME_DIR"
wally() { "$BIN" --home "$HOME_DIR" "$@"; }

pull_if_enabled() {
  local model="$1"
  if [[ "$PULL" == "1" ]]; then
    wally models pull "$model"
  fi
}

require_file() {
  local path="$1"
  if [[ ! -s "$path" ]]; then
    echo "expected non-empty file: $path" >&2
    exit 1
  fi
}

require_text() {
  local label="$1"
  local value="$2"
  if [[ -z "${value//[[:space:]]/}" ]]; then
    echo "$label produced empty output" >&2
    exit 1
  fi
}

make_default_image() {
  local out="$1"
  base64 --decode >"$out" <<'PNG'
iVBORw0KGgoAAAANSUhEUgAAAGQAAABkCAIAAAD/gAIDAAAAiklEQVR4nO3QQQ3AIADAQMDwMPrvQJkQxZbB7iTn9cw7wF2mB6wH7AbYDbAbYDfAbgDdALsBdgPsBtgNsBtgN8BugN0AuwF2A+wG2A2wG2A3wG6A3QC7AXYD7AbYDbAbYDfAboDdALsBdgPsBtgNsBtgN8BugN0AuwF2A+wG2A2wG2A3wG6A3QC7AXYDXBuUBgGkrp6WAAAAAElFTkSuQmCC
PNG
}

echo "MLX backend smoke"
wally --json backends | grep -q '"name":"mlx"'
wally --json backends | grep -q '"name":"llamacpp"'

echo "LLM: $LLM_MODEL"
pull_if_enabled "$LLM_MODEL"
llm_out="$(wally run "$LLM_MODEL" "Say OK in one short sentence." --max-tokens 16 --temp 0.1)"
require_text "LLM" "$llm_out"
printf '%s\n' "$llm_out"

# Availability and direct-path inference alone missed the install-symlink
# regression: MLX's actual library loader follows a different resource path.
RUNANYWHERE_HOME="$HOME_DIR" bash "$ROOT/scripts/test/smoke-mlx-symlink.sh" "$BIN" "$LLM_MODEL"

# TEMP(llm-only cut): TTS, STT, and VLM are commented out of src/app.cpp for
# this release, and their catalog ids (mlx-soprano-*, mlx-qwen3-asr-*,
# mlx-fastvlm-*) are filtered out of the language-only catalog, so both
# `wally models pull` and the command itself would fail here -- and abort
# the whole script under `set -euo pipefail`. Skip and report the skip
# instead of running them. Restore these three sections unguarded when
# register_tts/register_stt/register_vlm are uncommented in src/app.cpp.
echo "TTS: $TTS_MODEL"
echo "TTS: SKIP (tts disabled for llm-only cut)"
# pull_if_enabled "$TTS_MODEL"
# tts_wav="$HOME_DIR/mlx-smoke-tts.wav"
# rm -f "$tts_wav"
# wally tts --model "$TTS_MODEL" --text "Hello from MLX text to speech." --output "$tts_wav"
# require_file "$tts_wav"

echo "STT: $STT_MODEL"
echo "STT: SKIP (stt disabled for llm-only cut)"
# pull_if_enabled "$STT_MODEL"
# stt_out="$(wally stt --model "$STT_MODEL" --input "$tts_wav")"
# require_text "STT" "$stt_out"
# printf '%s\n' "$stt_out"

echo "VLM: $VLM_MODEL"
echo "VLM: SKIP (vlm disabled for llm-only cut)"
# pull_if_enabled "$VLM_MODEL"
# image_path="${WALLY_SMOKE_IMAGE:-${RUNANYWHERE_MLX_SMOKE_IMAGE:-$HOME_DIR/mlx-smoke-image.png}}"
# if [[ -z "${WALLY_SMOKE_IMAGE:-${RUNANYWHERE_MLX_SMOKE_IMAGE:-}}" ]]; then
#   make_default_image "$image_path"
# fi
# vlm_out="$(wally run "$VLM_MODEL" --image "$image_path" \
#   "Describe the image in one short sentence." --max-tokens 32 --temp 0.1)"
# require_text "VLM" "$vlm_out"
# printf '%s\n' "$vlm_out"

echo "smoke-mlx: ok (tts/stt/vlm skipped for llm-only cut)"
