#!/usr/bin/env bash
# fetch-kit.sh <platform> <version> <dest>
#
# Downloads the public C++ desktop kit into <dest>. The kit lives on
# RunanywhereAI/runanywhere-sdks (a public repo) as a cpp-desktop-v<ver>
# release asset, so the default github.token is enough — no PAT, and a plain
# anonymous HTTP download works too.
#
#   platform: macos-arm64 | linux-x64 | windows-x64 | windows-arm64
set -euo pipefail

platform="${1:?usage: fetch-kit.sh <platform> <version> <dest>}"
version="${2:?usage: fetch-kit.sh <platform> <version> <dest>}"
dest="${3:?usage: fetch-kit.sh <platform> <version> <dest>}"

asset="RunAnywhere-cpp-desktop-${platform}-v${version}.tar.gz"
tmp="$(mktemp -d)"

gh release download "cpp-desktop-v${version}" \
  --repo RunanywhereAI/runanywhere-sdks \
  --pattern "${asset}" \
  --dir "${tmp}"

mkdir -p "${dest}"
tar -xzf "${tmp}/${asset}" -C "${dest}" --strip-components=1

if [[ ! -f "${dest}/lib/cmake/RunAnywhere/RunAnywhereConfig.cmake" ]]; then
  echo "fetch-kit: ${dest} has no RunAnywhereConfig.cmake after extract" >&2
  ls -la "${dest}" >&2
  exit 1
fi
echo "kit ${version} (${platform}) -> ${dest}" >&2
