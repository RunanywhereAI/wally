#!/usr/bin/env bash
# Builds wally with the on-device engine linked: rac_server (GGUF/llama.cpp,
# via the packaged C++ kit) wired in behind `-tags ondevice`. A plain
# scripts/build.sh never touches the kit; only this script does.
#
# Version and console channel are parameterized, matching scripts/build.sh:
#   -tag <ver>                        stamps version/version.go's Version.
#   WALLY_BAKED_CONSOLE_API_URL /     baked via -ldflags -X (empty = prod
#   WALLY_BAKED_CONSOLE_WEB_ORIGIN    defaults from config.go). scripts/dev.sh
#                                     sets the dev endpoints.
# On macOS this also builds the wally-mlx Swift helper (scripts/build-mlx.sh),
# unless WALLY_SKIP_MLX=1, so one command produces the whole on-device bottle.
#
# The link recipe is not hand-copied. It is extracted from the kit's own
# lib/cmake/RunAnywhere/RunAnywhereConfig.cmake, which lists the exact archive
# order CMake would pass to the linker (ggml/llama/absl depend on each other
# and a wrong order fails under a strict linker) plus the rac_backend_*
# archives that need whole-archive because nothing calls a symbol inside them
# directly (they register their engine via a static constructor). Re-run this
# after any kit bump instead of touching a frozen list here.
set -euo pipefail

tag=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -tag) tag="${2:?-tag needs a value}"; shift 2 ;;
    -h|--help) echo "usage: $(basename "$0") [-tag <version>]" >&2; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

os="$(go env GOOS)"
kit="${WALLY_SDK_KIT:-/Users/home/Project/wally-work-sapce/wally-legacy/.deps/kit}"
cmake_config="$kit/lib/cmake/RunAnywhere/RunAnywhereConfig.cmake"
lib="$kit/lib"

if [[ ! -f "$cmake_config" ]]; then
  echo "build-ondevice: no kit at $kit (set WALLY_SDK_KIT)" >&2
  exit 1
fi

# The kit's own explicit link-order list: every
# ${RunAnywhere_LIBRARY_DIR}/lib*.a token in the config, in the order CMake
# would emit them. Filtered to archives that actually exist on this platform
# (Linux ships onnxruntime as a .so, not a .a) and, off Apple, stripped of the
# ONNX/Sherpa/CoreML stack: the CLI serves only GGUF there, so those engines
# and their heavy deps (onnxruntime, sherpa, kaldi, piper, ...) are dead weight
# that only drags in link dependencies the GGUF path never needs.
non_gguf='onnx|sherpa|kaldi|coreml|piper|espeak|ssentencepiece|kissfft|fst|fbank|ucd'
keep_archive() {
  [[ -e "$1" ]] || return 1
  if [[ "$os" != darwin ]]; then
    echo "$(basename "$1")" | grep -qiE "$non_gguf" && return 1
  fi
  return 0
}

extra_archives_raw="$(
  grep -o '\${RunAnywhere_LIBRARY_DIR}/[A-Za-z0-9_+.-]*\.a' "$cmake_config" \
    | sed "s#\${RunAnywhere_LIBRARY_DIR}#$lib#" \
    | awk '!seen[$0]++'
)"
extra_archives=""
while IFS= read -r a; do
  [[ -n "$a" ]] || continue
  keep_archive "$a" && extra_archives="$extra_archives $a"
done <<<"$extra_archives_raw"

ldflags="-L$lib $lib/librac_server.a $lib/librac_commons.a"
ldflags="$ldflags $extra_archives"

# rac_backend_* archives (llamacpp, mlx, onnx, sherpa in this kit) register via
# a ctor and get dropped by a normal static link unless whole-archived. The
# flag differs by linker: Apple ld64 uses -force_load per archive, GNU/LLD use
# --whole-archive/--no-whole-archive around the group. NeuRT / QHexRT are
# excluded exactly as the kit's own CMake does: private overlays linked apart.
backends=()
for f in "$lib"/librac_backend_*.a; do
  [[ -e "$f" ]] || continue
  case "$(basename "$f")" in
    *neurt*|*qhexrt*) continue ;;
  esac
  # Off Apple, only the GGUF (llamacpp) backend is served.
  if [[ "$os" != darwin ]]; then
    echo "$(basename "$f")" | grep -qiE "$non_gguf" && continue
  fi
  backends+=("$f")
done

