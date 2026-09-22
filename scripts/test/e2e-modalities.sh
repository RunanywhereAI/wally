#!/usr/bin/env bash
# Engine-agnostic modality e2e. Invoked from scripts/test/e2e.sh.
#
# Tests are keyed by primitive (llm, stt, tts, vlm, embed, image, vad, rerank,
# segment), not by engine name. wally routes via catalog framework / local path
# / plugin priority. --engine is never required; set WALLY_E2E_ENGINE only to
# force an overlay backend.
#
# Discovery (first hit wins per modality):
#   1. WALLY_E2E_<MOD> — catalog id or on-disk path
#   2. Legacy WALLY_E2E_{MODEL,MLX_MODEL,NEURT_MODEL,QHEXRT_MODEL}
#   3. Scan WALLY_E2E_MODEL_ROOTS + well-known dirs for *_HNPU / CoreML trees
#   4. `wally models list --json` downloaded rows whose modality matches
#   5. If WALLY_E2E_AUTO=1, small OSS catalog defaults the binary can run
#
# Skip = no model. Fail = a model was selected and the command failed.
# Public CI leaves every knob unset and stays green.
#
# Portable: macOS /bin/bash is 3.2 (no associative arrays). Never `find | head`
# under pipefail — SIGPIPE makes the whole script fail.
set -euo pipefail

WALLY="${1:?usage: e2e-modalities.sh <path-to-wally>}"

pass=0
fail=0
skip=0
ok()    { printf '  ok    %s\n' "$1"; pass=$((pass + 1)); }
bad()   { printf '  FAIL  %s\n' "$1"; fail=$((fail + 1)); }
skipm() { printf '  skip  %s\n' "$1"; skip=$((skip + 1)); }

MOD_LLM=""
MOD_STT=""
MOD_TTS=""
MOD_VLM=""
MOD_EMBED=""
MOD_IMAGE=""
MOD_VAD=""
MOD_RERANK=""
MOD_SEGMENT=""
MOD_DIARIZE=""

get_mod() {
  case "$1" in
    llm) printf '%s' "${MOD_LLM}" ;;
    stt) printf '%s' "${MOD_STT}" ;;
    tts) printf '%s' "${MOD_TTS}" ;;
    vlm) printf '%s' "${MOD_VLM}" ;;
    embed) printf '%s' "${MOD_EMBED}" ;;
    image) printf '%s' "${MOD_IMAGE}" ;;
    vad) printf '%s' "${MOD_VAD}" ;;
    rerank) printf '%s' "${MOD_RERANK}" ;;
    segment) printf '%s' "${MOD_SEGMENT}" ;;
    diarize) printf '%s' "${MOD_DIARIZE}" ;;
  esac
}

set_mod() {
  local mod="$1"
  local ref="$2"
  if [[ -z "${ref}" ]]; then
    return 0
  fi
  if [[ -n "$(get_mod "${mod}")" ]]; then
    return 0
  fi
  case "${mod}" in
    llm) MOD_LLM="${ref}" ;;
    stt) MOD_STT="${ref}" ;;
    tts) MOD_TTS="${ref}" ;;
    vlm) MOD_VLM="${ref}" ;;
    embed) MOD_EMBED="${ref}" ;;
    image) MOD_IMAGE="${ref}" ;;
    vad) MOD_VAD="${ref}" ;;
    rerank) MOD_RERANK="${ref}" ;;
    segment) MOD_SEGMENT="${ref}" ;;
    diarize) MOD_DIARIZE="${ref}" ;;
  esac
}

engine_args=()
if [[ -n "${WALLY_E2E_ENGINE:-}" ]]; then
  engine_args=(--engine "${WALLY_E2E_ENGINE}")
fi

workdir="$(mktemp -d "${TMPDIR:-/tmp}/wally-e2e-mod.XXXXXX")"
cleanup() { rm -rf "${workdir}"; }
trap cleanup EXIT

wav="${workdir}/tone.wav"
ppm="${workdir}/tiny.ppm"
png="${workdir}/tiny.png"
tts_wav="${workdir}/tts.wav"
img_out="${workdir}/gen.png"

python_bin=""
for cand in python3 python; do
  if command -v "${cand}" >/dev/null 2>&1; then
    python_bin="${cand}"
    break
  fi
done

