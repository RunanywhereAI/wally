# Local model harness handoff

Owner: @Siddhesh2377. Updated 2026-09-22 UTC.

- Wally: https://github.com/RunanywhereAI/wally/pull/127 (`codex/wally-local-harness`, based on main `02d8d06`).
- SDK: https://github.com/RunanywhereAI/runanywhere-sdks/pull/964 (`codex/local-harness-server`, based on main `067778a383`). SDK handoff head: `47b3baf2d9f8929eb3bfbcc4e8e9f72f2ca9934c`.
- **Wally remains draft. The fixed desktop kits are not published or pinned yet. Do not ship this against the current 0.20.37 pin.** Work stopped at the owner's request for this handoff; neither PR has been merged.

## What changed and why

Wally previously rejected every local model before reaching its existing server. A local launch now loads the model, starts the SDK OpenAI-compatible server on an ephemeral loopback port, passes that endpoint and the actual context budget to the child harness, then cleans up the server and temporary configuration when the child exits. `--home` reaches model resolution. Existing hosted routing stays covered by tests.

The SDK server previously lost conversation/tool-result history and streamed tool JSON as ordinary text. It now preserves the full conversation, emits structured tool calls for JSON and SSE, separates reasoning, serializes access to the loaded model, and handles cancellation and terminal/error states. Release-only context, logging, and desktop packaging changes were restored to main so rebuilding does not lose APIs already consumed by Wally. The old `rac_llm_create()` still forwards a null configuration; public struct/vtable layouts were not changed.

MLX was parsing tool calls twice: MLX-LM intercepted frames before the SDK parser received them. Standard text models now deliver raw generated text to the SDK parser. GPT-OSS/Harmony and Atem/Onyx retain their existing decoder. Stop strings still suppress trailing text; producer cancellation drains final token usage and waits for completion. VLM generation was not changed.

Wally help now has clearer grouping, examples, and terminal-aware styling. Wrapper `--help` displays Wally help; explicit `-- --help` is still forwarded. MLX shader resources are staged beside the physical binary so installed symlinks work. The Apple linker arguments also work with Swift 6.4/Xcode 27. SDK workflow pins can now be complete immutable refs, including commit SHAs, without escaping the version consistency check.

## Verified results and limits

| Check | Result |
| --- | --- |
| Wally native build and CTest | 16/16 passed, 20.35 seconds on macOS |
| SDK full native build | Passed |
| SDK broad CTest | 101/103 passed; `tool_calling_session_proto_tests` and `tool_calling_run_loop_tests` hit the explicit 30-second timeout. Not yet classified as existing vs introduced. |
| SDK focused server/thinking/tool suites | 3/3 passed; HTTP suite has 65 assertions |
| MLX stop filtering | Eight cases passed, including split stop markers |
| Actual pinned MLX cancellation regression | Passed without a downloaded model: retained prompt=37/completion=8, output `ab`, no tail |
| SDK exports and instruction synchronization | 902 declarations/929 exports and instruction checks passed |
| Wally SDK-ref checker | 12 regression tests passed |
| Apple product | Full Swift/MLX binary built with updated SDK source and Xcode 27 |
| OpenCode 1.18.32 + Qwen3 0.6B GGUF | Actual file-read tool cycle, result returned to model, final marker, exit 0; 4.62 seconds |
| OpenCode 1.18.32 + Qwen3 0.6B MLX | Same actual tool cycle, exit 0; latest compatibility retest 3.68 seconds |
| DeepSeek 0.1.7-alpha.1 + Qwen GGUF | Local server loaded successfully; harness failed with `NO_ADAPTER: no adapter registered for provider "runanywhere"` |
| Other harnesses | Hermetic child-process/HTTP tests only, not real installed-harness model proof |
| Windows/mobile/Web | Platform CI still needs final-head review; no local runtime proof |

The OpenCode proof uses a focused read-only agent. Qwen3 0.6B sometimes invents tools with the full default tool menu; this is transport/tool-roundtrip proof, not a coding-quality benchmark. Only this model family is needed for the remaining checks.

Local runtime proof used an **experimental** 0.20.37 kit with `librac_server.a` rebuilt from the SDK PR, plus the updated SDK Swift source. It does not prove a fresh published kit. Existing release assets and the installed user binary were not replaced. The current Wally kit pin and actual CI Swift selection still point to the old released dependencies.

## Remaining work, in order

