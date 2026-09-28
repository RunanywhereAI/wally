# Contributing to wally

`wally` is the desktop CLI for RunAnywhere, written in Rust (`Cargo.toml`).
Inference, catalog, download, and lifecycle live in the **C++ desktop kit**
(`find_package(RunAnywhere)`); CMake stays the build entry, resolves that kit,
and runs `cargo` for the application code. This repo is argv, terminal I/O,
and the Apple MLX host.

Do not point CMake at SDK source. Do not FetchContent or `add_subdirectory` the
SDK. If a command needs a sequence the public `rac_*` ABI does not offer, the
fix belongs in the SDK, then a new kit — not a workaround here.

## Prerequisites

- CMake 3.27+, a C++20 compiler (still needed: CMake resolves the kit and
  links the Apple Swift host)
- Ninja: `cmake/WallyRust.cmake` rejects multi-config generators (Visual
  Studio, Xcode, Ninja Multi-Config) because it picks the Cargo profile from
  `CMAKE_BUILD_TYPE` at configure time, so the `Build` command below passes
  `-G Ninja`
- The Rust toolchain `rust-toolchain.toml` pins (`rustup` installs it
  automatically the first time you run `cargo`/`rustc` in this tree)
- A staged kit matching `cmake/sdk-pin.cmake` (`WALLY_PINNED_SDK_VERSION`)
- Xcode 26+ only if you are linking the shipping Apple binary (`scripts/build/build-mlx.sh`)

Build a kit from a runanywhere-sdks checkout:

```bash
cmake --preset cpp-desktop-macos-arm64
cmake --build --preset cpp-desktop-macos-arm64 --target package-cpp-desktop \
  -j "$(sysctl -n hw.logicalcpu)"
```

The prefix is `dist/cpp-desktop-macos-arm64` in that checkout.

## Build

```bash
cmake -B build -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_PREFIX_PATH=/path/to/cpp-desktop-macos-arm64
cmake --build build -j "$(sysctl -n hw.logicalcpu)"
./build/wally version
./build/wally backends
```

On Apple Silicon that `wally` is the Swift MLX host (llama.cpp, ONNX, Sherpa,
and MLX in one process, linking the crate's static library). `wally-cxx` is
only the plain `cargo`-built intermediate. Independent clones need
`WALLY_SDK_SWIFT_PATH` pointing at a runanywhere-sdks tree.

`WALLY_SDK_DIR` / `WALLY_SDK_KIT` is an alias for that **kit prefix**. Pointing it
at a source tree is a configure error. Skip the Swift build for a faster loop:
`-DWALLY_APPLE_MLX_HOST=OFF`.

## Tests

```bash
ctest --test-dir build --output-on-failure   # runs `cargo test` (incl. tests/cli_golden.rs)
bash scripts/test/e2e.sh ./build/wally
# Device / overlay: primitives, not engines
# bash scripts/test/e2e-modalities.sh ./build/wally
```

`python3 scripts/test/test-ledger.py` checks that every C++ test case named in
`tests/cpp_case_ledger.txt` exists as a same-named Rust `#[test] fn`.

## Layout

```
src/commands/     one file per command; parse → bootstrap() → one rac_* call
src/catalog/      thin helpers over commons catalog APIs
src/cli/          the CLI11-compatible command-line parser (src/cli/mod.rs)
src/repl/         line editor with history (rustyline on macOS/Linux, plain
                  stdin on Windows)
src/sys/          raw FFI bindings to the kit's C ABI (bindgen, committed)
include/          wally_host.h — the wally_run_main entry for the Swift host
cmake/sdk-pin.cmake   EXACT kit version (+ sha256 once kits are published)
swift/            Apple entry: register MLX, then wally_run_main
```

## Adding a command

1. Add `src/commands/cmd_yours.rs` and declare `pub mod cmd_yours;` in
   `src/commands/mod.rs` — nothing to add to `Cargo.toml` or `CMakeLists.txt`.
2. Register it from `src/app.rs`.
3. Keep the module thin. No engine names, path patterns, or multi-step
   orchestration.

## Pull requests

Branch off `main`. `cmake --build` must stay green. On Apple that includes the
MLX host (`build/wally`). Do not add a second CLI binary.
