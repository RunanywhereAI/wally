#!/usr/bin/env bash
# Demonstrate the "unknown model -> refresh -> retry" recovery flow on your CLI.
#
#   scripts/test/model-recovery-demo.sh [harness] [model]
#     harness  a coding agent you have installed (default: opencode)
#     model    the model to launch (default: glm-5.3-flash)
#
# Requires: you are signed in (`wally account login`) and the chosen harness is installed.
#
# It seeds a *stale* catalog cache that is populated but missing the model, then
# launches the harness. Because the model is missing, wally runs the recovery:
#
#   could not find '<model>' in the catalog
#   fetching the latest catalog...        (blue)
#   catalog updated, checking your model  (green, if the fetch succeeds)
#   found the model, launching the harness (blue)  -> the harness starts
#
# If the console is busy/unreachable you instead see, in red:
#   Error: sorry, the server is busy, try again after some time
#
# Your real cache is backed up and restored on exit.
set -euo pipefail

HARNESS="${1:-opencode}"
MODEL="${2:-glm-5.3-flash}"
WALLY="${WALLY:-wally}"

profile="${WALLY_PROFILE_DIR:-$HOME/.config/wally}"
cache="${profile}/models.json"

backup=""
restore() {
    if [[ -n "${backup}" ]]; then
        mv -f "${backup}" "${cache}"
    else
        rm -f "${cache}"
    fi
}
if [[ -f "${cache}" ]]; then
    backup="$(mktemp)"
    cp "${cache}" "${backup}"
fi
trap restore EXIT

mkdir -p "${profile}"
# Populated (so validation runs) but deliberately missing ${MODEL}, and stamped
# in 1970 so it also counts as stale.
printf '{"fetched_at":0,"models":["stale-placeholder-model"]}\n' > "${cache}"

echo "seeded a stale cache missing '${MODEL}'."
echo "launching: ${WALLY} ${HARNESS} -m ${MODEL}"
echo "watch for: could not find -> fetching -> catalog updated -> found -> launching"
echo
exec "${WALLY}" "${HARNESS}" -m "${MODEL}"
