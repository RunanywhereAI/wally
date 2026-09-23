#!/usr/bin/env bash
# package-wally.sh <build-dir> <platform-tag>
#
# Relocatable wally bottle from a kit-linked CMake build. Does not compile the SDK.
#
#   platform-tag: macos-arm64 | linux-x86_64
#   version:      $WALLY_VERSION, else project(wally VERSION …)
#   macOS signing: $WALLY_CODESIGN_IDENTITY, optional $WALLY_CODESIGN_KEYCHAIN
#                  Set $WALLY_REQUIRE_DEVELOPER_ID=1 to reject ad-hoc signing.
#
# Layout:
#   wally-<platform>/bin/wally
#   wally-<platform>/lib/<onnxruntime shared lib>
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "${ROOT}/scripts/lib/common.sh"
BUILD="${1:?usage: package-wally.sh <build-dir> <platform-tag> [channel]}"
PLATFORM="${2:?usage: package-wally.sh <build-dir> <platform-tag> [channel]}"
# channel: empty/prod for the production bottle, "dev" for the dev-endpoint
# bottle. Only the archive filename changes (-dev); the staged tree and its
# single root stay wally-<platform> so install scripts extract both the same.
CHANNEL="${3:-}"
SUFFIX=""
[[ "${CHANNEL}" == dev ]] && SUFFIX="-dev"
[[ "${BUILD}" = /* ]] || BUILD="${ROOT}/${BUILD}"

VERSION="${WALLY_VERSION:-$(wally_version)}"
[[ -n "${VERSION}" ]] || { echo "error: cannot resolve WALLY version from versions.toml" >&2; exit 1; }

KIT="${WALLY_SDK_KIT:-${CMAKE_PREFIX_PATH:-}}"
KIT="${KIT%%:*}"

BIN=""
if [[ "$(uname -s)" == Darwin ]]; then
  # The macOS bottle is the Swift MLX host. Never silently ship wally-cxx.
  if [[ ! -x "${BUILD}/wally" ]]; then
    echo "error: macOS bottle requires ${BUILD}/wally (Swift MLX host)." >&2
    echo "  cmake --build with WALLY_APPLE_MLX_HOST=ON, or scripts/build/build-mlx.sh" >&2
    exit 1
  fi
  BIN="${BUILD}/wally"
else
  cands=("${BUILD}/wally-cxx" "${BUILD}/wally" "${BUILD}/wally.exe" "${BUILD}/Release/wally" "${BUILD}/Release/wally.exe")
  for cand in "${cands[@]}"; do
    if [[ -x "${cand}" ]]; then BIN="${cand}"; break; fi
  done
fi
[[ -n "${BIN}" ]] || { echo "error: wally binary not found under ${BUILD}" >&2; exit 1; }

DIST="${ROOT}/dist"
STAGE_ROOT="${DIST}/stage"
STAGE="${STAGE_ROOT}/wally-${PLATFORM}"
TARBALL="${DIST}/wally-${VERSION}-${PLATFORM}${SUFFIX}.tar.gz"

rm -rf "${STAGE}"
mkdir -p "${STAGE}/bin" "${STAGE}/lib"
cp "${BIN}" "${STAGE}/bin/wally"
chmod +x "${STAGE}/bin/wally"
[[ -f "${ROOT}/README.md" ]] && cp "${ROOT}/README.md" "${STAGE}/README.md"
shopt -s nullglob
for bundle in "${BUILD}"/*.bundle; do
  cp -R "${bundle}" "${STAGE}/bin/"
done
shopt -u nullglob
if [[ "$(uname -s)" == Darwin ]]; then
  if [[ ! -d "${STAGE}/bin/mlx-swift_Cmlx.bundle" ]]; then
    echo "error: macOS bottle requires mlx-swift_Cmlx.bundle next to wally (Metal shaders)." >&2
    echo "  cmake --build with WALLY_APPLE_MLX_HOST=ON, or scripts/build/build-mlx.sh" >&2
    exit 1
  fi
  if [[ ! -s "${BUILD}/mlx.metallib" ]]; then
    echo "error: macOS bottle requires mlx.metallib for launches through install symlinks." >&2
    echo "  rebuild with scripts/build/build-mlx.sh" >&2
    exit 1
  fi
  cp "${BUILD}/mlx.metallib" "${STAGE}/bin/mlx.metallib"
fi

copy_kit_runtime() {
  local src="$1"
  [[ -n "${src}" && -e "${src}" ]] || return 0
  # -L dereferences: the kit ships onnxruntime as a
  # libonnxruntime.so -> .so.1 -> .so.1.28.0 symlink chain, and the release
  # verifier rejects symlinks in a bottle (they can escape the archive root).
  # Copying the targets as real files keeps the soname the binary needs present
  # without a link. A dangling link is skipped by the -e guard above.
  cp -RL "${src}" "${STAGE}/lib/"
}

if [[ -n "${KIT}" && -d "${KIT}/third_party" ]]; then
  case "${PLATFORM}" in
    macos-*)
      copy_kit_runtime "${KIT}/third_party/libonnxruntime.dylib"
      ;;
    linux-*)
      # Every shared object the kit ships, not just onnxruntime: the binary
      # dynamically links sherpa too (libsherpa-onnx-c-api.so), and a bottle
      # missing any one of them fails to load with `cannot open shared object
      # file` the moment it runs -- which the `wally version` smoke below is
      # here to catch. patchelf's $ORIGIN/../lib rpath resolves them from here.
      shopt -s nullglob
      for so in "${KIT}/third_party"/*.so*; do
        copy_kit_runtime "${so}"
      done
      shopt -u nullglob
      ;;
  esac
fi

case "${PLATFORM}" in
  macos-*)
    if [[ -d "${STAGE}/lib" ]] && compgen -G "${STAGE}/lib/*.dylib" >/dev/null; then
      install_name_tool -add_rpath "@loader_path/../lib" "${STAGE}/bin/wally" 2>/dev/null || true
      for lib in "${STAGE}/lib/"*.dylib; do
        install_name_tool -id "@rpath/$(basename "${lib}")" "${lib}" 2>/dev/null || true
      done
    fi
    sign_identity="${WALLY_CODESIGN_IDENTITY:--}"
    if [[ "${WALLY_REQUIRE_DEVELOPER_ID:-0}" == 1 && "${sign_identity}" == - ]]; then
      echo "error: production packaging requires WALLY_CODESIGN_IDENTITY" >&2
      exit 1
    fi
    sign_args=(--force --sign "${sign_identity}")
    if [[ "${sign_identity}" != - ]]; then
      sign_args+=(--options runtime --timestamp)
    fi
    if [[ -n "${WALLY_CODESIGN_KEYCHAIN:-}" ]]; then
      sign_args+=(--keychain "${WALLY_CODESIGN_KEYCHAIN}")
    fi
    shopt -s nullglob
    for lib in "${STAGE}/lib/"*.dylib; do
      codesign "${sign_args[@]}" "${lib}"
      codesign --verify --strict "${lib}"
    done
    shopt -u nullglob
    codesign "${sign_args[@]}" "${STAGE}/bin/wally"
    codesign --verify --strict "${STAGE}/bin/wally"
    ;;
  linux-*)
    command -v patchelf >/dev/null 2>&1 || {
      echo "error: patchelf is required to package a relocatable Linux bottle" >&2
      exit 1
    }
    patchelf --set-rpath "\$ORIGIN/../lib" "${STAGE}/bin/wally"
    # DT_RUNPATH is not transitive: wally finding Sherpa does not help Sherpa
    # find ONNX Runtime beside it. Give every packaged shared object its own
    # sibling lookup so the archive runs without LD_LIBRARY_PATH.
    while IFS= read -r -d '' lib; do
      patchelf --set-rpath "\$ORIGIN" "${lib}"
    done < <(find "${STAGE}/lib" -type f -name '*.so*' -print0)
    find "${STAGE}/lib" -type f -name '*.so*' -exec chmod 0644 {} +
    ;;
esac

"${STAGE}/bin/wally" version >/dev/null

# The archive name says which flavour this is; the binary has to agree. A dev
# job whose endpoint variables were unset used to produce a `-dev` archive that
# defaults to production, and nothing anywhere noticed (wally #87).
#
# Asked in an empty profile with the runtime overrides cleared, so a signed-in
# account or a stray WALLY_CONSOLE_URL on the build machine cannot answer for
# the bake.
probe_profile="$(mktemp -d)"
about="$(env -u WALLY_CONSOLE_URL -u WALLY_CONSOLE_WEB_URL -u RCLI_CONSOLE_URL \
    -u RCLI_CONSOLE_WEB_URL WALLY_PROFILE_DIR="${probe_profile}" \
    "${STAGE}/bin/wally" about --json)"
rm -rf "${probe_profile}"
built_channel="$(printf '%s' "$about" | sed -nE 's/.*"channel":"([^"]*)".*/\1/p')"
case "${CHANNEL}" in
    dev)  want_channel="development" ;;
    *)    want_channel="production" ;;
esac
if [ "${built_channel}" != "${want_channel}" ]; then
    echo "error: packaging a '${CHANNEL:-prod}' archive from a '${built_channel}' binary." >&2
    echo "       Expected channel '${want_channel}'. Set WALLY_CHANNEL and the baked" >&2
    echo "       endpoint variables in the configure environment, or package the" >&2
    echo "       matching build." >&2
    exit 1
fi

mkdir -p "${DIST}"
rm -f "${TARBALL}" "${TARBALL}.sha256"
# macOS tar otherwise serializes Finder metadata as `._*` AppleDouble roots,
# breaking the single-root archive contract and surprising non-macOS clients.
COPYFILE_DISABLE=1 tar -czf "${TARBALL}" -C "${STAGE_ROOT}" "wally-${PLATFORM}"
(cd "${DIST}" && shasum -a 256 "$(basename "${TARBALL}")" > "$(basename "${TARBALL}").sha256")
echo "Packaged ${TARBALL}"
tar -tzf "${TARBALL}" | head -20
