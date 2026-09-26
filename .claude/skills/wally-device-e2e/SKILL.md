---
name: wally-device-e2e
description: Run wally's LLM e2e on Apple Neural Engine (NeuRT) and Snapdragon Hexagon NPU (QHexRT) devices. Use when adding overlay backends, proving LLM inference on device, or when a PC only has one backend's bundles on disk. Non-LLM modalities (STT/TTS/VLM/embed/image/VAD/rerank/segment/diarize) are deferred while the LLM-only cut is in effect.
---

# Wally device modality e2e

Do not write per-engine tests. The harness is `scripts/test/e2e-modalities.sh`,
called from `scripts/test/e2e.sh`, keyed by primitive (`llm`, `stt`, `tts`,
`vlm`, `embed`, `image`, `vad`, `rerank`, `segment`, `diarize`). This build's
LLM-only cut (`src/app.rs`) registers only the `llm` command — every other
primitive's wally subcommand is commented out, so pointing the harness at one
now fails with "no such command", not a skip. Run `llm` only until that cut is
lifted. wally picks the engine from catalog framework, local path, or plugin
priority. `--engine` is an override (`WALLY_E2E_ENGINE`), never a required
test input.

## Run

```bash
# Public CI (modelless): skip every modality
bash scripts/test/e2e.sh /path/to/wally

# Device: discover whatever is already on disk, then run llm
export RUNANYWHERE_HOME=/path/to/home          # already-pulled OSS models
export WALLY_E2E_MODEL_ROOTS=/path/to/hnpu:/path/to/coreml
bash scripts/test/e2e-modalities.sh /path/to/wally

# Or pin the model explicitly (path or catalog id)
WALLY_E2E_LLM=/path/to/lfm2_5_230m_HNPU \
  bash scripts/test/e2e-modalities.sh /path/to/wally
```

`WALLY_E2E_AUTO=1` also sets defaults for `stt`/`tts`/`vlm`/`embed`/`vad`/
`rerank`/`segment`/`image` (`whisper-tiny`, `piper`, `minilm`, …); on this
LLM-only cut every one of those now fails with "no such command" instead of
skipping, since their wally subcommand does not exist. Only the `llm` default
(`smollm2` / `mlx-qwen3`) actually runs — treat any other AUTO failure as the
disabled command, not your change. Never enable AUTO in public CI.

## Local model ids

The Windows ARM64 box often only has LFM `*_HNPU` trees under `Downloads\hnpu`,
copied for LLM smoke. Catalog ids:

| QHexRT id (local `*_HNPU`) | NeuRT id (local Core ML tree) |
|---|---|
| `lfm2_5_230m` | `lfm2_5_230m_ane` |


`wally models pull` of a Hugging Face **repo page** is HTML. Pass the expanded
directory to `-m`. Download `v81/*` only on Hexagon v81.

Skip with a clear "no bundle" when the tree is missing. Fail only when a
model was selected and the command failed.

## Overlay gotchas

- Public bottles never list `neurt` / `qhexrt`. Rebuild product `wally` against
  an overlay kit (`WALLY_SDK_KIT` pointing at that prefix).
- **QHexRT:** QAIRT **2.48** on Snapdragon X2 Elite / Hexagon v81.
  `ADSP_LIBRARY_PATH` must be the fully expanded
  `...\lib\hexagon-v81\unsigned` path. Nested `%QNN_SDK_ROOT%` in `cmd /c set`
  does not expand. Copy `QnnHtp*.dll` next to `wally.exe`. FastRPC ~90s then
  user-driver fallback is normal. Use a `.bat`, not nested `cmd /c`.

Non-LLM overlay coverage (NeuRT image generation, llama.cpp VLM, segment, STT)
is deferred, not deleted: those primitives run through this same harness once
`src/app.rs`'s LLM-only cut is uncommented, but until then their wally
subcommands do not exist, so this skill does not instruct running them.

See `wally-e2e` for bottle/backends assertions and Apple MLX host link flags.
