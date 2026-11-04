#!/usr/bin/env bash
#
# actor.sh — one entry point for a local wally build: deps, downloads, build,
# tests, lints and the CI checks, run with the same commands CI runs.
#
#   ./actor.sh <command> [command ...]
#
# Commands run in the order given and stop at the first failing command.
# `./actor.sh help` lists them. Every pin comes from versions.toml through
# scripts/lib/common.sh; nothing here hardcodes a version, a sha or a tag.
#
# Environment (all optional):
#   WALLY_KIT_DIR          kit prefix              default <repo>/kit
#   WALLY_SDK_SWIFT_PATH   runanywhere-sdks tree   default <repo>/.deps/runanywhere-sdks
#   WALLY_BUILD_DIR        CMake build tree        default <repo>/build
#   WALLY_BUILD_TYPE       Release | Debug         default Release
#   WALLY_JOBS             parallel jobs           default all logical CPUs
#   WALLY_APPLE_MLX_HOST   OFF skips the Swift host and builds build/wally-cxx
#   WALLY_CONSOLE_OPTIONAL 1 builds without console endpoints (fork builds)
#
# Exit codes: 0 ok | 1 a step failed | 2 usage or missing tool
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/common.sh
source "${ROOT}/scripts/lib/common.sh"
cd "${ROOT}"

PLATFORM="$(wally_host_platform)"
KIT_DIR="${WALLY_KIT_DIR:-${ROOT}/kit}"
SDK_DIR="${WALLY_SDK_SWIFT_PATH:-${ROOT}/.deps/runanywhere-sdks}"
BUILD_DIR="${WALLY_BUILD_DIR:-${ROOT}/build}"
BUILD_TYPE="${WALLY_BUILD_TYPE:-Release}"
MLX_HOST="${WALLY_APPLE_MLX_HOST:-ON}"
BINDGEN_VERSION="0.73.2"

case "${PLATFORM}" in
  macos-arm64) JOBS="${WALLY_JOBS:-$(sysctl -n hw.logicalcpu)}" ;;
  linux-*)     JOBS="${WALLY_JOBS:-$(nproc)}" ;;
  *)           JOBS="${WALLY_JOBS:-${NUMBER_OF_PROCESSORS:-4}}" ;;
esac

die() { printf 'error: %s\n' "$1" >&2; exit "${2:-1}"; }
have() { command -v "$1" >/dev/null 2>&1; }

# Product binary for this host: the Swift MLX host on Apple, wally-cxx when the
# host is off, wally.exe on Windows.
product_binary() {
  case "${PLATFORM}" in
    macos-arm64)
      if [ "${MLX_HOST}" = OFF ]; then echo "${BUILD_DIR}/wally-cxx"; else echo "${BUILD_DIR}/wally"; fi ;;
    windows-*) echo "${BUILD_DIR}/wally.exe" ;;
    *)         echo "${BUILD_DIR}/wally" ;;
  esac
}

# Xcode's linker cannot read a newer Command Line Tools SDK (ld fails with
# "unknown architecture arm64e.x1"). Pin the SDK to the one that ships with
# the selected developer dir, unless the caller already chose one.
apple_sdkroot() {
  [ "${PLATFORM}" = macos-arm64 ] || return 0
  if [ -z "${SDKROOT:-}" ]; then
    SDKROOT="$(xcrun --sdk macosx --show-sdk-path)" \
      || die "xcrun cannot find a macOS SDK; run xcode-select --install" 2
    export SDKROOT
  fi
}

# Steps inside one command keep going after a failure so a single run reports
# every broken check; the command fails at the end if any step did.
FAILED=()
run_step() {
  local name="$1"; shift
  log_step "${name}"
  if "$@"; then
    log_ok "${name}"
  else
    log_fail "${name}"
    FAILED+=("${name}")
  fi
}
finish_steps() {
  local what="$1"
  if [ "${#FAILED[@]}" -ne 0 ]; then
    printf '\n%s failed:\n' "${what}" >&2
    printf '  - %s\n' "${FAILED[@]}" >&2
    FAILED=()
    return 1
  fi
}

