#!/usr/bin/env bash
# Download and verify a pinned C++ desktop kit, then extract it to DEST.
#
#   scripts/fetch-kit.sh <macos-arm64|linux-x64|linux-arm64|windows-x64|windows-arm64> <dest-dir>
#
# Requires: curl or wget, and either shasum or sha256sum. The kit lives on a
# public GitHub release, so plain HTTPS works; `gh` is only a fallback.
# Pins live in versions.toml (the single source; cmake/sdk-pin.cmake reads the
# same file). SDK_VERSION, if set, must equal kit_version there -- checksums are
# keyed to that pin, not a repo variable.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "${ROOT}/scripts/lib/common.sh"
PLATFORM="${1:?usage: fetch-kit.sh <macos-arm64|linux-x64|linux-arm64|windows-x64|windows-arm64> <dest>}"
DEST="${2:?usage: fetch-kit.sh <macos-arm64|linux-x64|linux-arm64|windows-x64|windows-arm64> <dest>}"
PIN="${ROOT}/versions.toml"

PINNED_SDK="$(wally_kit_version)"
EXPECTED="$(wally_kit_sha "$PLATFORM")" || exit 2
if [[ -z "$PINNED_SDK" || -z "$EXPECTED" ]]; then
  echo "error: missing kit pin for '$PLATFORM' or kit_version in $PIN" >&2
  exit 1
fi
if [[ -n "${SDK_VERSION:-}" && "$SDK_VERSION" != "$PINNED_SDK" ]]; then
  echo "error: SDK_VERSION=$SDK_VERSION does not match kit_version=$PINNED_SDK in $PIN" >&2
  echo "  bump versions.toml (kit_version + kit_sha256_*) together; do not override only SDK_VERSION" >&2
  exit 1
fi
SDK_VERSION="$PINNED_SDK"

# The kit's own version (baked into the tarball name from core/VERSION) is always
# SDK_VERSION. The GitHub Release it hangs on is separate: kit-only releases live
# on a `cpp-desktop-v<ver>` tag so they never trip the full SDK release train,
# while a kit cut by that train sits on the plain `v<ver>` tag. `kit_release_tag`
# in versions.toml names it; absent, we fall back to `v<ver>` for older pins.
RELEASE_TAG="$(wally_kit_release_tag)"

asset="RunAnywhere-cpp-desktop-${PLATFORM}-v${SDK_VERSION}.tar.gz"
url="https://github.com/RunanywhereAI/runanywhere-sdks/releases/download/${RELEASE_TAG}/${asset}"
dl="$(mktemp -d)"
file="${dl}/${asset}"
trap 'rm -rf "$dl"' EXIT

# Draft GitHub Releases are invisible even to a signed-in token (`release not
# found`); the pin must point at a published release. Plain HTTPS first -- no
# auth, no gh -- then gh as fallback for networks or tokens that need it. Each
# attempt writes the same path, so a partial file from a failed attempt is
# dropped before the next, and gh gets --clobber to overwrite a leftover.
dl_ok=""
if curl -fSL --retry 3 -o "$file" "$url"; then
  dl_ok=1
elif rm -f "$file" && wget -q -O "$file" "$url"; then
  dl_ok=1
elif rm -f "$file" && gh release download "$RELEASE_TAG" \
    --repo RunanywhereAI/runanywhere-sdks \
    --pattern "$asset" --dir "$dl" --clobber; then
  dl_ok=1
fi
if [[ -z "$dl_ok" ]]; then
  echo "error: could not download $asset from RunanywhereAI/runanywhere-sdks@${RELEASE_TAG}" >&2
  echo "  that tag must be a published GitHub Release (drafts 404 for this token)." >&2
  gh release view "$RELEASE_TAG" --repo RunanywhereAI/runanywhere-sdks >&2 || true
  exit 1
fi

if command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$file" | awk '{print $1}')"
else
  actual="$(sha256sum "$file" | awk '{print $1}')"
fi
if [[ "$actual" != "$EXPECTED" ]]; then
  echo "error: kit checksum mismatch for $asset" >&2
  echo "  expected $EXPECTED" >&2
  echo "  actual   $actual" >&2
  exit 1
fi

# GitHub Windows runners pass GITHUB_WORKSPACE as D:\a\...; msys tar cannot -C that.
if command -v cygpath >/dev/null 2>&1; then
  DEST="$(cygpath -u "$DEST")"
fi
mkdir -p "$DEST"
tar xzf "$file" -C "$DEST" --strip-components=1
echo "kit verified and extracted to $DEST"

# Optional NeuRT / QHexRT overlay. Missing overlay is not a failure unless
# WALLY_REQUIRE_PRIVATE=1 — public bottles stay OSS.
if [[ -x "$ROOT/scripts/build/fetch-private-pack.sh" ]]; then
  "$ROOT/scripts/build/fetch-private-pack.sh" "$PLATFORM" "$DEST"
fi
