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
BUILD="${1:?usage: package-wally.sh <build-dir> <platform-tag>}"
PLATFORM="${2:?usage: package-wally.sh <build-dir> <platform-tag>}"
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
TARBALL="${DIST}/wally-${VERSION}-${PLATFORM}.tar.gz"

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
      # A kit whose llama.cpp is built with OpenMP makes wally need the GCC
      # runtime's libgomp.so.1, which a default Ubuntu or Debian install does
      # not have (it comes with the compiler): v0.6.0 failed on a clean 22.04
      # with `libgomp.so.1: cannot open shared object file`. Ship the build
      # machine's copy beside the kit's libraries, under the same $ORIGIN
      # runpath, together with its licence (GPLv3 with the GCC Runtime Library
      # Exception, which permits this). Keyed on what the binary actually needs,
      # so a kit built without OpenMP ships no libgomp at all.
      if readelf -d "${BUILD}/wally" | grep -q 'NEEDED.*\[libgomp\.so\.1\]'; then
        gomp="$(ldd "${BUILD}/wally" | awk '$1 == "libgomp.so.1" { print $3 }')"
        [[ -f "${gomp}" ]] || {
          echo "error: wally needs libgomp.so.1 and this build machine has none to bundle" >&2
          exit 1
        }
        copy_kit_runtime "${gomp}"
        gomp_licence=/usr/share/doc/libgomp1/copyright
        # Debian/Ubuntu ship the GCC Runtime Library Exception at the dpkg path
        # above; Arch/Manjaro ship the same licence under /usr/share/licenses.
        [[ -f "${gomp_licence}" ]] || gomp_licence=/usr/share/licenses/libgomp/RUNTIME.LIBRARY.EXCEPTION
        [[ -f "${gomp_licence}" ]] || {
          echo "error: bundling libgomp.so.1 needs its licence at /usr/share/doc/libgomp1/copyright" >&2
          exit 1
        }
        mkdir -p "${STAGE}/licenses"
        cp "${gomp_licence}" "${STAGE}/licenses/libgomp1.copyright"
      fi
      # The aarch64 kit is built against zstd + brotli (its cpp-httplib server)
      # and bz2 (libarchive); an x86_64 kit references none, so this bundles
      # only where they are real. Keyed on ldd, not readelf, so libbrotlicommon
      # -- pulled in behind libbrotli{enc,dec} rather than by wally directly --
      # is shipped too. All three licences are permissive and travel with the
      # library under the same $ORIGIN runpath as libgomp above.
      declare -A comp_licence=(
        [libzstd.so.1]=libzstd1
        [libbz2.so.1.0]=libbz2-1.0
        [libbrotlienc.so.1]=libbrotli1
        [libbrotlidec.so.1]=libbrotli1
        [libbrotlicommon.so.1]=libbrotli1
      )
      for soname in "${!comp_licence[@]}"; do
        so_path="$(ldd "${BUILD}/wally" | awk -v s="${soname}" '$1 == s { print $3 }')"
        [[ -n "${so_path}" && -f "${so_path}" ]] || continue
        copy_kit_runtime "${so_path}"
        pkg="${comp_licence[${soname}]}"
        lic="/usr/share/doc/${pkg}/copyright"
        [[ -f "${lic}" ]] || {
          echo "error: bundling ${soname} needs its licence at ${lic}" >&2
          exit 1
        }
        mkdir -p "${STAGE}/licenses"
        cp "${lic}" "${STAGE}/licenses/${pkg}.copyright"
      done
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

# The two checks below run the freshly built binary. On a build host whose glibc
# or libstdc++ is older than the bottle's floor they cannot: the arm64 bottle is
# built on ubuntu-22.04-arm (glibc 2.34), but its Sherpa prebuilt needs glibc
# 2.38, so the loader refuses to start a binary that is correct for its target.
# That is not a packaging failure -- check-linux-abi.py enforces the floor from
# the ELF files and the clean-distro install matrix runs the binary on a real
# target -- so a symbol-version loader error skips both checks with a note. Any
# other startup failure (a missing bundled library, a crash) still stops here.
skip_runtime_checks=0
# Seeded to 0: the `|| smoke_rc=$?` only runs on failure, so without this an
# inherited nonzero smoke_rc would send a successful smoke down the failure path.
smoke_rc=0
smoke_out="$("${STAGE}/bin/wally" version 2>&1)" || smoke_rc=$?
if [ "${smoke_rc}" -ne 0 ]; then
    if printf '%s' "${smoke_out}" | grep -qE "version .(GLIBC|GLIBCXX|CXXABI)_[0-9]"; then
        echo "note: this build host cannot start the bottle (its floor is newer than" >&2
        echo "      the host glibc/libstdc++); skipping the run-time smoke. check-linux-abi.py" >&2
        echo "      and the install matrix cover the target." >&2
        skip_runtime_checks=1
    else
        echo "error: wally failed to start during packaging:" >&2
        printf '%s\n' "${smoke_out}" >&2
        exit 1
    fi
fi

# A binary that resolves no console cannot sign anyone in. Configure requires
# the endpoints (CMakeLists.txt); CI discards them for fork pull requests, so
# this is only reachable for a bottle built that way. Asked in an empty profile
# with the runtime overrides cleared, so a signed-in account or a stray
# WALLY_CONSOLE_URL on the build machine cannot answer for the build.
if [ "${skip_runtime_checks}" -ne 1 ]; then
    probe_profile="$(mktemp -d)"
    about="$(env -u WALLY_CONSOLE_URL -u WALLY_CONSOLE_WEB_URL -u RCLI_CONSOLE_URL \
        -u RCLI_CONSOLE_WEB_URL WALLY_PROFILE_DIR="${probe_profile}" \
        "${STAGE}/bin/wally" about --json)"
    rm -rf "${probe_profile}"
    built_console="$(printf '%s' "$about" | sed -nE 's/.*"console":"([^"]*)".*/\1/p')"
    if [ -z "${built_console}" ]; then
        echo "error: this bottle resolves no console; it was configured without" >&2
        echo "       WALLY_BAKED_CONSOLE_API_URL / WALLY_BAKED_CONSOLE_WEB_ORIGIN." >&2
        exit 1
    fi
fi

mkdir -p "${DIST}"
rm -f "${TARBALL}" "${TARBALL}.sha256"
# macOS tar otherwise serializes Finder metadata as `._*` AppleDouble roots,
# breaking the single-root archive contract and surprising non-macOS clients.
COPYFILE_DISABLE=1 tar -czf "${TARBALL}" -C "${STAGE_ROOT}" "wally-${PLATFORM}"
(cd "${DIST}" && shasum -a 256 "$(basename "${TARBALL}")" > "$(basename "${TARBALL}").sha256")
echo "Packaged ${TARBALL}"
# sed, not `head`: it reads the whole stream, so `tar` never takes SIGPIPE on a
# bottle with more than 20 entries (the bundled arm64 one has 22), which under
# the caller's `set -o pipefail` would fail packaging on a cosmetic listing.
tar -tzf "${TARBALL}" | sed -n '1,20p'
