#!/usr/bin/env bash
# package.sh <build-dir> <platform-tag> [channel]
#
# Stages a relocatable wally bottle from build-ondevice.sh output and archives
# it. Matches the OG layout (wally-legacy) so install.sh keeps working:
#
#   wally-<platform>/bin/wally           the Go on-device binary
#   wally-<platform>/bin/wally-mlx        macOS only: the Swift MLX host
#   wally-<platform>/bin/*.bundle         macOS only: MLX Metal shaders
#   wally-<platform>/lib/<shared libs>    kit runtime the binary dlopens
#   wally-<platform>/README.md
#
#   platform-tag: macos-arm64 | linux-x86_64
#   channel:      prod (default) | dev  -> only the -dev archive suffix differs
#   version:      $WALLY_VERSION, else version/version.go's Version
#   macOS sign:   $WALLY_CODESIGN_IDENTITY (default "-" ad-hoc);
#                 $WALLY_REQUIRE_DEVELOPER_ID=1 rejects ad-hoc signing.
set -euo pipefail

BUILD="${1:?usage: package.sh <build-dir> <platform-tag> [channel]}"
PLATFORM="${2:?usage: package.sh <build-dir> <platform-tag> [channel]}"
CHANNEL="${3:-prod}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "${BUILD}" = /* ]] || BUILD="${ROOT}/${BUILD}"

SUFFIX=""
[[ "${CHANNEL}" == dev ]] && SUFFIX="-dev"

VERSION="${WALLY_VERSION:-}"
if [[ -z "${VERSION}" ]]; then
  VERSION="$(sed -nE 's/.*Version[[:space:]]*=[[:space:]]*"([0-9]+\.[0-9]+\.[0-9]+).*"/\1/p' "${ROOT}/version/version.go" | head -1)"
fi
[[ -n "${VERSION}" ]] || { echo "package: cannot resolve version" >&2; exit 1; }

BIN="${BUILD}/wally"
[[ -x "${BIN}" ]] || { echo "package: no wally binary at ${BIN} (run build-ondevice.sh)" >&2; exit 1; }

KIT="${WALLY_SDK_KIT:-${CMAKE_PREFIX_PATH:-}}"
KIT="${KIT%%:*}"

DIST="${ROOT}/dist"
STAGE="${DIST}/stage/wally-${PLATFORM}"
TARBALL="${DIST}/wally-${VERSION}-${PLATFORM}${SUFFIX}.tar.gz"

rm -rf "${STAGE}"
mkdir -p "${STAGE}/bin" "${STAGE}/lib"
cp "${BIN}" "${STAGE}/bin/wally"
chmod +x "${STAGE}/bin/wally"
[[ -f "${ROOT}/README.md" ]] && cp "${ROOT}/README.md" "${STAGE}/README.md"

# macOS: the Swift MLX host and its Metal bundles ride beside the binary.
if [[ "$(uname -s)" == Darwin ]]; then
  [[ -x "${BUILD}/wally-mlx" ]] || { echo "package: macOS bottle needs ${BUILD}/wally-mlx (scripts/build-mlx.sh)" >&2; exit 1; }
  cp "${BUILD}/wally-mlx" "${STAGE}/bin/wally-mlx"
  chmod +x "${STAGE}/bin/wally-mlx"
  shopt -s nullglob
  for bundle in "${BUILD}"/*.bundle; do
    cp -R "${bundle}" "${STAGE}/bin/"
  done
  shopt -u nullglob
  if [[ ! -d "${STAGE}/bin/mlx-swift_Cmlx.bundle" ]]; then
    echo "package: macOS bottle needs mlx-swift_Cmlx.bundle (Metal shaders)" >&2
    exit 1
  fi
fi

# Kit runtime shared libraries the binary dlopens (onnxruntime, sherpa). Copied
# as real files (-L dereferences the soname symlink chain) so the archive holds
# no symlinks that could escape its root; the rpath below resolves them.
copy_kit_runtime() {
  local src="$1"
  [[ -n "${src}" && -e "${src}" ]] || return 0
  cp -RL "${src}" "${STAGE}/lib/"
}
if [[ -n "${KIT}" && -d "${KIT}/third_party" ]]; then
  case "${PLATFORM}" in
    macos-*)
      copy_kit_runtime "${KIT}/third_party/libonnxruntime.dylib" ;;
    linux-*)
      shopt -s nullglob
      for so in "${KIT}/third_party"/*.so*; do copy_kit_runtime "${so}"; done
      shopt -u nullglob ;;
  esac
fi

case "${PLATFORM}" in
  macos-*)
    if compgen -G "${STAGE}/lib/*.dylib" >/dev/null; then
      for b in wally wally-mlx; do
        [[ -e "${STAGE}/bin/${b}" ]] && install_name_tool -add_rpath "@loader_path/../lib" "${STAGE}/bin/${b}" 2>/dev/null || true
      done
      for l in "${STAGE}/lib/"*.dylib; do
        install_name_tool -id "@rpath/$(basename "${l}")" "${l}" 2>/dev/null || true
      done
    fi
    sign_identity="${WALLY_CODESIGN_IDENTITY:--}"
    if [[ "${WALLY_REQUIRE_DEVELOPER_ID:-0}" == 1 && "${sign_identity}" == - ]]; then
      echo "package: production packaging requires WALLY_CODESIGN_IDENTITY" >&2
      exit 1
    fi
    sign_args=(--force --sign "${sign_identity}")
    [[ "${sign_identity}" != - ]] && sign_args+=(--options runtime --timestamp)
    [[ -n "${WALLY_CODESIGN_KEYCHAIN:-}" ]] && sign_args+=(--keychain "${WALLY_CODESIGN_KEYCHAIN}")
    shopt -s nullglob
    for l in "${STAGE}/lib/"*.dylib; do codesign "${sign_args[@]}" "${l}"; done
    shopt -u nullglob
    [[ -e "${STAGE}/bin/wally-mlx" ]] && codesign "${sign_args[@]}" "${STAGE}/bin/wally-mlx"
    codesign "${sign_args[@]}" "${STAGE}/bin/wally"
    codesign --verify --strict "${STAGE}/bin/wally"
    ;;
  linux-*)
    if command -v patchelf >/dev/null; then
      patchelf --set-rpath "\$ORIGIN/../lib" "${STAGE}/bin/wally"
    fi
    ;;
esac

mkdir -p "${DIST}"
tar -C "${DIST}/stage" -czf "${TARBALL}" "wally-${PLATFORM}"
( cd "${DIST}" && shasum -a 256 "$(basename "${TARBALL}")" > "$(basename "${TARBALL}").sha256" )
echo "packaged ${TARBALL}" >&2
echo "$(cat "${TARBALL}.sha256")" >&2