write_wav() {
  if [[ -n "${python_bin}" ]]; then
    "${python_bin}" - "${wav}" <<'PY'
import math, struct, sys, wave
path = sys.argv[1]
sr, n = 16000, 8000
with wave.open(path, "w") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(sr)
    frames = b"".join(
        struct.pack("<h", int(6000 * math.sin(2 * math.pi * 440 * i / sr)))
        for i in range(n)
    )
    w.writeframes(frames)
PY
    return
  fi
  {
    printf 'RIFF'
    printf '%s' $'\x24\x1f\x00\x00'
    printf 'WAVEfmt '
    printf '%s' $'\x10\x00\x00\x00\x01\x00\x01\x00\x80\x3e\x00\x00\x00\x7d\x00\x00\x02\x00\x10\x00'
    printf 'data'
    printf '%s' $'\x00\x1f\x00\x00'
    dd if=/dev/zero bs=7936 count=1 2>/dev/null
  } > "${wav}"
}

write_ppm() {
  {
    printf 'P6\n8 8\n255\n'
    if [[ -n "${python_bin}" ]]; then
      "${python_bin}" -c 'import sys; sys.stdout.buffer.write(bytes([255,0,0])*64)'
    else
      dd if=/dev/zero bs=192 count=1 2>/dev/null
    fi
  } > "${ppm}"
}

write_png() {
  if [[ -z "${python_bin}" ]]; then
    return 1
  fi
  "${python_bin}" - "${png}" <<'PY'
import struct, zlib, sys
path = sys.argv[1]
def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
row = b"\x00" + (b"\xff\x00\x00" * 64)
png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">IIBBBBB", 64, 64, 8, 2, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(row * 64, 9))
png += chunk(b"IEND", b"")
open(path, "wb").write(png)
PY
}

write_wav
write_ppm
write_png || true

backend_json="$("${WALLY}" --json backends 2>/dev/null || true)"
has_backend() {
  local name="$1"
  printf '%s' "${backend_json}" | grep -F "\"${name}\"" >/dev/null 2>&1 || return 1
}

guess_mod() {
  local n
  n="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  n="${n##*/}"
  case "${n}" in
    *whisper*|*moonshine*|*parakeet*|*canary*|*asr*|*nemotron_asr*) echo stt ;;
    *kitten*|*kokoro*|*magpie*|*melott*|*piper*|*soprano*|*tts*) echo tts ;;
    *embed*|*minilm*|*siglip*) echo embed ;;
    *rerank*) echo rerank ;;
    *internvl*|*smolvlm*|*qwen2-vl*|*qwen2_vl*|*qwen3_vl*|*qwen3-vl*|*fastvlm*|*lfm2_5_vl*|*lfm2.5-vl*|*cosmos3_edge_vlm*|*nemotron_nano_vl*) echo vlm ;;
    *diffusion*|*sd15*|*stable-diffusion*|*cosmos3_edge_image*|*cosmos3_edge_diffusion*) echo image ;;
    *silero*) echo vad ;;
    *segformer*|*segment*) echo segment ;;
    *sortformer*|*diariz*) echo diarize ;;
    *) echo llm ;;
  esac
}

# 1. explicit per-modality env
set_mod llm     "${WALLY_E2E_LLM:-${WALLY_E2E_MODEL:-}}"
set_mod stt     "${WALLY_E2E_STT:-}"
set_mod tts     "${WALLY_E2E_TTS:-}"
set_mod vlm     "${WALLY_E2E_VLM:-}"
set_mod embed   "${WALLY_E2E_EMBED:-}"
set_mod image   "${WALLY_E2E_IMAGE:-}"
set_mod vad     "${WALLY_E2E_VAD:-}"
set_mod rerank  "${WALLY_E2E_RERANK:-}"
set_mod segment "${WALLY_E2E_SEGMENT:-}"
set_mod diarize "${WALLY_E2E_DIARIZE:-}"

# 2. legacy engine knobs (still accepted; tests stay modality-keyed)
if [[ -n "${WALLY_E2E_MLX_MODEL:-}" ]]; then
  set_mod "$(guess_mod "${WALLY_E2E_MLX_MODEL}")" "${WALLY_E2E_MLX_MODEL}"
fi
if [[ -n "${WALLY_E2E_NEURT_MODEL:-}" ]]; then
  set_mod "$(guess_mod "${WALLY_E2E_NEURT_MODEL}")" "${WALLY_E2E_NEURT_MODEL}"
fi
if [[ -n "${WALLY_E2E_QHEXRT_MODEL:-}" ]]; then
  set_mod "$(guess_mod "${WALLY_E2E_QHEXRT_MODEL}")" "${WALLY_E2E_QHEXRT_MODEL}"
fi

# 3. scan well-known roots
roots=""
append_root() {
  local r="$1"
  [[ -n "${r}" && -d "${r}" ]] || return 0
  case ":${roots}:" in
    *":${r}:"*) return 0 ;;
  esac
  if [[ -z "${roots}" ]]; then
    roots="${r}"
  else
    roots="${roots}:${r}"
  fi
}

