#!/usr/bin/env bash
# Builds the single wally binary for the host OS/arch into build/.
#
# Version is never hardcoded here. Pass -tag <git-tag> for a release build;
# a local build omits it and the binary carries whatever version/version.go
# itself sets.
#
# Console endpoints follow the same rule as config.go: WALLY_BAKED_CONSOLE_API_URL
# and WALLY_BAKED_CONSOLE_WEB_ORIGIN, if set in the environment, get baked in via
# -ldflags -X. Left unset, the binary falls back to config.go's own prod
# defaults, so a plain ./scripts/build.sh ships the production console.
set -euo pipefail

usage() {
  echo "Usage: $(basename "$0") [-tag <git-tag>] [-o <output-path>]" >&2
  exit 1
}

tag=""
out=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -tag) tag="${2:?-tag needs a value}"; shift 2 ;;
    -o) out="${2:?-o needs a value}"; shift 2 ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
done

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

module="github.com/RunanywhereAI/wally"

goos="$(go env GOOS)"
if [[ -z "$out" ]]; then
  out="build/wally"
  [[ "$goos" == "windows" ]] && out="build/wally.exe"
fi
mkdir -p "$(dirname "$out")"

cpu_count() {
  if command -v nproc >/dev/null 2>&1; then
    nproc
  elif command -v sysctl >/dev/null 2>&1; then
    sysctl -n hw.ncpu
  else
    getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1
  fi
}

ldflags=""
if [[ -n "$tag" ]]; then
  ldflags="-X ${module}/version.Version=${tag}"
fi
if [[ -n "${WALLY_BAKED_CONSOLE_API_URL:-}" ]]; then
  ldflags="${ldflags} -X ${module}/config.BakedConsoleAPIURL=${WALLY_BAKED_CONSOLE_API_URL}"
fi
if [[ -n "${WALLY_BAKED_CONSOLE_WEB_ORIGIN:-}" ]]; then
  ldflags="${ldflags} -X ${module}/config.BakedConsoleWebOrigin=${WALLY_BAKED_CONSOLE_WEB_ORIGIN}"
fi

echo "building ${out} (GOOS=${goos} GOARCH=$(go env GOARCH), $(cpu_count) CPUs)" >&2
[[ -n "$tag" ]] && echo "version: ${tag}" >&2 || echo "version: version/version.go default" >&2

go build -p "$(cpu_count)" -trimpath -ldflags "${ldflags}" -o "$out" .

echo "built ${out}" >&2