1. Inspect final-head SDK CI and resolve the two broad-suite timeouts. Re-run them alone with the normal timeout and compare with main before attributing a regression. Check Windows standalone `find_package(RunAnywhere)` + `RunAnywhere::server`; the final SDK commit fixes the missing `Threads::Threads` discovery, but only a simulated Windows CMake configure was tested locally.
2. Finish/review CodeRabbit on both PRs and address all new comments. A full review is being requested again after the final handoff push. Earlier Wally review was canceled when its head changed; no submitted review or actionable bot findings existed at the handoff snapshot. SDK review was pending. Cubic declined because its trial limit was reached. None of that is approval.
3. Build all four public desktop kits from the final SDK commit: macOS arm64, Linux x64, Windows x64, Windows arm64. Workflow: `.github/workflows/cpp-desktop-kit.yml`. Run `35697023983` targets `47b3baf2d9`; older run `35696736271` targets `13fa2a1c83` and predates the final Windows CMake packaging fix. Verify the run's head SHA before using its artifacts.
4. Publish those verified artifacts and their SHA-256 files under a new immutable prerelease tag, e.g. `cpp-desktop-local-harness-<commit>`, with `latest=false`. Do not overwrite existing releases. `core/VERSION` is currently `0.20.36`, so the new tag may carry filenames ending in `v0.20.36`; do not relabel them `0.20.37`.
5. Update Wally `versions.toml`: release tag, actual kit version, all four checksums, and IDL version/schema hash/protoc copied from the new kit. Update `sdk_ci_ref` and the SDK checkout refs in both `.github/workflows/ci.yml` and `release.yml` to the same immutable SDK commit/tag containing the MLX fix. Updating only the C++ kit leaves MLX broken. The separate published Swift fallback pin does not export `RunAnywhereMLXRuntime`; Apple shipping builds require the SDK source checkout.
6. Fetch the newly pinned kit, rebuild Wally, re-run tests and the live Qwen checks below. Confirm Windows CI and installed-symlink MLX smoke. Then remove the draft status.
7. Diagnose DeepSeek's `NO_ADAPTER` against a supported `dsh` version and the provider/patch schema in `src/harness/agents.cpp`. The local server startup passed; external harness compatibility did not. Latest rc packages had an unavailable dependency, so the live test used `0.1.7-alpha.1`.
8. Small SDK compatibility follow-up: `engines/llamacpp/rac_llamacpp_logging.cpp` now forwards CONT fragments using the previous severity in the default callback, whereas main discarded them. Review whether to preserve original default filtering while keeping continuation resolution for custom callbacks. This was identified but not changed before handoff.

## Commands to test

SDK, from its checkout (choose the corresponding Linux preset on Linux):

```bash
cmake --preset macos-debug -DRAC_BUILD_SERVER=ON
cmake --build --preset macos-debug
ctest --preset macos-debug --output-on-failure
ctest --test-dir build/macos-debug -R 'openai_server|llm_thinking|tool_calling' --output-on-failure
swiftc bindings/swift/Sources/MLXRuntime/MLXTextStopFilter.swift bindings/swift/Tests/MLXRuntimeStandalone/MLXTextStopFilterTests.swift -o /tmp/mlx-stop-tests
/tmp/mlx-stop-tests
```

The actual MLX dependency regression is documented by `python3 bindings/swift/scripts/test-mlx-cancellation.py --help`; point it at the Xcode build's Products/Release and SourcePackages/checkouts directories. It does not download a model.

Wally, **after the new kit and source pins are in place**:

```bash
export WALLY_SDK_SWIFT_PATH=/absolute/path/to/runanywhere-sdks
export WALLY_SDK_KIT="$PWD/.deps/local-harness-kit"
bash scripts/build/fetch-kit.sh macos-arm64 "$WALLY_SDK_KIT"
cmake -B build -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_PREFIX_PATH="$WALLY_SDK_KIT" -DWALLY_APPLE_MLX_HOST=OFF
cmake --build build
ctest --test-dir build --output-on-failure
python3 tests/test_versions.py
python3 scripts/ci/check-versions.py
bash scripts/build/build-mlx.sh build
bash scripts/test/e2e.sh build/wally
```

For the real OpenCode check, install OpenCode 1.18.32 and put its binary on PATH. Use a scratch directory with `probe.txt` containing `ORCHID_731` and this `opencode.json`:

```json
{"agent":{"probe":{"description":"Local transport probe","mode":"primary","prompt":"Use the read tool to read the requested file. /no_think","tools":{"*":false,"read":true},"permission":{"*":"deny","read":"allow"}}}}
```

From that directory, set `WALLY_BIN` to the rebuilt absolute path and `MODEL_HOME` to the model storage directory. Both formats of Qwen3 0.6B must already be downloaded. Run one at a time:

```bash
export PWD="$PWD"
"$WALLY_BIN" --home "$MODEL_HOME" opencode -m qwen3-0.6b -- run --dir "$PWD" --title 'Local probe' --agent probe --format json 'Read probe.txt with the read tool and return its contents. /no_think'
"$WALLY_BIN" --home "$MODEL_HOME" opencode -m mlx-qwen3-0.6b -- run --dir "$PWD" --title 'Local probe' --agent probe --format json 'Read probe.txt with the read tool and return its contents. /no_think'
```

Require a completed `read` tool event, a subsequent model answer containing `ORCHID_731`, final stop, exit 0, and no leftover local server. A plain text response without a tool event is not a pass. Pass both `--dir` and the correct `PWD`; OpenCode otherwise selected the wrong directory during initial testing.

## Evidence on Sanchit's machine

Worktrees live under `/Users/sanchitmonga/development/ODLM/MONOREPOOO/ra_cloud_work/.codex-worktrees/`: `wally-local-harness` and `sdk-local-harness-server`. Original shared checkouts were preserved.

- Full SDK build: `/tmp/sdk-independent-full-build.log`; broad tests: `/tmp/sdk-full-final-ctest.log`.
- Wally tests: `/tmp/wally-live-final-ctest.log`; product smoke: `/tmp/wally-live-product-e2e.log`.
- Live logs: `/tmp/wally-local-evidence/opencode-gguf-read/`, `opencode-mlx-compat-read/`, and `deepseek-gguf-read/`.
- Existing reproduction helper: `/tmp/run-wally-harness-probe.py`; model home: `/Users/sanchitmonga/.local/share/runanywhere`.
- Complete review snapshots: `/tmp/wally-local-review/comments/`; refresh via `/tmp/wally-local-review/refresh_comments.py`.

The `/tmp` files are local evidence only. The commands and remaining-work list above are the portable handoff.
