#!/usr/bin/env bash
# Exercise the installed launch layout with a real, already-downloaded MLX
# model. No installation changes or model downloads. The temporary directory
# deliberately contains neither a resource bundle nor a metallib.
#   scripts/test/smoke-mlx-symlink.sh /absolute/path/to/wally [model]
set -euo pipefail

BIN="${1:?usage: smoke-mlx-symlink.sh <wally> [model]}"
MODEL="${2:-mlx-qwen3-0.6b-4bit}"
if [[ "$(uname -s)" != Darwin ]]; then
    echo "smoke-mlx-symlink: SKIP (macOS only)"
    exit 0
fi
BIN="$(cd "$(dirname "${BIN}")" && pwd)/$(basename "${BIN}")"
[[ -x "${BIN}" ]] || { echo "not executable: ${BIN}" >&2; exit 1; }
scratch="$(mktemp -d "${TMPDIR:-/tmp}/wally-mlx-symlink.XXXXXX")"
trap 'rm -rf "${scratch}"' EXIT
ln -s "${BIN}" "${scratch}/wally"
output="$(cd "${scratch}" && ./wally run "${MODEL}" "Say hello." \
    --max-output-tokens 4 --no-think)"
[[ -n "${output//[[:space:]]/}" ]] || { echo "MLX returned no text" >&2; exit 1; }
printf 'smoke-mlx-symlink: ok (%s)\n%s\n' "${MODEL}" "${output}"
