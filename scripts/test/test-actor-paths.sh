#!/usr/bin/env bash
# Proves strip_trailing_slashes (scripts/lib/common.sh), which actor.sh applies
# to WALLY_KIT_DIR, WALLY_SDK_SWIFT_PATH and WALLY_BUILD_DIR. A kit dir of
# "kit/" used to put the download temp dir "kit/.download.<pid>" inside the
# kit itself, and the download failed. No network, no kit.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/common.sh
source "${SCRIPT_DIR}/../lib/common.sh"

fails=0
expect() {
  local input="$1" want="$2" got
  got="$(strip_trailing_slashes "${input}")"
  if [ "${got}" = "${want}" ]; then
    printf 'ok   %-14s -> %s\n' "'${input}'" "${got}"
  else
    printf 'FAIL %-14s -> %s (want %s)\n' "'${input}'" "${got}" "${want}"
    fails=$((fails + 1))
  fi
}

expect "/a/kit"   "/a/kit"
expect "/a/kit/"  "/a/kit"
expect "/a/kit//" "/a/kit"
expect "kit/"     "kit"
expect "/"        "/"
expect "//"       "/"
expect "/a b/k/"  "/a b/k"

[ "${fails}" -eq 0 ] || { echo "${fails} failure(s)"; exit 1; }