case "$os" in
  darwin)
    for f in "${backends[@]}"; do ldflags="$ldflags -Wl,-force_load,$f"; done
    # System deps the kit's CMake pulls via find_dependency, plus the Apple
    # frameworks it links unconditionally on APPLE. -lc++ is not part of the
    # kit's recipe: CMake links C++ targets through the C++ driver, which adds
    # it implicitly; cgo's external linker invokes the C driver, so the C++
    # runtime has to be named explicitly or the link fails on __cxa_*/operator
    # new. The frameworks match RunAnywhereConfig.cmake's APPLE block.
    ldflags="$ldflags -lc++ -lcurl -lz -lbz2 -ldl -lpthread"
    ldflags="$ldflags -framework Foundation -framework Accelerate -framework Metal -framework MetalKit -framework CoreML"
    ldflags="$ldflags -framework CoreFoundation -framework CFNetwork"
    # librac_server.a (cpp-httplib) compiles in brotli/zstd/openssl and calls
    # their symbols directly (not through libcurl), plus CFHost (CFNetwork) for
    # its async getaddrinfo on Apple. RunAnywhereConfig.cmake does not declare
    # these as find_dependency; the kit's own consumer picks them up from the
    # ambient (Homebrew) toolchain, so this does too.
    for formula in brotli zstd openssl@3; do
      prefix="$(brew --prefix "$formula" 2>/dev/null || true)"
      if [[ -z "$prefix" ]]; then
        echo "build-ondevice: brew formula '$formula' not found (needed by rac_server's httplib)" >&2
        exit 1
      fi
      ldflags="$ldflags -L$prefix/lib"
    done
    ldflags="$ldflags -lbrotlidec -lbrotlienc -lbrotlicommon -lzstd -lssl -lcrypto"
    ;;
  linux)
    if [[ ${#backends[@]} -gt 0 ]]; then
      ldflags="$ldflags -Wl,--whole-archive ${backends[*]} -Wl,--no-whole-archive"
    fi
    # GNU driver: name the C++ runtime and the same system deps the APPLE path
    # gets from frameworks. brotli/zstd/openssl come from the distro (-dev
    # packages), matching how the kit's Linux consumer links httplib's extras.
    ldflags="$ldflags -lstdc++ -lm -lcurl -lz -lbz2 -ldl -lpthread"
    ldflags="$ldflags -lbrotlidec -lbrotlienc -lbrotlicommon -lzstd -lssl -lcrypto"
    ;;
  windows)
    if [[ ${#backends[@]} -gt 0 ]]; then
      ldflags="$ldflags -Wl,--whole-archive ${backends[*]} -Wl,--no-whole-archive"
    fi
    ldflags="$ldflags -lstdc++ -lz -lbz2 -lssl -lcrypto -lws2_32 -lbcrypt"
    ;;
  *)
    echo "build-ondevice: unsupported GOOS=$os" >&2
    exit 1
    ;;
esac

export CGO_ENABLED=1
export CGO_CFLAGS="-I$kit/include"
export CGO_LDFLAGS="$ldflags"

module="github.com/RunanywhereAI/wally"
out="build/wally"
[[ "$os" == windows ]] && out="build/wally.exe"
mkdir -p "$(dirname "$out")"

go_ldflags=""
[[ -n "$tag" ]] && go_ldflags="-X ${module}/version.Version=${tag}"
if [[ -n "${WALLY_BAKED_CONSOLE_API_URL:-}" ]]; then
  go_ldflags="$go_ldflags -X ${module}/config.BakedConsoleAPIURL=${WALLY_BAKED_CONSOLE_API_URL}"
fi
if [[ -n "${WALLY_BAKED_CONSOLE_WEB_ORIGIN:-}" ]]; then
  go_ldflags="$go_ldflags -X ${module}/config.BakedConsoleWebOrigin=${WALLY_BAKED_CONSOLE_WEB_ORIGIN}"
fi

echo "building ${out} (ondevice, GOOS=${os}, kit: $kit)" >&2
[[ -n "$tag" ]] && echo "version: ${tag}" >&2 || echo "version: version/version.go default" >&2
[[ -n "${WALLY_BAKED_CONSOLE_API_URL:-}" ]] && echo "channel: dev (baked endpoints)" >&2 || echo "channel: prod" >&2
go build -tags ondevice -trimpath -ldflags "${go_ldflags}" -o "$out" .
echo "built ${out}" >&2

# The macOS bottle is the Swift MLX host: build wally-mlx beside wally so the
# bottle carries both engines. WALLY_SKIP_MLX=1 skips it for a quick GGUF-only
# local iteration.
if [[ "$os" == darwin && "${WALLY_SKIP_MLX:-0}" != 1 ]]; then
  echo "building wally-mlx (Swift MLX host)..." >&2
  bash "${root_dir}/scripts/build-mlx.sh"
fi
