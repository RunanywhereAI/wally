---
name: wally-device-e2e
description: Run engine-agnostic wally modality e2e on Apple Neural Engine (NeuRT) and Snapdragon Hexagon NPU (QHexRT) devices. Use when adding overlay backends, proving LLM/STT/TTS/VLM/embed/diffusion on device, or when a PC only has one modality's bundles on disk.
---

# Wally device modality e2e

Do not write per-engine tests. The harness is `scripts/test/e2e-modalities.sh`,
called from `scripts/test/e2e.sh`. Keys are **primitives** (`llm`, `stt`, `tts`,
`vlm`, `embed`, `image`, `vad`, `rerank`, `segment`). wally picks the engine
from catalog framework, local path, or plugin priority. `--engine` is an
override (`WALLY_E2E_ENGINE`), never a required test input.

## Run

```bash
# Public CI (modelless): skip every modality
bash scripts/test/e2e.sh /path/to/wally

# Device: discover whatever is already on disk, then run each primitive
export RUNANYWHERE_HOME=/path/to/home          # already-pulled OSS models
export WALLY_E2E_MODEL_ROOTS=/path/to/hnpu:/path/to/coreml
bash scripts/test/e2e-modalities.sh /path/to/wally

# Or pin one primitive (path or catalog id)
WALLY_E2E_LLM=/path/to/lfm2_5_230m_HNPU \
WALLY_E2E_STT=/path/to/whisper_base_HNPU \
WALLY_E2E_TTS=/path/to/kitten_micro_0_8_HNPU \
WALLY_E2E_EMBED=/path/to/embeddinggemma_300m_HNPU \
  bash scripts/test/e2e-modalities.sh /path/to/wally
```

`WALLY_E2E_AUTO=1` pulls small OSS catalog defaults the **registered** backends
can run (`smollm2`, `whisper-tiny`, `piper`, `minilm`, `silero`, `mlx-qwen3`,
…). Never enable AUTO in public CI.

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
- **NeuRT image:** `--prompt` and `--out` required; `--steps 4` for smoke.
  Compiled zip, not the HF repo HTML. Tree needs `TextEncoder.mlmodelc` /
  `Unet.mlmodelc` / `VAEDecoder.mlmodelc`.
- **llama.cpp VLM:** do **not** add a literal `<image>` in the prompt — the
  SDK inserts `mtmd_default_marker()`. An extra `<image>` makes
  `mtmd_tokenize` see 0 media markers. A tiny PNG can `bad_alloc` in
  SmolVLM2 after the 512×512 warmup; skip or pass a real photo via
  `WALLY_E2E_VLM`.
- **segment:** binary P6 PPM, not PNG.
- STT has no `--engine` flag; put `-m` before the wav.

See `wally-e2e` for bottle/backends assertions and Apple MLX host link flags.
