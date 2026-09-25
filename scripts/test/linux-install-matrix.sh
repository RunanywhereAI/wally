#!/usr/bin/env bash
# Install a Linux bottle with install.sh on clean distributions, the way a user does.
#
#   scripts/test/linux-install-matrix.sh <dir-with-archive-and-sha256> <version>
#
# Every image starts with nothing but curl added: no compiler, so no libgomp,
# which is exactly the machine v0.6.0 failed on. The installer fetches the
# archive from the directory through WALLY_INSTALL_BASE_URL, so the checksum,
# the preflight, the staged probe and the atomic swap all run as released.
#
# Supported images must install and start. Unsupported ones must be refused
# before download, with a message that names the reason: that refusal is
# product behaviour, and a regression to a loader error is a failure here.
#
# Needs docker. Images run as linux/amd64; on an arm64 host that is emulation.
set -euo pipefail

DIST="${1:?usage: linux-install-matrix.sh <dist-dir> <version>}"
VERSION="${2:?usage: linux-install-matrix.sh <dist-dir> <version>}"
VERSION="${VERSION#v}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIST="$(cd "${DIST}" && pwd)"
ARCHIVE="wally-${VERSION}-linux-x86_64.tar.gz"
[[ -f "${DIST}/${ARCHIVE}" && -f "${DIST}/${ARCHIVE}.sha256" ]] || {
  echo "error: ${DIST} needs ${ARCHIVE} and ${ARCHIVE}.sha256" >&2
  exit 1
}

# image | expectation | text the output must contain
CASES=(
  "ubuntu:22.04|installs|wally ${VERSION}"
  "ubuntu:24.04|installs|wally ${VERSION}"
  "debian:12|installs|wally ${VERSION}"
  "ubuntu:20.04|refused|needs glibc 2.35 or newer"
  "alpine:3.20|refused|uses musl"
)

run_case() {
  local image="$1"
  docker run --rm --platform linux/amd64 \
    -v "${DIST}:/dist:ro" -v "${ROOT}/install.sh:/install.sh:ro" \
    -e WALLY_INSTALL_VERSION="${VERSION}" -e WALLY_INSTALL_BASE_URL=file:///dist \
    "${image}" sh -c '
      set -u
      if command -v apt-get >/dev/null; then
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq >/dev/null && apt-get install -y -qq --no-install-recommends curl ca-certificates >/dev/null
      elif command -v apk >/dev/null; then
        apk add --no-cache -q curl >/dev/null
      fi
      sh /install.sh </dev/null 2>&1
      status=$?
      echo "::install-exit=${status}"
      [ "${status}" -eq 0 ] || exit 0
      "${HOME}/.local/bin/wally" --version 2>&1
      "${HOME}/.local/bin/wally" backends 2>&1
    '
}

failures=0
for entry in "${CASES[@]}"; do
  IFS='|' read -r image expectation needle <<<"${entry}"
  output="$(run_case "${image}" 2>&1)" || true
  status="$(printf '%s\n' "${output}" | sed -n 's/^::install-exit=//p' | tail -1)"
  verdict="ok"
  case "${expectation}" in
    installs)
      if [[ "${status}" != 0 ]] || ! grep -qF "${needle}" <<<"${output}" ||
        ! grep -qE '^llamacpp ' <<<"${output}"; then
        verdict="FAIL (expected a working install)"
      fi
      ;;
    refused)
      if [[ "${status}" == 0 || -z "${status}" ]] || ! grep -qF "${needle}" <<<"${output}" ||
        grep -q 'Downloading' <<<"${output}"; then
        verdict="FAIL (expected a refusal before download naming: ${needle})"
      fi
      ;;
  esac
  printf '%-14s %-9s %s\n' "${image}" "${expectation}" "${verdict}"
  if [[ "${verdict}" != ok ]]; then
    failures=$((failures + 1))
    printf '%s\n' "${output}" | tail -20 | sed 's/^/    /'
  fi
done

if [[ "${failures}" -ne 0 ]]; then
  echo "error: ${failures} install case(s) failed" >&2
  exit 1
fi
echo "every install case behaved"