if [[ -n "${WALLY_E2E_MODEL_ROOTS:-}" ]]; then
  old_ifs="${IFS}"
  IFS=':;'
  # shellcheck disable=SC2086
  for r in ${WALLY_E2E_MODEL_ROOTS}; do
    append_root "${r}"
  done
  IFS="${old_ifs}"
fi
append_root "${RUNANYWHERE_HOME:-}"
append_root "${HOME}/.runanywhere"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    append_root "${HOME}/Downloads/hnpu"
    append_root "${HOME}/Downloads/maple_bundle_root"
    if [[ -n "${USERPROFILE:-}" ]]; then
      append_root "${USERPROFILE}/Downloads/hnpu"
      append_root "${USERPROFILE}/Downloads/maple_bundle_root"
    fi
    ;;
esac

scan_root() {
  local root="$1"
  local dir parent
  while IFS= read -r dir; do
    [[ -n "${dir}" ]] || continue
    case "${dir}" in
      *.mlmodelc)
        parent="$(dirname "${dir}")"
        # TextEncoder/VAEDecoder can sort before Unet; first-hit-wins would
        # otherwise store a diffusion tree as llm.
        if [[ -d "${parent}/Unet.mlmodelc" ]]; then
          set_mod image "${parent}"
        else
          set_mod "$(guess_mod "${parent}")" "${parent}"
        fi
        ;;
      *_HNPU|*_ANE)
        # Incomplete HF snapshots (e.g. only host_weights/) are not runnable.
        if [[ -z "$(find "${dir}" -maxdepth 3 \( -name '*.json' -o -name '*.bin' -o -name '*.mlmodelc' \) ! -path '*/host_weights/*' -print -quit 2>/dev/null)" ]]; then
          continue
        fi
        set_mod "$(guess_mod "${dir}")" "${dir}"
        ;;
      *)
        set_mod "$(guess_mod "${dir}")" "${dir}"
        ;;
    esac
  done < <(find "${root}" -maxdepth 8 \( -type d \( -name '*_HNPU' -o -name '*_ANE' -o -name '*.mlmodelc' \) \) -print 2>/dev/null)
}

old_ifs="${IFS}"
IFS=':'
for root in ${roots}; do
  scan_root "${root}"
done
IFS="${old_ifs}"

# 4. downloaded catalog rows
list_json_file="${workdir}/models.json"
if "${WALLY}" --json models list >"${list_json_file}" 2>/dev/null; then
  if [[ -s "${list_json_file}" && -n "${python_bin}" ]]; then
    while IFS=$'\t' read -r mod ref; do
      # Windows Python writes text-mode stdout as CRLF; the \r would otherwise
      # ride along into the model id and make wally report it as unknown.
      ref="${ref%$'\r'}"
      [[ -n "${mod}" && -n "${ref}" ]] || continue
      set_mod "${mod}" "${ref}"
    done < <("${python_bin}" - "${list_json_file}" <<'PY'
import json, sys
path = sys.argv[1]
try:
    data = json.loads(open(path).read())
except Exception:
    sys.exit(0)
mod_map = {
    "llm": "llm", "stt": "stt", "tts": "tts", "vlm": "vlm",
    "embedding": "embed", "diffusion": "image", "vad": "vad",
    "segment": "segment", "diarize": "diarize",
}
for m in data.get("models") or []:
    if not m.get("downloaded"):
        continue
    mid = str(m.get("id") or "")
    if "rerank" in mid.lower():
        mod = "rerank"
    else:
        mod = mod_map.get(str(m.get("modality") or ""), "")
    ref = m.get("id") or m.get("local_path") or ""
    if mod and ref:
        print("%s\t%s" % (mod, ref))
PY
    )
  fi
fi

# 5. AUTO: small OSS catalog ids the registered backends can serve.
if [[ "${WALLY_E2E_AUTO:-0}" == "1" ]]; then
  if has_backend llamacpp; then
    set_mod llm smollm2
    set_mod vlm smolvlm2
    set_mod rerank bge-reranker
  fi
  if has_backend mlx; then
    set_mod llm mlx-qwen3
    set_mod tts mlx-soprano
    set_mod embed mlx-qwen3-embed
  fi
  if has_backend sherpa; then
    set_mod stt whisper-tiny
    set_mod tts piper
  fi
  if has_backend onnx; then
    set_mod vad silero
    set_mod embed minilm
    set_mod segment segformer
  fi
  if has_backend neurt; then
    set_mod image sd15
  fi
fi

echo "e2e (modalities, engine-agnostic): ${WALLY}"
if [[ -n "${WALLY_E2E_ENGINE:-}" ]]; then
  echo "  note  WALLY_E2E_ENGINE=${WALLY_E2E_ENGINE} (override only)"
