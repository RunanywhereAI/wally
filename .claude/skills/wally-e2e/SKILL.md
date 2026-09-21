---
name: wally-e2e
description: Verify a built wally binary against a pinned C++ desktop kit on macOS and Windows. Use when CI smoke/e2e is red, backends are missing, DLLs fail to load, or the Apple MLX host fails to link.
---

# Wally e2e

Entry: `scripts/test/e2e.sh <path-to-wally>`. Always runs `scripts/test/smoke.sh`, then
`scripts/test/e2e-modalities.sh` (engine-agnostic primitives). Public CI leaves
modality knobs unset so every round-trip **skips**. Device runs set
`WALLY_E2E_<MOD>` / `WALLY_E2E_MODEL_ROOTS` / `WALLY_E2E_AUTO=1`. See
`wally-device-e2e` for ANE/NPU.

| Env | Primitive | Example |
|---|---|---|
| `WALLY_E2E_LLM` / `WALLY_E2E_MODEL` | llm | `mlx-qwen3` or a `*_HNPU` dir |
| `WALLY_E2E_STT` | stt | `whisper-tiny` or `whisper_base_HNPU` |
| `WALLY_E2E_TTS` | tts | `piper` or `kitten_micro_0_8_HNPU` |
| `WALLY_E2E_VLM` | vlm | `smolvlm2` (SDK inserts the media marker) |
| `WALLY_E2E_EMBED` | embed | `minilm` or `embeddinggemma_300m_HNPU` |
| `WALLY_E2E_IMAGE` | image | compiled SD1.5 tree / `sd15` |
| `WALLY_E2E_NEURT_MODEL` | classified by path | `sd15`, a Parakeet ANE tree, or `lfm2-230m-ane` |
| `WALLY_E2E_VAD` | vad | `silero` |
| `WALLY_E2E_RERANK` | rerank | `bge-reranker` |
| `WALLY_E2E_SEGMENT` | segment | `segformer` (P6 PPM) |
| `WALLY_E2E_ENGINE` | override only | `qhexrt` / `neurt` / `mlx` |

Legacy `WALLY_E2E_MLX_MODEL` / `WALLY_E2E_NEURT_MODEL` / `WALLY_E2E_QHEXRT_MODEL`
are classified by path/id into a primitive (not always image). Do not add new
engine-named knobs.

`scripts/test/assert-binary-backends.sh` greps `nm`/`llvm-nm`/`dumpbin`/`strings`
for registrar symbols (`raMLXRegisterRuntime`, `rac_plugin_entry_neurt`,
`rac_plugin_entry_qhexrt`, …) so a backends() listing cannot pass without
the engine actually being linked into the bottle.

Pass `WALLY_SDK_KIT` so overlay backends (`neurt` / `qhexrt`) are required
when those libs are in the kit. `CMAKE_PREFIX_PATH` is only used for
`HAS_*` flags and Windows DLL staging — an ambient overlay prefix must
not make a public OSS bottle fail for missing NeuRT.

## What "green" means

`scripts/test/assert-backends.sh` requires every engine the kit actually ships:

| Condition | Required `wally --json backends` name |
|---|---|
| no kit Config (public OSS bottle) | `llamacpp` + `onnx` + `sherpa` |
| kit `RunAnywhere_HAS_LLAMACPP` TRUE | `llamacpp` |
| kit `RunAnywhere_HAS_ONNX` TRUE | `onnx` |
| kit `RunAnywhere_HAS_SHERPA` TRUE | `sherpa` |
| Darwin arm64 product binary `wally` (not `wally-cxx`) | `mlx` |
| overlay `lib/librac_backend_neurt.a` / `rac_backend_neurt.lib` | `neurt` |
| overlay `lib/librac_backend_qhexrt.a` / `rac_backend_qhexrt.lib` | `qhexrt` |

Do **not** drop sherpa from the expected list to make 0.20.26 Windows green
while `HAS_SHERPA` is TRUE. That kit compiled sherpa with speech ops off
(`RAC_SHERPA_ROUTABLE=0`): `rac_backend_sherpa_register()` returned SUCCESS,
`capability_check` returned `BACKEND_UNAVAILABLE`, the registry refused the
plugin. The fix is pin a routable kit (0.20.28+), not weaken the assertion.

## `backends` must walk every live primitive

