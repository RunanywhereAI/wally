#!/usr/bin/env bash
# Builds the wally-mlx helper (swift/) and places it beside build/wally, along
# with the MLX Metal shader bundle it needs at runtime. Called by
# scripts/build-ondevice.sh after the Go build; can also run standalone.
#
# rac_server (the packaged C++ kit's HTTP server, what the Go ondevice engine
# links via cgo) is GGUF/llama.cpp-only regardless of which backends are
# registered -- confirmed against wally-legacy/.deps/kit (librac_server.a has
# zero MLX/safetensors awareness) and against wally-legacy's own
# harness.cpp:Resolve(), which refuses any non-LlamaCpp local model for
# exactly that reason. wally-mlx is therefore a standalone Swift process, not
# a second cgo path: it links the published runanywhere-swift SDK directly
# (RunAnywhere + RunAnywhereMLX, both resolving to the same RACommonsBinary
# xcframework, so there is exactly one commons instance in that process) and
# serves ONE MLX model over a hand-rolled OpenAI-compatible HTTP server built
# on the registry-aware lifecycle/generate proto ABI
# (rac_model_registry_discover_proto -> rac_model_lifecycle_load_proto ->
# rac_llm_generate_proto), since MLX only ever runs through that path.
#
# `swift build` cannot compile mlx-swift's Metal shaders (no metallib is
# produced, and the binary fails at runtime with "Failed to load the default
# metallib" -- confirmed live). xcodebuild can, so this script builds through
# the xcodebuild-generated scheme for the swift/ package instead of a plain
# `swift build`, matching wally-legacy's own scripts/build/build-mlx.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="${1:-${ROOT}/build}"
mkdir -p "${BUILD}"
BUILD="$(cd "${BUILD}" && pwd)"

if ! command -v xcodebuild >/dev/null 2>&1; then
  echo "build-mlx: xcodebuild not found (needs a full Xcode install, not just CLT -- swift build alone cannot compile MLX's Metal shaders)" >&2
  exit 1
fi

cd "${ROOT}/swift"

DERIVED="${BUILD}/.wally-mlx-xcode"
xcode_log="${BUILD}/xcodebuild-mlx.log"
set +e
xcodebuild build \
  -scheme wally-mlx \
  -destination "platform=macOS,arch=$(uname -m)" \
  -configuration Release \
  -derivedDataPath "${DERIVED}" \
  >"${xcode_log}" 2>&1
status=$?
set -e
if [[ "${status}" -ne 0 ]]; then
  echo "error: xcodebuild failed with status ${status}" >&2
  grep -E "error: |Undefined symbols|library not found|clang: error|ld: error" "${xcode_log}" >&2 || true
  echo "----- tail of ${xcode_log} -----" >&2
  tail -80 "${xcode_log}" >&2
  exit "${status}"
fi

PRODUCTS="${DERIVED}/Build/Products/Release"
[[ -x "${PRODUCTS}/wally-mlx" ]] || { echo "build-mlx: xcodebuild produced no wally-mlx binary" >&2; exit 1; }

cp "${PRODUCTS}/wally-mlx" "${BUILD}/wally-mlx"

# Metal shader bundles must sit next to the executable (mlx-swift's
# ra_mlx_metal_resource_anchor / default resource-bundle lookup walks the
# directory the real binary lives in). Copy every .bundle xcodebuild laid
# down, not just mlx-swift's -- matches build-ondevice.sh's own "copy
# everything the linked SDK needs beside the binary" posture.
shopt -s nullglob
for bundle in "${PRODUCTS}"/*.bundle; do
  dest="${BUILD}/$(basename "${bundle}")"
  rm -rf "${dest}"
  cp -R "${bundle}" "${dest}"
done
shopt -u nullglob

echo "built ${BUILD}/wally-mlx" >&2
