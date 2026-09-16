#!/usr/bin/env bash
# Dev build: bakes the dev console endpoints and builds to build/wally-dev,
# so a dev binary never sits under the same name as a prod one.
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

export WALLY_BAKED_CONSOLE_API_URL="${WALLY_BAKED_CONSOLE_API_URL:-https://inference.runanywhere.ai/api-dev}"
export WALLY_BAKED_CONSOLE_WEB_ORIGIN="${WALLY_BAKED_CONSOLE_WEB_ORIGIN:-https://runanywhere-frontend-development.up.railway.app}"

goos="$(go env GOOS)"
out="build/wally-dev"
[[ "$goos" == "windows" ]] && out="build/wally-dev.exe"

exec "$root_dir/scripts/build.sh" -o "$out" "$@"