`src/commands/cmd_backends.cpp` iterates `1 .. RAC_PRIMITIVE_COUNT-1`, skipping
retired wire value 6. ONNX without RAG only advertises SEGMENT / DIARIZE. A
hardcoded GENERATE_TEXT / TRANSCRIBE / EMBED list made onnx invisible even when
the plugin was registered. Do not reintroduce a primitive allow-list.

## Windows DLLs

Win32 `LoadLibrary` searches the exe directory, then PATH. `e2e.sh` copies
`third_party` / `bin` / `lib` `*.dll` next to `wally.exe` and prepends those
dirs to PATH **before** smoke. Skipping that produces "llamacpp only" even when
the kit contains `rac_backend_onnx.lib` + `onnxruntime.dll`.

Never pass `onnxruntime.dll` to `link.exe` (LNK1107) — link the import lib;
stage the DLL at runtime.

GitHub Windows: `GITHUB_WORKSPACE` is `D:\a\...`; msys `tar -C` needs
`cygpath -u` (`fetch-kit.sh` already does).

## Apple MLX host link (Ninja)

`scripts/build/bundle-core.sh` merges everything `wally` links into `libwally_bundle.a`
for SwiftPM.

- Ninja **never** writes `CMakeFiles/wally.dir/link.txt` (Makefiles only).
- Harvest with `ninja -t commands wally | tail -1`. The **last** command is the
  link. Grepping for `wally` hits compile lines (`CMakeFiles/wally.dir/…`).
- Apple ld emits one token `-Wl,-force_load,/abs/path/lib.a`. That ends in
  `.a` but is not a path. Prefixing `BUILD/` produces
  `build/-Wl,-force_load,…`. Strip `-Wl,-force_load,` first.
- Ninja lists archives twice; `libtool -static` then fails on duplicate
  members unless you dedupe.

`scripts/build/build-mlx.sh` must dump the xcodebuild log on failure (`Undefined
symbols` does not contain `error:`). Do not grep bare `error:` — every
CompileC line contains `-Werror=`. Observed CI `32786359915`: grep
`error:|Metal|BUILD` left only `clang: error: linker command failed`.

