---
name: wally-kit-pin
description: Bump cmake/sdk-pin.cmake to a new published SDK C++ desktop kit (version + SHA-256 + IDL lock). Use after an SDK GitHub Release is published (not draft), when fetch-kit.sh 404s, or when SCHEMA_LOCK mismatches.
---

# Wally kit pin

Pin file: `cmake/sdk-pin.cmake`. Fetcher: `scripts/build/fetch-kit.sh`. Overlay:
`scripts/build/fetch-private-pack.sh`. CMake wrapper: `cmake/RunAnywhereSDK.cmake`.

Companion: **cpp-desktop-kit** in runanywhere-sdks (what the tarball contains).

## Preconditions

1. SDK GitHub Release `v$SDK` is **published** (`isDraft=false`). Drafts 404
   for this repo's `GITHUB_TOKEN`. Prerelease is fine; `latest` is not required.
2. Public assets exist:
   `RunAnywhere-cpp-desktop-{macos-arm64,windows-x64}-v$SDK.tar.gz`
   (+ windows-arm64 once that kit is on the release).
3. Do **not** point `WALLY_SDK_DIR` at SDK **source** — configure error.
   Prefix is `-DCMAKE_PREFIX_PATH=` or `-DWALLY_SDK_KIT=`.

## Bump together (never version-only)

```bash
# After SDK v$SDK is published:
gh release download "v$SDK" --repo RunanywhereAI/runanywhere-sdks \
  --pattern 'RunAnywhere-cpp-desktop-*-v'"$SDK"'.tar.gz*' -D /tmp/kits
shasum -a 256 /tmp/kits/RunAnywhere-cpp-desktop-*.tar.gz
```

Update in `cmake/sdk-pin.cmake`:

- `WALLY_PINNED_SDK_VERSION`
- `WALLY_PINNED_KIT_SHA256_MACOS_ARM64`
- `WALLY_PINNED_KIT_SHA256_WINDOWS_X64`
- `WALLY_PINNED_KIT_SHA256_WINDOWS_ARM64` (uncomment once the asset exists)
- IDL triple copied from the kit's `share/runanywhere/SCHEMA_LOCK`:
  `WALLY_PINNED_IDL_VERSION`, `WALLY_PINNED_IDL_SCHEMA_SHA256`,
  `WALLY_PINNED_IDL_PROTOC_VERSION`

`fetch-kit.sh` refuses `SDK_VERSION != WALLY_PINNED_SDK_VERSION`. Do not override
only an env var to "try" a newer kit — checksums are keyed to the pin.

Configure fails if the extracted kit's `SCHEMA_LOCK` does not match the IDL
pin. When the schema changes, consume a new kit and bump the pin — never
hand-write proto types locally. Wally never runs `protoc`: `crate::io::proto::v1::*`
is generated at build time by prost/protox straight from the new kit's `.proto`
files (`build.rs`), and nothing generated is committed.

`src/sys/bindings.rs` (the `rac_*` C ABI declarations) **is** committed and
derived from the kit's C headers by bindgen — regenerate it after a kit pin
bump with `scripts/build/gen-sys-bindings.sh` (`--check` verifies it is
current; CI runs that on macOS).

## CI Swift tree

`.github/workflows/ci.yml` and `release.yml` check out
`RunanywhereAI/runanywhere-sdks` at `ref: v$SDK` into `.deps/runanywhere-sdks`
for the Apple MLX host (`WALLY_SDK_SWIFT_PATH`). Bump that ref in the **same
commit** as the pin. macos-26 (Xcode 26 / Swift 6.2 — matches SDK
`swift-tools-version: 6.2`); macos-15 is Swift 6.1; macos-14 is Swift 5.10.

## Private overlays

Not part of the pin. Applied after extract:

1. `WALLY_PRIVATE_OVERLAY` (local tarball)
2. tarball next to the kit prefix named
   `RunAnywhere-cpp-desktop-{macos-arm64-neurt,windows-arm64-qhexrt}-private-v*.tar.gz`
3. skip (OSS) unless `WALLY_REQUIRE_PRIVATE=1`

QHexRT is Windows ARM64 only. Public bottles must stay OSS — never bake overlay
bytes into a Homebrew bottle or a public GitHub Release asset.

Windows ARM64 **public** kit is commons-only (no llama.cpp). Overlay apply
then rebuild product `wally.exe`. `v0.20.28` ARM64 kits are missing
`lib/libcurl.lib` (packager only globbed x64-windows-static) — copy from
vcpkg `arm64-windows-static` until the next SDK kit train. Wally
`cmake/RunAnywhereSDK.cmake` links that import lib when present.

NeuRT: public mac bottle has MLX, not NeuRT. Overlay tarball +
`WALLY_APPLE_MLX_HOST=ON` rebuild. See **wally-e2e** for QAIRT / SD15 / DSP
gotchas; do not weaken public CI to require overlays.

## Verify

```bash
bash scripts/build/fetch-kit.sh macos-arm64 /tmp/kit
cmake -B build -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_PREFIX_PATH=/tmp/kit
cmake --build build -j "$(sysctl -n hw.logicalcpu)"
WALLY_SDK_KIT=/tmp/kit bash scripts/test/e2e.sh ./build/wally
```
