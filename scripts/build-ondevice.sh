#!/usr/bin/env bash
# Builds wally with the on-device engine linked: rac_server (GGUF/llama.cpp,
# via the packaged C++ kit) wired in behind `-tags ondevice`. A plain
# scripts/build.sh never touches the kit; only this script does.
#
# The link recipe is not hand-copied. It is extracted from the kit's own
# lib/cmake/RunAnywhere/RunAnywhereConfig.cmake, which lists the exact
# archive order CMake would pass to the linker (ggml/llama/absl depend on
# each other and a wrong order fails under a strict linker) plus the
# rac_backend_* archives that need -Wl,-force_load because nothing calls a
# symbol inside them directly (they register their engine via a static
# constructor). Re-run this script after any kit bump instead of touching a
# frozen list here.
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

kit="${WALLY_SDK_KIT:-/Users/home/Project/wally-work-sapce/wally-legacy/.deps/kit}"
cmake_config="$kit/lib/cmake/RunAnywhere/RunAnywhereConfig.cmake"
lib="$kit/lib"

if [[ ! -f "$cmake_config" ]]; then
  echo "build-ondevice: no kit at $kit (set WALLY_SDK_KIT)" >&2
  exit 1
fi

# The kit's own explicit link-order list: every
# ${RunAnywhere_LIBRARY_DIR}/lib*.a token in the config, in the order CMake
# would emit them.
extra_archives="$(
  grep -o '\${RunAnywhere_LIBRARY_DIR}/[A-Za-z0-9_+.-]*\.a' "$cmake_config" \
    | sed "s#\${RunAnywhere_LIBRARY_DIR}#$lib#" \
    | awk '!seen[$0]++'
)"

# rac_backend_* archives (llamacpp, mlx, onnx, sherpa in this kit) register
# via ctor and get dropped by a normal static link unless whole-archived.
# NeuRT / QHexRT are excluded here exactly as the kit's own CMake does: they
# are private overlays linked separately and are not part of this kit.
force_load=""
for f in "$lib"/librac_backend_*.a; do
  [[ -e "$f" ]] || continue
  case "$(basename "$f")" in
    *neurt*|*qhexrt*) continue ;;
  esac
  force_load="$force_load -Wl,-force_load,$f"
done

ldflags="-L$lib $lib/librac_server.a $lib/librac_commons.a"
ldflags="$ldflags $(echo "$extra_archives" | tr '\n' ' ')"
ldflags="$ldflags $force_load"
# System deps the kit's CMake pulls via find_dependency, plus the Apple
# frameworks it links unconditionally on APPLE. -lc++ is not part of the
# kit's own recipe: CMake links C++ targets through the C++ driver, which
# adds it implicitly. cgo's external linker invokes the C driver even
# though every archive above is C++, so the standard library (__cxa_*,
# operator new/delete, __gxx_personality_v0) has to be named explicitly or
# the link fails with "symbol(s) not found" for basic runtime symbols.
ldflags="$ldflags -lc++ -lcurl -lz -lbz2 -ldl -lpthread"
ldflags="$ldflags -framework Foundation -framework Accelerate -framework Metal -framework MetalKit -framework CoreML"
ldflags="$ldflags -framework CoreFoundation -framework CFNetwork"

# librac_server.a (cpp-httplib) compiles in brotli/zstd/openssl support and
# calls their symbols directly (not through libcurl), plus CFHost
# (CFNetwork) for its own async getaddrinfo on Apple. RunAnywhereConfig.cmake
# does not declare these as find_dependency, so the kit's own consumer (rcli,
# built in the same tree as these Homebrew formulae) picks them up from the
# ambient toolchain rather than the exported config. Homebrew is that
# ambient toolchain here.
for formula in brotli zstd openssl@3; do
  prefix="$(brew --prefix "$formula" 2>/dev/null || true)"
  if [[ -z "$prefix" ]]; then
    echo "build-ondevice: brew formula '$formula' not found (needed by rac_server's httplib)" >&2
    exit 1
  fi
  ldflags="$ldflags -L$prefix/lib"
done
ldflags="$ldflags -lbrotlidec -lbrotlienc -lbrotlicommon -lzstd -lssl -lcrypto"

export CGO_ENABLED=1
export CGO_CFLAGS="-I$kit/include"
export CGO_LDFLAGS="$ldflags"

module="github.com/RunanywhereAI/wally"
out="build/wally"
mkdir -p "$(dirname "$out")"

go_ldflags="-X ${module}/config.BakedConsoleAPIURL=https://inference.runanywhere.ai/api-dev"
go_ldflags="$go_ldflags -X ${module}/config.BakedConsoleWebOrigin=https://runanywhere-frontend-development.up.railway.app"

echo "building ${out} (ondevice, kit: $kit)" >&2
go build -tags ondevice -ldflags "$go_ldflags" -o "$out" .
echo "built ${out}" >&2
