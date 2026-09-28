# Local verification (kit → wally)

Record pass/fail per OS. Do not treat a screenshot as a pass.

## Kit

```bash
# from runanywhere-sdks
./scripts/build/package-cpp-desktop.sh cpp-desktop-macos-arm64
# staged prefix: dist/cpp-desktop-macos-arm64
```

Kit must contain `include/rac/**`, `lib/librac_commons.a` (or equivalent), `lib/cmake/RunAnywhere/RunAnywhereConfig.cmake`, `share/runanywhere/idl/*.proto`. No `wally` binary.

## Wally (this repo)

```bash
cmake -B build -G Ninja -DCMAKE_PREFIX_PATH=<kit> -DCMAKE_BUILD_TYPE=Release
cmake --build build -j "$(sysctl -n hw.logicalcpu)"
# Apple product binary is build/wally (Swift MLX host). Windows: build/wally.exe
```

`WALLY_SDK_DIR` pointing at SDK **source** must configure-fail.

## Matrix

| Check | macOS arm64 | Windows x64 |
| --- | --- | --- |
| `--version` prints product + kit pin | | |
| `--help` / no-TTY bare `wally` | | |
| TTY bare `wally` → REPL | | |
| `backends` / `engines` (no stubs for missing packs) | | |
| LLM generate + stream (llama.cpp) | | |
| MLX listed by `wally backends` (same binary) | yes | n/a |
| NeuRT (only with pack + `NEURUN_TOKEN`) | skip | |
| STT / TTS / VAD (Sherpa) | | |
| embed / rerank | | |
| image (skip with engines reason if no NeuRT) | | |
| segment / diarize | | |
| voice STT→LLM→TTS | | |
| RAG if `RAC_HAVE_RAG` | | |
| `serve` | skip Windows | |
| `--json` on list/show/generate | | |

QHexRT: skip unless the box is Windows ARM64.
