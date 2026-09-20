# Engines and platforms

Moved out of the README. What each engine runs, where it exists, and how wally picks one.

## Backends

One `wally` binary. **Catalog models already name their engine** (GGUF → llama.cpp, `mlx-*` → MLX, Core ML → NeuRT, QNN-context → QHexRT). You normally do not pick one.

Override only when you mean it:

```bash
wally llm generate --engine mlx -m mlx-qwen3 "Hello"
wally run --engine qhexrt /path/to/lfm2_5_230m_HNPU "Hello"
```

`--engine` accepts `mlx`, `llamacpp`, `sherpa`, `onnx`, `neurt` / `coreml` / `ane`, and `qhexrt` / `qnn` / `npu` / `hexagon`. If you omit it, commons picks the highest-priority **registered** backend that implements that primitive:

| Priority | Engine | Who wins unpinned work |
|---|---|---|
| 150 | QHexRT | Every primitive it implements, and only on a Windows ARM64 overlay binary (often the *only* engine in that binary) |
| 110 | MLX | Apple GPU: LLM / VLM / TTS / STT / embeddings when an `mlx-*` model is not already pinned |
| 100 | llama.cpp | GGUF LLM / VLM / embed / rerank |
| 100 | NeuRT | Core ML only. Stays at 100 **on purpose** so it never steals GGUF/MLX traffic. A Core ML bundle reaches NeuRT by framework pin, not by winning priority |
| 90 | Sherpa-ONNX | STT / TTS / VAD |
| 50 | ONNX Runtime | embeddings / VAD / diarization / segmentation |

`wally backends` is the source of truth for **this** binary. Public bottles never list `neurt` or `qhexrt`. Those engines are private overlays, never Homebrew / GitHub Release assets.

### Where each engine exists

| Backend | macOS Apple Silicon | Windows x64 | Windows ARM64 | Linux x64 |
|---|---|---|---|---|
| [llama.cpp](https://github.com/ggml-org/llama.cpp) | public bottle | public bottle | — | source build only |
| [MLX](https://github.com/ml-explore/mlx) (Apple GPU) | public bottle (product `wally`, not `wally-cxx`) | — | — | — |
| [Sherpa-ONNX](https://github.com/k2-fsa/sherpa-onnx) | public bottle | public bottle | — | source build only |
| [ONNX Runtime](https://onnxruntime.ai) | public bottle | public bottle | — | source build only |
| NeuRT (Apple Neural Engine; Core ML is the format) | **overlay** rebuild | — | — | — |
| QHexRT (Qualcomm Hexagon NPU) | — | — | **overlay** rebuild | — |

Public Windows ARM64 kits are commons-only (no llama.cpp / ONNX / Sherpa on MSVC ARM64). Snapdragon NPU is overlay-only. x64 Windows has no Hexagon path.

### Modalities × engines

Yes = this engine implements the primitive. Try = a catalog id that `wally pull` / a local path can run. Overlay engines still need the matching **on-disk bundle** (compiled `.mlmodelc` tree, or `*_HNPU` / `v81/` QNN-context dir) — a Hugging Face *repo page* is HTML, not a model.

| Modality | Command | llama.cpp | MLX | Sherpa | ONNX | NeuRT | QHexRT |
|---|---|---|---|---|---|---|---|
| LLM | `wally run` / `llm generate` | yes · `smollm2`, `qwen3` | yes · `mlx-qwen3` | — | — | yes · `lfm2-230m-ane` local Core ML tree | yes · `lfm2-230m-npu` local `*_HNPU` |


MLX registers with a one-line `-811` then Swift callbacks install it — that warning is expected. `image generate` is compiled only when NeuRT is linked; `--prompt` and `--out` are required (not a positional prompt). `--steps 4` is enough for a smoke PNG.

QHexRT on device also needs QAIRT matching the Hexagon skel (`QNN_SDK_ROOT` + `ADSP_LIBRARY_PATH=…\lib\hexagon-v81\unsigned` on v81). Overlay 2.47 DLLs vs a 2.41/2.48 device skel will fail to instantiate graphs. Pass the `*_HNPU` directory, not a GGUF. GGUF files cannot run on the ARM64 overlay binary (no llama.cpp).


## macOS vs Windows

**macOS Apple Silicon** (public bottle): llama.cpp + MLX + Sherpa + ONNX. Pull `qwen3` (GGUF) or `mlx-qwen3` (GPU). Image generation is NeuRT (`sd15`) and only works after the private overlay is linked into product `wally`.

**Windows x64** (public zip): GGUF / ONNX / Sherpa. No MLX, no NeuRT, no QHexRT.

**Windows ARM64** (Snapdragon): public kit has no llama.cpp/ONNX/Sherpa. The QHexRT overlay runs Hexagon NPU models from a local `*_HNPU` tree. Do not expect `mlx-*`, GGUF, or `sd15` on that binary.

`wally serve` is macOS and Linux.

Device round-trips are **by modality**, not by engine. `scripts/test/e2e.sh` always
runs `scripts/test/e2e-modalities.sh`; public CI leaves the knobs unset and skips.
On a machine that already has models:

```bash
export RUNANYWHERE_HOME=/path/to/home          # already-pulled OSS models
export WALLY_E2E_MODEL_ROOTS=/path/to/hnpu      # *_HNPU / *_ANE / *.mlmodelc trees
bash scripts/test/e2e-modalities.sh /path/to/wally   # no --engine required
```

`WALLY_E2E_LLM`, `WALLY_E2E_STT`, `WALLY_E2E_IMAGE`, … pin one primitive. Catalog
ids (`mlx-qwen3`, `whisper-base-npu`) pin the framework; a Hugging Face repo
page is HTML, not a bundle.

