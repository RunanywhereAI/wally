#!/usr/bin/env bash
# Shared helpers for wally's build/test/release scripts. Source it, don't run it:
#
#   source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
#
# versions.toml is the single source of truth for every pin; nothing here or in
# a caller hardcodes a version, a sha, or a kit tag. Functions use `local` and
# print to stderr, so a caller's stdout stays its own.

# Repo root, regardless of which subdirectory the sourcing script lives in.
wally_root() {
  local here
  here="$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)"
  # Walk up until we find versions.toml (the repo marker), so a script at any
  # depth under scripts/ resolves the same root.
  while [[ "${here}" != "/" && ! -f "${here}/versions.toml" ]]; do
    here="$(dirname "${here}")"
  done
  printf '%s\n' "${here}"
}

# One flat `key = "value"` line out of versions.toml. The file's own header
# guarantees the flat format, so one regex reads any pin.
wally_pin() {
  local key="$1" root
  root="$(wally_root)"
  sed -n "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p" \
    "${root}/versions.toml" | head -1
}

wally_version()       { wally_pin version; }
wally_kit_version()   { wally_pin kit_version; }
wally_kit_release_tag() {
  local tag; tag="$(wally_pin kit_release_tag)"
  printf '%s\n' "${tag:-v$(wally_kit_version)}"
}

# Kit sha for a platform tag
# (macos-arm64|windows-x64|windows-arm64|linux-x64|linux-arm64).
wally_kit_sha() {
  case "$1" in
    macos-arm64)   wally_pin kit_sha256_macos_arm64 ;;
    windows-x64)   wally_pin kit_sha256_windows_x64 ;;
    windows-arm64) wally_pin kit_sha256_windows_arm64 ;;
    linux-x64)     wally_pin kit_sha256_linux_x64 ;;
    linux-arm64)   wally_pin kit_sha256_linux_arm64 ;;
    *) echo "error: unknown platform '$1'" >&2; return 2 ;;
  esac
}

# The host's own kit platform tag, for a local build.
wally_host_platform() {
  case "$(uname -s)/$(uname -m)" in
    Darwin/*)                 echo macos-arm64 ;;
    Linux/aarch64|Linux/arm64) echo linux-arm64 ;;
    Linux/*)                  echo linux-x64 ;;
    *)                        echo windows-x64 ;;
  esac
}

# "a/b//" -> "a/b"; "/" stays "/".
strip_trailing_slashes() {
  local path="$1"
  while [[ "${path}" == */ && "${path}" != "/" ]]; do path="${path%/}"; done
  printf '%s' "${path}"
}

log_step() { printf '== %s ==\n' "$*" >&2; }
log_ok()   { printf '  ok   %s\n' "$*" >&2; }
log_fail() { printf '  FAIL %s\n' "$*" >&2; }