fi
for mod in llm stt tts vlm embed image vad rerank segment diarize; do
  ref="$(get_mod "${mod}")"
  if [[ -n "${ref}" ]]; then
    echo "  pick  ${mod} -> ${ref}"
  fi
done

run_cmd() {
  local label="$1"
  shift
  set +e
  "$@" >/dev/null 2>"${workdir}/err"
  local rc=$?
  set -e
  if [[ "${rc}" -eq 0 ]]; then
    ok "${label}"
    return 0
  fi
  local err
  err="$(grep -E 'error:|FAIL|failed' "${workdir}/err" 2>/dev/null | grep -v 'PluginAvailability' | tail -n 1 | tr '\n' ' ' | cut -c1-240 || true)"
  if [[ -z "${err}" ]]; then
    err="$(tail -n 1 "${workdir}/err" 2>/dev/null | tr '\n' ' ' | cut -c1-240 || true)"
  fi
  bad "${label}${err:+ (${err})}"
}

# --- runners (no --engine unless WALLY_E2E_ENGINE is set) --------------------
if [[ -n "${MOD_LLM}" ]]; then
  run_cmd "llm ${MOD_LLM}" \
    "${WALLY}" llm generate -m "${MOD_LLM}" ${engine_args[@]+"${engine_args[@]}"} \
    "Reply with exactly: ok" --max-output-tokens 16
else
  skipm "llm (set WALLY_E2E_LLM or place a model under WALLY_E2E_MODEL_ROOTS)"
fi

if [[ -n "${MOD_TTS}" ]]; then
  run_cmd "tts ${MOD_TTS}" \
    "${WALLY}" tts synthesize "hello from wally" --output "${tts_wav}" \
    -m "${MOD_TTS}"
else
  skipm "tts (set WALLY_E2E_TTS)"
fi

if [[ -n "${MOD_STT}" ]]; then
  stt_in="${wav}"
  if [[ -s "${tts_wav}" ]]; then
    stt_in="${tts_wav}"
  fi
  run_cmd "stt ${MOD_STT}" \
    "${WALLY}" stt transcribe -m "${MOD_STT}" "${stt_in}"
else
  skipm "stt (set WALLY_E2E_STT)"
fi

if [[ -n "${MOD_VLM}" && -s "${png}" ]]; then
  run_cmd "vlm ${MOD_VLM}" \
    "${WALLY}" vlm generate -m "${MOD_VLM}" ${engine_args[@]+"${engine_args[@]}"} \
    --image "${png}" "What color is this?" --max-output-tokens 16
else
  skipm "vlm (set WALLY_E2E_VLM)"
fi

if [[ -n "${MOD_EMBED}" ]]; then
  run_cmd "embed ${MOD_EMBED}" \
    "${WALLY}" embed -m "${MOD_EMBED}" ${engine_args[@]+"${engine_args[@]}"} "hello"
else
  skipm "embed (set WALLY_E2E_EMBED)"
fi

if [[ -n "${MOD_IMAGE}" ]]; then
  run_cmd "image ${MOD_IMAGE}" \
    "${WALLY}" image generate --model "${MOD_IMAGE}" \
    --prompt "a red square" --out "${img_out}" --steps 4
else
  skipm "image (set WALLY_E2E_IMAGE to a compiled CoreML / HNPU diffusion tree)"
fi

if [[ -n "${MOD_VAD}" ]]; then
  run_cmd "vad ${MOD_VAD}" \
    "${WALLY}" vad detect -m "${MOD_VAD}" "${wav}"
else
  skipm "vad (set WALLY_E2E_VAD)"
fi

if [[ -n "${MOD_RERANK}" ]]; then
  run_cmd "rerank ${MOD_RERANK}" \
    "${WALLY}" rerank -m "${MOD_RERANK}" "fruit" \
    --doc "bananas are yellow" --doc "steel is a metal"
else
  skipm "rerank (set WALLY_E2E_RERANK)"
fi

if [[ -n "${MOD_SEGMENT}" ]]; then
  run_cmd "segment ${MOD_SEGMENT}" \
    "${WALLY}" segment -m "${MOD_SEGMENT}" "${ppm}"
else
  skipm "segment (set WALLY_E2E_SEGMENT; input is binary P6 PPM)"
fi

if [[ -n "${MOD_DIARIZE}" ]]; then
  run_cmd "diarize ${MOD_DIARIZE}" \
    "${WALLY}" diarize -m "${MOD_DIARIZE}" "${wav}"
else
  skipm "diarize (set WALLY_E2E_DIARIZE)"
fi

echo "modalities: ${pass} passed, ${fail} failed, ${skip} skipped"
exit "${fail}"
