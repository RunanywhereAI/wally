#!/usr/bin/env bash
# Builds the single Apple wally binary: the Go CLI compiled to a c-archive
# (wally_run_main) linked into a Swift @main host that registers MLX in-process.
# One executable serving MLX + GGUF + cloud, no wally-mlx subprocess — the OG
# shape, with Go standing in for the OG's C++ wally_run_main.
#
#   -tag <ver>                     stamps version/version.go's Version
#   WALLY_BAKED_CONSOLE_API_URL /  baked via -ldflags -X (empty = prod)
#   WALLY_BAKED_CONSOLE_WEB_ORIGIN
#
# xcodebuild (not `swift build`) owns the final link because only it compiles
# mlx-swift's Metal shaders into the bundle beside the executable.
set -euo pipefail

tag=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -tag) tag="${2:?-tag needs a value}"; shift 2 ;;
    -h|--help) echo "usage: $(basename "$0") [-tag <version>]" >&2; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

[[ "$(uname -s)" == Darwin ]] || { echo "build-app: Apple only" >&2; exit 1; }

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"
build="$root_dir/build"
mkdir -p "$build"

kit="${WALLY_SDK_KIT:-/Users/home/Project/wally-work-sapce/wally-legacy/.deps/kit}"
cmake_config="$kit/lib/cmake/RunAnywhere/RunAnywhereConfig.cmake"
lib="$kit/lib"
[[ -f "$cmake_config" ]] || { echo "build-app: no kit at $kit (set WALLY_SDK_KIT)" >&2; exit 1; }

module="github.com/RunanywhereAI/wally"

# 1. Go CLI -> c-archive. cgo LDFLAGS are not applied here; the Swift link below
#    resolves the kit's rac_* symbols via OTHER_LDFLAGS.
go_ldflags=""
[[ -n "$tag" ]] && go_ldflags="-X ${module}/version.Version=${tag}"
[[ -n "${WALLY_BAKED_CONSOLE_API_URL:-}" ]] && go_ldflags="$go_ldflags -X ${module}/config.BakedConsoleAPIURL=${WALLY_BAKED_CONSOLE_API_URL}"
[[ -n "${WALLY_BAKED_CONSOLE_WEB_ORIGIN:-}" ]] && go_ldflags="$go_ldflags -X ${module}/config.BakedConsoleWebOrigin=${WALLY_BAKED_CONSOLE_WEB_ORIGIN}"

echo "building Go c-archive (libwally.a)..." >&2
CGO_ENABLED=1 CGO_CFLAGS="-I$kit/include" \
  go build -tags ondevice -trimpath -buildmode=c-archive \
  -ldflags "$go_ldflags" -o "$build/libwally.a" .

# 2. The C++ kit link recipe, same as build-ondevice.sh's darwin branch.
extra_archives="$(
  grep -o '\${RunAnywhere_LIBRARY_DIR}/[A-Za-z0-9_+.-]*\.a' "$cmake_config" \
    | sed "s#\${RunAnywhere_LIBRARY_DIR}#$lib#" | awk '!seen[$0]++'
)"
kitflags="-L$lib $lib/librac_server.a $lib/librac_commons.a $(echo "$extra_archives" | tr '\n' ' ')"
for f in "$lib"/librac_backend_*.a; do
  [[ -e "$f" ]] || continue
  case "$(basename "$f")" in *neurt*|*qhexrt*) continue ;; esac
  kitflags="$kitflags -Wl,-force_load,$f"
done
kitflags="$kitflags -lc++ -lcurl -lz -lbz2 -ldl -lpthread"
kitflags="$kitflags -framework Foundation -framework Accelerate -framework Metal -framework MetalKit -framework CoreML -framework CoreFoundation -framework CFNetwork"
for formula in brotli zstd openssl@3; do
  prefix="$(brew --prefix "$formula" 2>/dev/null || true)"
  [[ -n "$prefix" ]] || { echo "build-app: brew formula '$formula' not found" >&2; exit 1; }
  kitflags="$kitflags -L$prefix/lib"
done
kitflags="$kitflags -lbrotlidec -lbrotlienc -lbrotlicommon -lzstd -lssl -lcrypto"

# 3. xcodebuild the Swift host, force-loading the Go archive and the kit.
other_ldflags="-Wl,-force_load,$build/libwally.a $kitflags"
derived="$build/.wally-app-xcode"
xlog="$build/xcodebuild-app.log"
cd "$root_dir/swift"
set +e
xcodebuild build \
  -scheme wally-mlx \
  -destination "platform=macOS,arch=$(uname -m)" \
  -configuration Release \
  -derivedDataPath "$derived" \
  OTHER_LDFLAGS="$other_ldflags" \
  >"$xlog" 2>&1
status=$?
set -e
if [[ "$status" -ne 0 ]]; then
  echo "build-app: xcodebuild failed ($status)" >&2
  grep -E "error:|Undefined symbols|ld: |clang: error|library not found" "$xlog" >&2 || true
  tail -40 "$xlog" >&2
  exit "$status"
fi

products="$derived/Build/Products/Release"
[[ -x "$products/wally-app" ]] || { echo "build-app: no wally-app produced" >&2; exit 1; }
cp "$products/wally-app" "$build/wally"
shopt -s nullglob
for bundle in "$products"/*.bundle; do
  dest="$build/$(basename "$bundle")"; rm -rf "$dest"; cp -R "$bundle" "$dest"
done
shopt -u nullglob
echo "built $build/wally (single binary)" >&2
