#!/usr/bin/env bash
# Apple shipping binary: the Rust crate's static library + Swift MLX host → build/wally.
#
#   scripts/build/build-mlx.sh [build-dir]
#
# Requires:
#   - cmake already built the wally target (the crate's libwally.a and
#     build/wally-native-link-args.txt, the kit link line build.rs resolved)
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

RUST_LIB="${WALLY_RUST_STATICLIB:-${BUILD}/cargo/release/libwally.a}"
LINK_ARGS="${BUILD}/wally-native-link-args.txt"
[[ -f "${RUST_LIB}" ]] || { echo "error: ${RUST_LIB} not found - build the wally target first" >&2; exit 1; }
[[ -s "${LINK_ARGS}" ]] || { echo "error: ${LINK_ARGS} not found - build the wally target first" >&2; exit 1; }

# swiftc (Xcode 27) rejects raw `-Wl,` options and ignores bare archive paths in
# OTHER_LDFLAGS, so everything aimed at ld goes through -Xlinker; -l/-L/-F and
# -framework are swiftc options and pass as they are.
flags=()
pending_framework=0
while IFS= read -r arg; do
    [[ -n "${arg}" ]] || continue
    if [[ "${pending_framework}" -eq 1 ]]; then
        flags+=("-framework" "${arg}")
        pending_framework=0
        continue
    fi
    case "${arg}" in
        -framework) pending_framework=1 ;;
        -l*|-L*|-F*) flags+=("${arg}") ;;
        -Wl,*)
            IFS=',' read -r -a parts <<< "${arg#-Wl,}"
            for part in "${parts[@]}"; do flags+=("-Xlinker" "${part}"); done
            ;;
        *) flags+=("-Xlinker" "${arg}") ;;
    esac
done < "${LINK_ARGS}"
# The static library's own native dependencies (Rust std, native-tls's
# Security.framework), then the C++ runtime the kit needs.
rust_native=(-framework CoreFoundation -framework Security -liconv)

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
# The kit's plugin backends arrive force-loaded in the link args (static
# registrars); the Rust archive is a regular archive.
# Comments must not sit in a `\` continuation — they cut the command in half.
set +e
RUNANYWHERE_BUILD_MLX_DISTRIBUTION_FRAMEWORK=1 \
    xcodebuild build \
    -scheme wally-mlx \
    -destination "platform=macOS,arch=$(uname -m)" \
    -configuration Release \
    -derivedDataPath .build/xcode \
    HEADER_SEARCH_PATHS="\$(inherited) ${KIT}/include ${ROOT}/include" \
    OTHER_LDFLAGS="-L$(dirname "${RUST_LIB}") -lwally ${flags[*]} ${rust_native[*]} -lc++" \
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
echo "built ${BUILD}/wally"
