#!/usr/bin/env bash
# Apple shipping binary: CMake `wally-cxx` objects + Swift MLX host → build/wally.
#
#   scripts/build/build-mlx.sh [build-dir]
#
# Requires:
#   - cmake already built the wally target (wally-cxx + link.txt)
#   - a kit prefix on CMAKE_PREFIX_PATH / WALLY_SDK_KIT (public headers)
#   - Xcode (xcodebuild compiles MLX Metal shaders; `swift build` cannot)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUILD="${1:-${ROOT}/build}"
# Absolute, because this script `cd`s to ${ROOT}/swift before it looks inside
# ${BUILD} again. A relative build dir silently resolved against swift/ instead,
# so libwally_plugins.a was "not found" even when it existed, and the empty
# plugin array then tripped `set -u` on macOS bash 3.2 with the useless message
# `plugin_ldflags[*]: unbound variable`.
BUILD="$(cd "${BUILD}" 2>/dev/null && pwd)" || {
    echo "error: build dir '${1:-${ROOT}/build}' does not exist - run cmake first" >&2
    exit 1
}

KIT="${WALLY_SDK_KIT:-${CMAKE_PREFIX_PATH:-}}"
KIT="${KIT%%:*}"
if [[ -z "${KIT}" || ! -d "${KIT}/include" ]]; then
    echo "error: set WALLY_SDK_KIT to a staged C++ desktop kit prefix (include/rac)" >&2
    exit 1
fi

"${ROOT}/scripts/build/bundle-core.sh" "${BUILD}"

flags=()
while IFS= read -r entry; do
    [[ -n "${entry}" ]] || continue
    flags+=("${entry}")
done < "${BUILD}/wally-link-flags.txt"

# The published runanywhere-swift tarball does not export RunAnywhereMLXRuntime
# (Swift MLX without a second commons archive). Apple wally therefore needs the
# SDK source tree: nested monorepo, or WALLY_SDK_SWIFT_PATH in CI.
if [[ -z "${WALLY_SDK_SWIFT_PATH:-}" && -f "${ROOT}/../../Package.swift" ]]; then
    # Canonicalize: SwiftPM's local package identity is the last path
    # component, so a trailing `/../..` would register the package as `..`.
    export WALLY_SDK_SWIFT_PATH="$(cd "${ROOT}/../.." && pwd)"
fi
if [[ -z "${WALLY_SDK_SWIFT_PATH:-}" || ! -f "${WALLY_SDK_SWIFT_PATH}/Package.swift" ]]; then
    echo "error: Apple wally needs the SDK Swift tree (RunAnywhereMLXRuntime)." >&2
    echo "  export WALLY_SDK_SWIFT_PATH=/path/to/runanywhere-sdks" >&2
    echo "  or build from EXTERNAL/WALLY inside that monorepo." >&2
    exit 1
fi

cd "${ROOT}/swift"
xcode_log="${BUILD}/xcodebuild-mlx.log"
# Bare .a paths are ignored by SwiftPM's swiftc; -Wl,-force_load is not.
# Only plugin backends are force-loaded (static registrars). The rest of the
# C++ objects, including llama-common, are a regular archive so download.cpp.o
# is not pulled (it references cpp-httplib methods the kit never emitted).
# Comments must not sit in a `\` continuation — they cut the command in half.
plugin_ldflags=()
if [[ -f "${BUILD}/libwally_plugins.a" ]]; then
    plugin_ldflags+=("-Wl,-force_load,${BUILD}/libwally_plugins.a")
fi
set +e
RUNANYWHERE_BUILD_MLX_DISTRIBUTION_FRAMEWORK=1 \
    xcodebuild build \
    -scheme wally-mlx \
    -destination "platform=macOS,arch=$(uname -m)" \
    -configuration Release \
    -derivedDataPath .build/xcode \
    HEADER_SEARCH_PATHS="\$(inherited) ${KIT}/include ${ROOT}/include" \
    OTHER_LDFLAGS="${plugin_ldflags[*]:-} -L${BUILD} -lwally_bundle -lc++ ${flags[*]}" \
    >"${xcode_log}" 2>&1
xcodebuild_status=$?
set -e
if [[ "${xcodebuild_status}" -ne 0 ]]; then
    echo "error: xcodebuild failed with status ${xcodebuild_status}" >&2
    # Do not match `-Werror=` on every CompileC line.
    grep -E "error: |Undefined symbols|library not found|clang: error|ld: error" "${xcode_log}" >&2 || true
    echo "----- tail of ${xcode_log} -----" >&2
    tail -80 "${xcode_log}" >&2
    exit "${xcodebuild_status}"
fi
grep -E "error: |warning: .*[Mm]etal|BUILD SUCCEEDED" "${xcode_log}" || true

PRODUCTS="${ROOT}/swift/.build/xcode/Build/Products/Release"
[[ -x "${PRODUCTS}/WallyMLX" ]] || { echo "the MLX build produced no binary" >&2; exit 1; }

cp "${PRODUCTS}/WallyMLX" "${BUILD}/wally"
# Metal shader bundles must sit next to the executable. Copy every .bundle
# xcodebuild laid down (mlx-swift_Cmlx.bundle, mlx-swift_Cmlx.bundle, …).
shopt -s nullglob
for bundle in "${PRODUCTS}"/*.bundle; do
    dest="${BUILD}/$(basename "${bundle}")"
    rm -rf "${dest}"
    cp -R "${bundle}" "${dest}"
done
shopt -u nullglob
# MLX's SwiftPM bundle lookup uses NSBundle.mainBundle, which follows the
# public launch symlink rather than the installed executable. Its supported
# colocated mlx.metallib lookup uses dladdr and resolves the physical binary.
# Keep the bundle for SwiftPM and stage the same shaders for installed launches.
metallib="${BUILD}/mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib"
[[ -s "${metallib}" ]] || {
    echo "error: MLX build did not produce default.metallib" >&2
    exit 1
}
cp "${metallib}" "${BUILD}/mlx.metallib"
echo "built ${BUILD}/wally"