Never put `#` comments in a `\`-continued `xcodebuild` invocation. Bash
cuts the command there, so `OTHER_LDFLAGS` and the log redirect never
run (empty `xcodebuild-mlx.log`, status taken from a later assignment).

Link flags that must survive the Swift host:

- `-Wl,-force_load,$BUILD/libwally_plugins.a` then `-L$BUILD -lwally_bundle`.
  Force-load **only** the plugin backends (static registrars). Do **not**
  force-load llama-common: that pulls `download.cpp.o`, which references
  cpp-httplib `Client::Get` methods the kit never emitted as objects
  (`wally-cxx` never needed that TU). Observed locally after revealing the
  real `Ld` log.
- `-L$KIT/third_party -lonnxruntime` and `-Wl,-rpath,$KIT/third_party` —
  `bundle-core.sh` rewrites the kit dylib to `-l` and must keep `-L` plus the
  rpath `wally-cxx` already had, or the Swift host abort-traps at launch
  (`Library not loaded: @rpath/libonnxruntime.dylib`). Harvest `-l*` as well
  as dylib conversions (`-ldl`, `-lbz2`).
- Canonicalize `WALLY_SDK_SWIFT_PATH` with `cd && pwd`. SwiftPM's local package
  identity is the **directory name**, so `…/EXTERNAL/Wally/../..` registers as
  `..`. Nested checkouts named `sdks1` must use that name in
  `.product(..., package:)`.

Do not point `WALLY_SDK_SWIFT_PATH` at an unreleased `Package.swift` whose
`sdkVersion` zips 404 (`v0.20.28` before publish). CI checks out the **tagged**
SDK tree (`ref: v$SDK`) whose binaryTargets already exist.

CI macOS runner is **macos-26** (Xcode 26 / Swift 6.2). The MLX host resolves
`RunanywhereAI/runanywhere-sdks` `Package.swift`, which is
`swift-tools-version: 6.2`. macos-15 is Xcode 16.4 / Swift 6.1 and fails after
a successful libtool merge with `package 'runanywhere-sdks' is using Swift
tools version 6.2.0 but the installed version is 6.1.0`. macos-14 is Swift
5.10. `release.yml` must use the same runner as `ci.yml`.

## Linux

Linux bottles are not a v1 merge blocker. Windows x64 and macOS arm64 are.
`scripts/test/e2e-linux.sh` exists for later.

## Private engines

NeuRT / QHexRT only appear in `backends` when the overlay was applied.
`scripts/test/e2e.sh` requires `neurt` / `qhexrt` when
`lib/librac_backend_neurt.a` or `lib/rac_backend_qhexrt.lib` exists — not by
grepping packaged `HAS_NEURT FALSE` (that stays false; find_package flips it
when the archive is present). Public CI must pass without overlays. Image gen
(`cmd_image.cpp`) is compiled out unless NeuRT is present.

## Device / overlay gotchas (0.5.1 + kit 0.20.28)

Public bottles never list `neurt` or `qhexrt`. That is the product, not a
test gap. Overlay-rebuild the product binary (`WALLY_APPLE_MLX_HOST=ON` on
Mac; ARM64 MSVC + QHexRT overlay on Snapdragon).

- **`CMAKE_PREFIX_PATH` is not an overlay opt-in.** Only `WALLY_SDK_KIT`
  makes e2e require `neurt`/`qhexrt`. An ambient overlay prefix from a
  previous rebuild will otherwise fail a public-bottle run.
- **Binary assert:** never `nm | grep -q` under `pipefail` (SIGPIPE → false
  FAIL). Stream `strings -a` / `nm -a`. Darwin MLX proof is
  `mlx-swift_Cmlx.bundle` next to product `wally` (`nm -gU` misses Swift
  host symbols). First C++ `rac_plugin_register(mlx)` logs `-811`; Swift
  callbacks then register MLX — noisy, not a miss.
- **Windows ARM64 public/overlay kits have `HAS_LLAMACPP FALSE`.** Do not
  require `llamacpp` in e2e. Overlay `wally.exe` listing **only** `qhexrt`
  (priority 150) is correct. On-disk GGUF (`qwen3.5-2b`) cannot run there.
- **QHexRT generate needs QAIRT matching the device skel, not the overlay
  DLL set.** Snapdragon X2 Elite / Hexagon v81: `QNN_SDK_ROOT` +
  `ADSP_LIBRARY_PATH=%QNN_SDK_ROOT%\lib\hexagon-v81\unsigned`, copy
  `aarch64-windows-msvc` `QnnHtp.dll` / `QnnHtpPrepare.dll` /
  `QnnHtpV81Stub.dll` / `QnnHtpV81CalculatorStub.dll` / `QnnSystem.dll`
  next to `wally.exe`. Overlay 2.47 DLLs vs device 2.41 skels fail; QAIRT
  **2.48** worked. Pass the `*_HNPU` directory (`--engine qhexrt`), not a
  GGUF. FastRPC `openSession` timeouts (~90s) then user-driver fallback
  are normal; a second generate while DSP is wedged fails with
  `Skel failed to process context binary` / `0x3ea` — `taskkill wally.exe`
  and use a `.bat` with **fully expanded** `ADSP_LIBRARY_PATH` (nested
  `%QNN_SDK_ROOT%` in `cmd /c "set A=…&& set B=%A%\…"` does not expand).
- **VS on the ARM64 box may be 2026 / 18 Community**, not 2022:
  `C:\Program Files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvarsarm64.bat`.
  CMake/Ninja live under VS CMake extensions; they are not on default PATH.
- **v0.20.28 Windows ARM64 public kit omits `libcurl.lib`.** Copy from
  `arm64-windows-static` into the kit `lib/` before linking (fixed in the
  SDK packager for the *next* kit; do not retag 0.20.28). Wally already
  links kit `libcurl.lib` when present.
- Published product bottles: macOS `wally-$V-macos-arm64.tar.gz`, Windows
  **x64** zip. There is no public Windows ARM64 bottle; NPU is overlay-only.
- **The private QHexRT overlay tarball used to ship zero skel files** (only
  `.dll`/`.lib`, no `.so`/`.cat`) — `rac-cli`'s own overlay build could not
  run `qwen3.8-27b-1bit-npu` (the Bonsai/Maple ternary decoder) out of the
  box; validating it required hand-copying `librun_main_on_hexagon_skel.so`
  + `.cat` in from the `electron-qhexrt` npm package as a workaround. Fixed
  in `runanywhere-sdks`' `scripts/build/package-private-engine-overlay.sh`
  (widened the copy filter and added a pass for `dsp/win-arm64/`). **Wally
  itself never had the `ADSP_LIBRARY_PATH` bug the Electron binding had** —
  `fastrpc_win.cpp`'s `exe_dir()` fallback naturally resolves for `wally.exe`
  because dependent DLLs/skels are staged flat beside the executable by this
  repo's own packaging convention — but that protection is a property of the
  *packaging layout*, not of Wally's code, so it is not something to assume
  going forward. **Always build a fresh overlay from the actual release
  script and run the ternary model against it after any SDK kit-pin bump**
  that touches QHexRT — do not assume last time's manually-patched overlay
  is still representative of what a real user's overlay build produces.
- **The private overlay tarball must be EXTRACTED ON TOP OF `kit/`, merging
  into the same directory tree (`overlay/bin/*` → `kit/bin/`, `overlay/lib/*`
  → `kit/lib/`, `overlay/include/*` → `kit/include/`, `overlay/share/...` →
  `kit/share/...`) — never kept as a separate sibling `overlay/` directory
  fed to CMake via a second `CMAKE_PREFIX_PATH` entry.** `wally_stage_windows_runtime_dlls()`
  (`cmake/RunAnywhereSDK.cmake`) only ever copies from
  `${RunAnywhere_LIBRARY_DIR}/../bin` — i.e. `kit/bin` — so a same-named
  `overlay/bin` sitting next to `kit/` is silently never consulted. Worse,
  this fails **completely silently**: the build succeeds, `wally.exe` links,
  and `wally backends --json` returns `{"backends":[]}` with no error naming
  QHexRT at all (`find_library`-style detection in `RunAnywhereSDK.cmake`
  just doesn't find `kit/lib/rac_backend_qhexrt.lib` because it was never
  copied there). If a fresh overlay build reports zero backends, check this
  BEFORE suspecting the overlay tarball's contents.
- **`qwen3.8-27b-1bit-npu`'s `HostOpFailed` had THREE compounding causes,
  found and fixed one at a time — a kit-pin bump to v0.20.31 alone was NOT
  enough; Wally needed its own additional fix (below) even with a perfectly
  merged overlay.**
  1. The overlay-skel-files-never-shipped bug (above), fixed upstream in
     `runanywhere-sdks`' overlay packaging script.
  2. `qhexrt::qnn::Backend::profile()` — called (via the same
     `engines/qhexrt/qhexrt_session.cpp` this repo statically links, same as
     the Electron binding) to pick the `v75`/`v79`/`v81` manifest directory
     before the manifest is even parsed — shared its device query with the
     code path that opens a real QNN HTP device, so the ternary decoder's
     `host_only` manifest paid for a live QNN device it never needed. Fixed
     in `neurun` v0.20.31 (`Backend::profile()` no longer shares
     `ensure_device()` with `device()`) — see that repo's
     `qhexrt-profile-must-not-create-live-device` KB finding.
  3. **Wally-specific, and NOT fixed by the kit-pin bump alone**:
     `copy-overlay-dlls.cmake` globbed `*.dll` only, so even a correctly
     merged overlay (per the bullet above) left the Bonsai skel's `.so`/
     `.cat` sitting in `kit/bin/` and NEVER staged next to `wally.exe` — the
     one place `fastrpc_win.cpp`'s `ADSP_LIBRARY_PATH ∪ exe_dir()` search
     actually looks. Fixed by widening the glob to `*.dll *.so *.cat`.
  **`fastrpc_win.cpp`'s `SET_PATH`/`GET_PATH` both returning a non-zero rc
  (`0x14`/`AEE_EUNSUPPORTED`) is EXPECTED and HARMLESS on this driver
  (`libcdsprpc` 11.1.4 simply doesn't implement that control call — see that
  file's own header comment) — do not treat it as a symptom of anything.**
  This was chased as a diagnostic signal once and wasted real device time;
  the only signal that matters is whether `remote_handle64_open` for the
  skel itself returns non-zero (`0x80000406` = `AEE_EUNABLETOLOAD`, which
  that same file's header comment exhaustively catalogs the causes of —
  missing skel, missing/wrong/stale `.cat`, or — as this entry adds — the
  pair never being in the searched directory at all).
  Confirmed fixed end to end on a Snapdragon X2 Elite with all three fixes
  in place: `wally run --engine qhexrt` against `qwen3.8-27b-1bit-npu` opens
  the cDSP session and generates correctly ("The capital of France is
  **Paris**.", 0.105 tok/s, 12425 DSP linears).
