#!/usr/bin/env bash
# bundle-macos-dylibs.sh <stage-dir>
#
# Make a macOS bottle self-contained. The Swift MLX host links the kit's server
# component against Homebrew's openssl@3, brotli and zstd by absolute path
# (/opt/homebrew/opt/...). A machine without Homebrew then fails at launch with
# `dyld: Library not loaded: /opt/homebrew/opt/openssl@3/lib/libcrypto.3.dylib`,
# including during `wally update`'s own post-install check.
#
# Copies every such library (and what they pull in) into <stage>/lib, points the
# binary at it through @rpath, and leaves signing to package-wally.sh.
#
#   $WALLY_DYLIB_FALLBACK_DIRS  optional ':'-separated dirs searched for a
#                               library whose recorded path does not exist
#                               (used to re-pack a bottle off a Homebrew host).
set -euo pipefail

STAGE="${1:?usage: bundle-macos-dylibs.sh <stage-dir>}"
BIN="${STAGE}/bin/wally"
LIB="${STAGE}/lib"
mkdir -p "${LIB}"

is_brew() { [[ "$1" == /opt/homebrew/* || "$1" == /usr/local/opt/* || "$1" == /usr/local/Cellar/* ]]; }

# Dependencies of $1 that live outside the system.
brew_deps() { otool -L "$1" | awk 'NR > 1 { print $1 }' | while read -r d; do is_brew "$d" && echo "$d"; done; }

resolve() {
  local ref="$1" found
  if [[ -e "${ref}" ]]; then echo "${ref}"; return; fi
  IFS=: read -ra dirs <<<"${WALLY_DYLIB_FALLBACK_DIRS:-}"
  for d in "${dirs[@]}"; do
    found="$(find "${d}" -name "$(basename "${ref}")" 2>/dev/null | head -1)"
    [[ -n "${found}" ]] && { echo "${found}"; return; }
  done
  echo "error: cannot find ${ref} to bundle" >&2
  return 1
}

queue=("${BIN}")
while ((${#queue[@]})); do
  target="${queue[0]}"; queue=("${queue[@]:1}")
  while read -r ref; do
    [[ -n "${ref}" ]] || continue
    name="$(basename "${ref}")"
    if [[ ! -f "${LIB}/${name}" ]]; then
      cp -L "$(resolve "${ref}")" "${LIB}/${name}"
      chmod u+w "${LIB}/${name}"
      install_name_tool -id "@rpath/${name}" "${LIB}/${name}"
      queue+=("${LIB}/${name}")
    fi
    install_name_tool -change "${ref}" "@rpath/${name}" "${target}"
  done < <(brew_deps "${target}")
done

# Anything still pointing at Homebrew would reintroduce the failure.
for f in "${BIN}" "${LIB}"/*.dylib; do
  if left="$(brew_deps "${f}")" && [[ -n "${left}" ]]; then
    echo "error: ${f} still links ${left}" >&2
    exit 1
  fi
done