version_at_least() {
  # version_at_least <have> <want>: true when have >= want (dotted numbers).
  [ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" = "$2" ]
}

# --- deps --------------------------------------------------------------------

cmd_deps() {
  local missing=0 want_cmake want_rust have_cmake

  want_cmake="$(wally_pin cmake_minimum)"
  want_rust="$(wally_pin rust)"

  need() {
    if have "$1"; then log_ok "$1"; else log_fail "$1 not on PATH ($2)"; missing=1; fi
  }
  optional() {
    if have "$1"; then log_ok "$1"; else printf '  skip %s not on PATH (%s)\n' "$1" "$2" >&2; fi
  }

  log_step "required tools"
  need git     "install git"
  need cmake   "brew install cmake"
  need ninja   "brew install ninja"
  need rustup  "https://rustup.rs"
  need cargo   "rustup installs it"
  need python3 "install python 3"
  need gh      "brew install gh; used to download the kit"
  if [ "${PLATFORM}" = macos-arm64 ]; then
    need xcrun "xcode-select --install"
    [ "${MLX_HOST}" = OFF ] || need swift "install Xcode 26 for the MLX host"
  fi

  log_step "versions"
  if have cmake; then
    have_cmake="$(cmake --version | sed -n 's/^cmake version \([0-9.]*\).*/\1/p')"
    if version_at_least "${have_cmake}" "${want_cmake}"; then
      log_ok "cmake ${have_cmake} (needs ${want_cmake}+)"
    else
      log_fail "cmake ${have_cmake} is older than ${want_cmake}"; missing=1
    fi
  fi
  if have rustup; then
    if rustup toolchain list | grep -q "^${want_rust}"; then
      log_ok "rust ${want_rust} toolchain installed"
    else
      log_fail "rust ${want_rust} missing: rustup toolchain install ${want_rust} --component rustfmt,clippy"; missing=1
    fi
  fi
  if have gh; then
    if gh auth status >/dev/null 2>&1; then log_ok "gh authenticated"; else log_fail "gh not signed in: gh auth login"; missing=1; fi
  fi

  log_step "console endpoints"
  if [ "${WALLY_CONSOLE_OPTIONAL:-}" = 1 ]; then
    log_ok "WALLY_CONSOLE_OPTIONAL=1, building without a default console"
  else
    local key value
    for key in WALLY_BAKED_CONSOLE_API_URL WALLY_BAKED_CONSOLE_WEB_ORIGIN; do
      value="${!key:-}"
      if [ -z "${value}" ] && [ -f .env ]; then
        value="$(sed -n "s/^${key}=//p" .env | head -1)"
      fi
      if [ -n "${value}" ]; then log_ok "${key} set"; else log_fail "${key} empty: cp .env.example .env and fill it in"; missing=1; fi
    done
  fi

  log_step "optional tools (lint and check)"
  optional shellcheck "brew install shellcheck; lints install.sh"
  optional dash       "brew install dash; runs install.sh as Debian's sh"
  optional ruby       "checks Formula/wally.rb"
  if have bindgen && bindgen --version 2>/dev/null | grep -q "${BINDGEN_VERSION}"; then
    log_ok "bindgen ${BINDGEN_VERSION}"
  else
    printf '  skip bindgen %s missing (cargo install bindgen-cli --version %s --locked)\n' \
      "${BINDGEN_VERSION}" "${BINDGEN_VERSION}" >&2
  fi

  [ "${missing}" -eq 0 ] || die "missing dependencies above" 2
}

# --- downloads -----------------------------------------------------------------

# The kit is re-fetched when its stamp does not match versions.toml, so a pin
# bump never builds against a stale prefix.
cmd_kit() {
  local want stamp="${KIT_DIR}/.wally-kit-version"
  want="$(wally_kit_version)"
  if [ -f "${stamp}" ] && [ "$(cat "${stamp}")" = "${want}" ] \
     && [ -f "${KIT_DIR}/share/runanywhere/SCHEMA_LOCK" ]; then
    log_ok "kit ${want} already at ${KIT_DIR}"
    return 0
  fi
  log_step "fetch kit ${want} (${PLATFORM}) -> ${KIT_DIR}"
  rm -rf "${KIT_DIR}"
  bash scripts/build/fetch-kit.sh "${PLATFORM}" "${KIT_DIR}"
  [ -f "${KIT_DIR}/include/runanywhere/proto/model_types.pb.h" ] \
    || die "kit has no generated proto headers under ${KIT_DIR}/include"
  [ -f "${KIT_DIR}/share/runanywhere/SCHEMA_LOCK" ] \
    || die "kit has no share/runanywhere/SCHEMA_LOCK"
  printf '%s\n' "${want}" > "${stamp}"
}

# The Swift tree is checked out at the exact sdk_ci_ref CI uses, so the MLX host
# links the same Swift code a release ships.
cmd_sdk() {
  [ "${PLATFORM}" = macos-arm64 ] || { log_ok "no Swift host on ${PLATFORM}"; return 0; }
  [ "${MLX_HOST}" = OFF ] && { log_ok "MLX host off, no Swift tree needed"; return 0; }
  local ref
  ref="$(wally_pin sdk_ci_ref)"
  if [ -d "${SDK_DIR}/.git" ] && [ "$(git -C "${SDK_DIR}" rev-parse HEAD)" = "${ref}" ]; then
    log_ok "runanywhere-sdks at ${ref:0:12}"
    return 0
  fi
  log_step "checkout runanywhere-sdks@${ref:0:12} -> ${SDK_DIR}"
  if [ ! -d "${SDK_DIR}/.git" ]; then
    mkdir -p "${SDK_DIR}"
    git -C "${SDK_DIR}" init -q
    git -C "${SDK_DIR}" remote add origin https://github.com/RunanywhereAI/runanywhere-sdks.git
  fi
  git -C "${SDK_DIR}" fetch -q --depth 1 origin "${ref}"
  git -C "${SDK_DIR}" checkout -q --detach FETCH_HEAD
}

# --- build ---------------------------------------------------------------------

cmd_configure() {
  [ -f "${KIT_DIR}/share/runanywhere/SCHEMA_LOCK" ] || die "no kit at ${KIT_DIR}; run ./actor.sh kit"
  apple_sdkroot
  local args=(-B "${BUILD_DIR}" -G Ninja -DCMAKE_BUILD_TYPE="${BUILD_TYPE}" -DCMAKE_PREFIX_PATH="${KIT_DIR}")
  if [ "${PLATFORM}" = macos-arm64 ]; then
    args+=(-DCMAKE_OSX_SYSROOT="${SDKROOT}")
    if [ "${MLX_HOST}" = OFF ]; then
      args+=(-DWALLY_APPLE_MLX_HOST=OFF -DWALLY_APPLE_CXX_BINARY=ON)
    else
      export WALLY_SDK_SWIFT_PATH="${SDK_DIR}"
    fi
  fi
  if have ccache; then args+=(-DCMAKE_CXX_COMPILER_LAUNCHER=ccache); fi
  log_step "configure ${BUILD_TYPE} -> ${BUILD_DIR}"
  cmake "${args[@]}"
}

cmd_build() {
  # build-mlx.sh reads WALLY_SDK_SWIFT_PATH and SDKROOT at build time, not
  # only at configure time, so both are exported for the build step too, and
  # the Swift tree is checked out at sdk_ci_ref first.
  if [ "${PLATFORM}" = macos-arm64 ] && [ "${MLX_HOST}" != OFF ]; then
    cmd_sdk
    export WALLY_SDK_SWIFT_PATH="${SDK_DIR}"
  fi
  [ -f "${BUILD_DIR}/build.ninja" ] || cmd_configure
  apple_sdkroot
  log_step "build (${JOBS} jobs)"
  cmake --build "${BUILD_DIR}" -j "${JOBS}"
  "$(product_binary)" --version
}

# --- tests ---------------------------------------------------------------------

cmd_test() {
  [ -f "${BUILD_DIR}/build.ninja" ] || die "not configured; run ./actor.sh build"
  apple_sdkroot
  run_step "ctest (cargo test)" ctest --test-dir "${BUILD_DIR}" --output-on-failure -j "${JOBS}"
  run_step "update download failures" python3 scripts/test/test-update-download.py "$(product_binary)"
  finish_steps test
}

cmd_e2e() {
  local bin
  bin="$(product_binary)"
  [ -x "${bin}" ] || die "no ${bin}; run ./actor.sh build"
  WALLY_SDK_KIT="${KIT_DIR}" bash scripts/test/e2e.sh "${bin}"
}

# --- lint ----------------------------------------------------------------------

clippy() {
  WALLY_BUILD_ENV="${BUILD_DIR}/generated/wally-build.env" \
  CARGO_TARGET_DIR="${BUILD_DIR}/cargo" \
    cargo clippy --all-targets -- -D warnings
}

shell_syntax() {
  bash -n actor.sh scripts/build/package-wally.sh scripts/release/update-tap.sh && sh -n install.sh
}

cmd_lint() {
  apple_sdkroot
  run_step "cargo fmt --check" cargo fmt --check
  if [ -f "${BUILD_DIR}/generated/wally-build.env" ]; then
    run_step "cargo clippy -D warnings" clippy
  else
    printf '  skip clippy needs a configured build; run ./actor.sh build first\n' >&2
  fi
  run_step "shell syntax" shell_syntax
  if have shellcheck; then
    run_step "shellcheck install.sh (POSIX sh)" shellcheck -s sh install.sh
    run_step "shellcheck actor.sh" shellcheck -x actor.sh
  fi
  if have dash; then run_step "dash -n install.sh" dash -n install.sh; fi
  if have ruby; then run_step "ruby -c Formula/wally.rb" ruby -c Formula/wally.rb; fi
  run_step "AGENTS.md and skills mirror" bash scripts/ci/check-agents-sync.sh
  finish_steps lint
}

# --- checks (hermetic: no build, no network) -----------------------------------

contract_checks() {
  python3 tests/test_contract_sync.py \
    && python3 contracts/generate_console_binding.py --check \
    && python3 contracts/sync_from_inferenceinfra.py --check
}
version_checks()  { python3 tests/test_versions.py && python3 scripts/ci/check-versions.py; }
retired_ids()     { python3 tests/test_retired_model_ids.py && python3 scripts/ci/check-retired-model-ids.py; }
release_tooling() {
  python3 tests/test_release_assets.py \
    && python3 tests/test_linux_abi.py \
    && python3 -m py_compile scripts/release/verify-release-assets.py scripts/release/stamp-formula.py scripts/release/check-linux-abi.py
}
install_tests() {
  bash scripts/test/test-install-cross-shell.sh \
    && bash scripts/test/test-install-skill-dirs.sh \
    && bash scripts/test/test-install-lock.sh \
    && bash scripts/test/test-install-libstdcxx-preflight.sh
}

cmd_check() {
  run_step "console contract and binding" contract_checks
  run_step "versions.toml consistency" version_checks
  run_step "WallyRust.cmake generator guard" python3 tests/test_wally_rust_cmake_guard.py
  run_step "no retired model ids" retired_ids
  run_step "C++ test ledger" python3 scripts/test/test-ledger.py
  run_step "release tooling" release_tooling
  run_step "install.sh tests" install_tests
  finish_steps check
}

# Needs bindgen and the kit headers; macOS only, like CI.
cmd_bindings() {
  [ "${PLATFORM}" = macos-arm64 ] || { log_ok "bindings check runs on macOS only"; return 0; }
  if ! { have bindgen && bindgen --version | grep -q "${BINDGEN_VERSION}"; }; then
    die "bindgen ${BINDGEN_VERSION} missing: cargo install bindgen-cli --version ${BINDGEN_VERSION} --locked" 2
  fi
  WALLY_SDK_KIT="${KIT_DIR}" bash scripts/build/gen-sys-bindings.sh --check
}

# --- groups --------------------------------------------------------------------

cmd_setup() { cmd_deps; cmd_kit; cmd_sdk; }
cmd_ci()    { cmd_check; cmd_build; cmd_test; cmd_lint; }
cmd_all()   { cmd_setup; cmd_ci; cmd_bindings; }

cmd_clean() {
  log_step "remove ${BUILD_DIR}"
  rm -rf "${BUILD_DIR}"
}

usage() {
  cat <<EOF
actor.sh — build, test, lint and check wally locally, the way CI does.

Usage: ./actor.sh <command> [command ...]

Setup
  deps       check tools, versions, gh sign-in and console endpoints
  kit        download the pinned C++ kit to \$WALLY_KIT_DIR (skips if current)
  sdk        check out runanywhere-sdks at sdk_ci_ref (macOS MLX host only)
  setup      deps + kit + sdk

Build
  configure  cmake configure against the kit (pins the Xcode SDK on macOS)
  build      configure if needed, build, print the binary's version
  clean      delete the build tree

Verify
  test       ctest (cargo test) and the update-download check
  lint       cargo fmt, clippy, shellcheck, shell syntax, AGENTS.md sync
  check      hermetic CI checks: contract, versions, ledger, install tests
  bindings   src/sys/bindings.rs matches the kit (macOS, needs bindgen)
  e2e        product e2e against the kit (scripts/test/e2e.sh)

Groups
  ci         check + build + test + lint
  all        setup + ci + bindings

Kit:   ${KIT_DIR}
Build: ${BUILD_DIR} (${BUILD_TYPE}, ${JOBS} jobs)
EOF
}

[ "$#" -gt 0 ] || { usage; exit 2; }

for command in "$@"; do
  case "${command}" in
    deps|kit|sdk|setup|configure|build|clean|test|lint|check|bindings|e2e|ci|all)
      "cmd_${command}" ;;
    -h|--help|help) usage ;;
    *) printf 'error: unknown command: %s\n\n' "${command}" >&2; usage >&2; exit 2 ;;
  esac
done
