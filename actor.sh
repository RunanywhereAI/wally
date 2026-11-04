#!/usr/bin/env bash
#
# actor.sh — one entry point for a local wally build: deps, downloads, build,
# tests, lints and the CI checks, run with the same commands CI runs.
#
#   ./actor.sh <command> [command ...]
#
# Before any command runs, a preflight detects the OS, the Apple toolchain and
# every tool the requested commands need, prints them as one report, and stops
# before touching anything when a required tool is missing. Commands then run
# in the order given and stop at the first failing command. `./actor.sh help`
# lists them. Every pin comes from versions.toml through scripts/lib/common.sh.
#
# Environment (all optional):
#   WALLY_KIT_DIR          kit prefix              default <repo>/kit
#   WALLY_SDK_SWIFT_PATH   runanywhere-sdks tree   default <repo>/.deps/runanywhere-sdks
#   WALLY_BUILD_DIR        CMake build tree        default <repo>/build
#   WALLY_BUILD_TYPE       Release | Debug         default Release
#   WALLY_JOBS             parallel build jobs     default all logical CPUs
#   WALLY_APPLE_MLX_HOST   ON | OFF                default ON when Swift and Xcode are present
#   WALLY_CONSOLE_OPTIONAL 1 builds without console endpoints (fork builds)
#   NO_COLOR               any value turns colour off
#
# Directories actor.sh did not create are never deleted or moved: a kit it
# did not download is used as is, and an SDK checkout you point it at is only
# checked against the pinned commit.
#
# Exit codes: 0 ok | 1 a step failed | 2 usage error or missing requirement
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/common.sh
source "${ROOT}/scripts/lib/common.sh"
cd "${ROOT}"

DEFAULT_KIT_DIR="${ROOT}/kit"
DEFAULT_SDK_DIR="${ROOT}/.deps/runanywhere-sdks"
KIT_DIR="${WALLY_KIT_DIR:-${DEFAULT_KIT_DIR}}"
SDK_DIR="${WALLY_SDK_SWIFT_PATH:-${DEFAULT_SDK_DIR}}"
BUILD_DIR="${WALLY_BUILD_DIR:-${ROOT}/build}"
BUILD_TYPE="${WALLY_BUILD_TYPE:-Release}"
BINDGEN_VERSION="0.73.2"
KIT_STAMP=".wally-kit-version"
SDK_REMOTE="https://github.com/RunanywhereAI/runanywhere-sdks.git"

# The default SDK tree is ours to fetch into; a path the user set is theirs.
if [ "${SDK_DIR}" = "${DEFAULT_SDK_DIR}" ]; then SDK_MANAGED=1; else SDK_MANAGED=0; fi

# --- output ------------------------------------------------------------------

if [ -t 2 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != dumb ]; then
  C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'
  C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_CYAN=$'\033[36m'
else
  C_RESET=""; C_BOLD=""; C_DIM=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_CYAN=""
fi
case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
  *UTF-8*|*utf8*|*UTF8*|*utf-8*) G_OK="✓"; G_BAD="✗"; G_WARN="!"; G_SKIP="–"; G_RULE="─"; G_DOT="·" ;;
  *) G_OK="ok"; G_BAD="xx"; G_WARN="!!"; G_SKIP="--"; G_RULE="-"; G_DOT="|" ;;
esac

say()   { printf '%s\n' "$*" >&2; }
die()   { printf '%s%s error%s %s\n' "${C_RED}" "${G_BAD}" "${C_RESET}" "$1" >&2; exit "${2:-1}"; }
warn()  { printf '%s%s%s %s\n' "${C_YELLOW}" "${G_WARN}" "${C_RESET}" "$1" >&2; }
rule()  { local line="" i; for ((i = 0; i < 64; i++)); do line+="${G_RULE}"; done; say " ${C_DIM}${line}${C_RESET}"; }
title() { say ""; say " ${C_BOLD}$1${C_RESET}"; }
field() { printf ' %s%-10s%s %s\n' "${C_DIM}" "$1" "${C_RESET}" "$2" >&2; }
# row <ok|bad|warn|skip> <name> <detail> [hint]
row() {
  local glyph colour
  case "$1" in
    ok)   glyph="${G_OK}";   colour="${C_GREEN}" ;;
    bad)  glyph="${G_BAD}";  colour="${C_RED}" ;;
    warn) glyph="${G_WARN}"; colour="${C_YELLOW}" ;;
    *)    glyph="${G_SKIP}"; colour="${C_DIM}" ;;
  esac
  printf '   %s%-2s%s %-15s %-16s %s%s%s\n' "${colour}" "${glyph}" "${C_RESET}" "$2" "$3" "${C_DIM}" "${4:-}" "${C_RESET}" >&2
}
log_step() { say ""; say "${C_CYAN}${C_BOLD}==>${C_RESET} ${C_BOLD}$*${C_RESET}"; }
log_ok()   { printf '   %s%s%s %s\n' "${C_GREEN}" "${G_OK}" "${C_RESET}" "$*" >&2; }
log_fail() { printf '   %s%s%s %s\n' "${C_RED}" "${G_BAD}" "${C_RESET}" "$*" >&2; }

have() { command -v "$1" >/dev/null 2>&1; }
first_version() { sed -nE 's/[^0-9]*([0-9]+(\.[0-9]+)+).*/\1/p' | head -1; }

version_at_least() {
  # version_at_least <have> <want>: true when have >= want (dotted numbers).
  [ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" = "$2" ]
}

# --- host detection ----------------------------------------------------------

detect_host() {
  local uname_s uname_m
  uname_s="$(uname -s)"; uname_m="$(uname -m)"
  OS_KIND=""; OS_NAME=""; CORES=""; MEM_GB=""; PKG=""
  case "${uname_s}" in
    Darwin)
      OS_KIND=macos
      OS_NAME="macOS $(sw_vers -productVersion 2>/dev/null || echo '?')"
      CORES="$(sysctl -n hw.logicalcpu)"
      MEM_GB="$(( $(sysctl -n hw.memsize) / 1073741824 ))"
      [ "${uname_m}" = arm64 ] || die "Intel Macs are not supported: the kit ships for macos-arm64 only" 2
      PLATFORM=macos-arm64
      ;;
    Linux)
      OS_KIND=linux
      # shellcheck disable=SC1091
      OS_NAME="$( (. /etc/os-release 2>/dev/null && printf '%s' "${PRETTY_NAME:-Linux}") || echo Linux)"
      CORES="$(nproc)"
      MEM_GB="$(awk '/MemTotal/ { printf "%d", $2 / 1048576 }' /proc/meminfo 2>/dev/null || echo '?')"
      case "${uname_m}" in aarch64|arm64) PLATFORM=linux-arm64 ;; *) PLATFORM=linux-x64 ;; esac
      ;;
    MINGW*|MSYS*|CYGWIN*)
      OS_KIND=windows
      OS_NAME="Windows (${uname_s%%_*} shell)"
      CORES="${NUMBER_OF_PROCESSORS:-4}"
      MEM_GB="?"
      case "${PROCESSOR_ARCHITECTURE:-${uname_m}}" in ARM64|aarch64) PLATFORM=windows-arm64 ;; *) PLATFORM=windows-x64 ;; esac
      ;;
    *) die "unsupported OS: ${uname_s}" 2 ;;
  esac
  ARCH="${uname_m}"
  JOBS="${WALLY_JOBS:-${CORES}}"

  if   have brew;    then PKG=brew
  elif have apt-get; then PKG=apt
  elif have dnf;     then PKG=dnf
  elif have pacman;  then PKG=pacman
  elif have winget;  then PKG=winget
  elif have choco;   then PKG=choco
  else PKG=none
  fi

  detect_apple_toolchain
}

# Xcode's linker cannot read a Command Line Tools SDK newer than itself (ld
# fails with "unknown architecture arm64e.x1"), so the SDK is pinned to the
# one that ships with the selected developer dir unless the caller chose one.
# The Swift MLX host needs a full Xcode; without one the host is turned off.
detect_apple_toolchain() {
  XCODE_VERSION=""; XCODE_SDK=""; CLT_SDK=""; SDK_NOTE=""; MLX_REASON=""
  if [ "${OS_KIND}" != macos ]; then
    MLX_HOST=OFF; MLX_REASON="no MLX on ${OS_KIND}"
    return 0
  fi
  XCODE_VERSION="$(xcodebuild -version 2>/dev/null | sed -n 's/^Xcode //p' || true)"
  if have xcrun; then
    XCODE_SDK="$(xcrun --sdk macosx --show-sdk-version 2>/dev/null || true)"
  fi
  if [ -d /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk ]; then
    CLT_SDK="$(basename "$(readlink /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk 2>/dev/null || echo MacOSX.sdk)" .sdk)"
    CLT_SDK="${CLT_SDK#MacOSX}"
  fi
  if [ -z "${SDKROOT:-}" ] && have xcrun; then
    SDKROOT="$(xcrun --sdk macosx --show-sdk-path 2>/dev/null || true)"
    if [ -n "${SDKROOT}" ]; then
      export SDKROOT
      if [ -n "${CLT_SDK}" ] && [ "${CLT_SDK}" != "${XCODE_SDK}" ]; then
        SDK_NOTE="CLT SDK ${CLT_SDK} is newer than Xcode's linker; pinned to SDK ${XCODE_SDK}"
      fi
    fi
  fi

  if [ -n "${WALLY_APPLE_MLX_HOST:-}" ]; then
    MLX_HOST="${WALLY_APPLE_MLX_HOST}"; MLX_REASON="set by WALLY_APPLE_MLX_HOST"
  elif [ -z "${XCODE_VERSION}" ]; then
    MLX_HOST=OFF; MLX_REASON="no full Xcode; building wally-cxx"
  elif ! have swift; then
    MLX_HOST=OFF; MLX_REASON="swift not on PATH; building wally-cxx"
  else
    MLX_HOST=ON; MLX_REASON="Xcode ${XCODE_VERSION} found"
  fi
}

# --- tool registry -------------------------------------------------------------

tool_version() {
  case "$1" in
    git)        git --version | first_version ;;
    cmake)      cmake --version | head -1 | first_version ;;
    ninja)      ninja --version | first_version ;;
    rustup)     rustup --version 2>/dev/null | head -1 | first_version ;;
    cargo)      cargo --version 2>/dev/null | first_version ;;
    python3)    python3 --version 2>&1 | first_version ;;
    curl)       curl --version | head -1 | first_version ;;
    tar)        tar --version 2>&1 | head -1 | first_version ;;
    gh)         gh --version | head -1 | first_version ;;
    xcrun)      xcrun --version | first_version ;;
    swift)      swift --version 2>&1 | sed -n 's/.*Swift version //p' | first_version ;;
    shellcheck) shellcheck --version | sed -n 2p | first_version ;;
    ruby)       ruby -v | first_version ;;
    bindgen)    bindgen --version 2>/dev/null | first_version ;;
    ccache)     ccache --version | head -1 | first_version ;;
    *)          echo present ;;
  esac
}

install_hint() {
  local tool="$1" pkg="$1"
  case "${tool}" in
    rustup|cargo) echo "curl -sSf https://sh.rustup.rs | sh"; return ;;
    bindgen)      echo "cargo install bindgen-cli --version ${BINDGEN_VERSION} --locked"; return ;;
    xcrun)        echo "xcode-select --install"; return ;;
    swift)        echo "install Xcode 26+, or set WALLY_APPLE_MLX_HOST=OFF"; return ;;
  esac
  case "${PKG}:${tool}" in
    apt:ninja)    pkg=ninja-build ;;
    dnf:ninja)    pkg=ninja-build ;;
    brew:python3) pkg=python ;;
    winget:*)     ;;
  esac
  case "${PKG}" in
    brew)   echo "brew install ${pkg}" ;;
    apt)    echo "sudo apt-get install -y ${pkg}" ;;
    dnf)    echo "sudo dnf install -y ${pkg}" ;;
    pacman) echo "sudo pacman -S ${pkg}" ;;
    winget) echo "winget install ${pkg}" ;;
    choco)  echo "choco install ${pkg}" ;;
    *)      echo "install ${pkg}" ;;
  esac
}

# What each command needs. "req" stops the run when missing; "opt" only
# narrows what the command can check.
requirements() {
  local mac_build=""
  if [ "${OS_KIND}" = macos ]; then
    mac_build="xcrun"
    [ "${MLX_HOST}" = OFF ] || mac_build="xcrun swift"
  fi
  case "$1" in
    kit)                 REQ="curl tar";                                          OPT="gh" ;;
    sdk)                 REQ="git";                                               OPT="" ;;
    configure|build)     REQ="git cmake ninja rustup cargo python3 ${mac_build}"; OPT="ccache" ;;
    test)                REQ="cmake python3";                                     OPT="" ;;
    lint)                REQ="cargo";                                             OPT="shellcheck dash ruby" ;;
    fix)                 REQ="cargo git python3";                                 OPT="shellcheck dash ruby bindgen" ;;
    check)               REQ="git python3";                                       OPT="dash" ;;
    bindings)            REQ="cargo";                                             OPT="bindgen" ;;
    e2e)                 REQ="python3";                                           OPT="" ;;
    *)                   REQ="";                                                  OPT="" ;;
  esac
}

# Expand group commands so the preflight sees every real step.
expand_commands() {
  local out="" c
  for c in "$@"; do
    case "${c}" in
      setup) out+=" kit sdk" ;;
      ci)    out+=" check build test lint" ;;
      all)   out+=" kit sdk check build test lint bindings" ;;
      deps)  out+=" kit sdk build test lint check bindings" ;;
      *)     out+=" ${c}" ;;
    esac
  done
  printf '%s\n' "${out}"
}

add_unique() {
  # add_unique <list> <word...>: print list with each word appended once.
  local list="$1" w; shift
  for w in "$@"; do
    case " ${list} " in *" ${w} "*) ;; *) list="${list} ${w}" ;; esac
  done
  printf '%s\n' "${list}"
}

# --- state checks used by the preflight ------------------------------------------

kit_state() {
  # Sets KIT_STATE (ready|stale|yours|absent|foreign) and KIT_DETAIL.
  local want; want="$(wally_kit_version)"
  if [ -f "${KIT_DIR}/${KIT_STAMP}" ]; then
    if [ "$(cat "${KIT_DIR}/${KIT_STAMP}")" = "${want}" ] && [ -f "${KIT_DIR}/share/runanywhere/SCHEMA_LOCK" ]; then
      KIT_STATE=ready;  KIT_DETAIL="${want}, downloaded by actor.sh"
    else
      KIT_STATE=stale;  KIT_DETAIL="$(cat "${KIT_DIR}/${KIT_STAMP}") on disk, pin wants ${want}"
    fi
  elif [ -f "${KIT_DIR}/share/runanywhere/SCHEMA_LOCK" ]; then
    KIT_STATE=yours;    KIT_DETAIL="your kit, used as is (configure checks its SCHEMA_LOCK)"
  elif [ ! -e "${KIT_DIR}" ] || [ -z "$(ls -A "${KIT_DIR}" 2>/dev/null)" ]; then
    KIT_STATE=absent;   KIT_DETAIL="not downloaded yet; \`kit\` fetches ${want}"
  else
    KIT_STATE=foreign;  KIT_DETAIL="not a kit and not empty; actor.sh will not touch it"
  fi
}

sdk_state() {
  # Sets SDK_STATE (pinned|moved|absent|notsdk|off) and SDK_DETAIL.
  local ref head
  if [ "${MLX_HOST}" = OFF ]; then SDK_STATE=off; SDK_DETAIL="not needed (${MLX_REASON})"; return; fi
  ref="$(wally_pin sdk_ci_ref)"
  if [ ! -e "${SDK_DIR}" ]; then
    SDK_STATE=absent; SDK_DETAIL="not checked out; \`sdk\` fetches ${ref:0:12}"
  elif [ ! -f "${SDK_DIR}/Package.swift" ] || ! git -C "${SDK_DIR}" rev-parse HEAD >/dev/null 2>&1; then
    SDK_STATE=notsdk; SDK_DETAIL="not a runanywhere-sdks git checkout"
  else
    head="$(git -C "${SDK_DIR}" rev-parse HEAD)"
    if [ "${head}" = "${ref}" ]; then
      SDK_STATE=pinned; SDK_DETAIL="at ${ref:0:12}"
    else
      SDK_STATE=moved;  SDK_DETAIL="at ${head:0:12}, CI uses ${ref:0:12}"
    fi
  fi
  if [ "${SDK_MANAGED}" = 1 ]; then SDK_DETAIL="${SDK_DETAIL} (managed)"; else SDK_DETAIL="${SDK_DETAIL} (yours)"; fi
}

console_state() {
  # Sets CONSOLE_STATE (ok|optional|missing) and CONSOLE_DETAIL.
  local key value found=0 source=env
  if [ "${WALLY_CONSOLE_OPTIONAL:-}" = 1 ]; then
    CONSOLE_STATE=optional; CONSOLE_DETAIL="WALLY_CONSOLE_OPTIONAL=1, no default console"; return
  fi
  for key in WALLY_BAKED_CONSOLE_API_URL WALLY_BAKED_CONSOLE_WEB_ORIGIN; do
    value="${!key:-}"
    if [ -z "${value}" ] && [ -f .env ]; then
      value="$(sed -n "s/^${key}=//p" .env | head -1)"
      [ -z "${value}" ] || source=.env
    fi
    [ -z "${value}" ] || found=$((found + 1))
  done
  if [ "${found}" -eq 2 ]; then
    CONSOLE_STATE=ok; CONSOLE_DETAIL="both endpoints set (${source}), values withheld"
  else
    CONSOLE_STATE=missing; CONSOLE_DETAIL="cp .env.example .env and fill in both WALLY_BAKED_CONSOLE_* values"
  fi
}

# --- preflight -----------------------------------------------------------------

# preflight <requested commands...>: print the host and requirement report for
# exactly what will run, then stop with exit 2 if anything required is missing.
preflight() {
  local steps req="" opt="" s tool v missing=0 needs_build=0 needs_kit=0 needs_sdk=0
  # shellcheck disable=SC2046
  steps="$(expand_commands "$@")"
  for s in ${steps}; do
    requirements "${s}"
    # shellcheck disable=SC2086
    req="$(add_unique "${req}" ${REQ})"
    # shellcheck disable=SC2086
    opt="$(add_unique "${opt}" ${OPT})"
    case "${s}" in
      configure|build) needs_build=1; needs_kit=1; needs_sdk=1 ;;
      kit|test|e2e|bindings) needs_kit=1 ;;
      sdk) needs_sdk=1 ;;
    esac
  done

  say ""
  say " ${C_BOLD}wally actor${C_RESET} ${C_DIM}${G_DOT} $*${C_RESET}"
  rule
  field host "${OS_NAME} ${G_DOT} ${ARCH} ${G_DOT} ${CORES} cores ${G_DOT} ${MEM_GB} GB ${G_DOT} pkg: ${PKG}"
  field kit-tag "${PLATFORM} ${G_DOT} kit $(wally_kit_version) ${G_DOT} wally $(wally_version)"
  if [ "${OS_KIND}" = macos ]; then
    field xcode "${XCODE_VERSION:-none} ${G_DOT} SDK ${XCODE_SDK:-?} ${G_DOT} CLT SDK ${CLT_SDK:-none}"
    [ -z "${SDK_NOTE}" ] || field "" "${C_YELLOW}${SDK_NOTE}${C_RESET}"
  fi
  field mlx-host "${MLX_HOST} ${C_DIM}(${MLX_REASON})${C_RESET}"
  field build "${BUILD_TYPE} ${G_DOT} ${JOBS} jobs ${G_DOT} ${BUILD_DIR#"${ROOT}"/}"

  if [ -n "${req// /}" ]; then
    title required
    for tool in ${req}; do
      if have "${tool}"; then
        v="$(tool_version "${tool}" 2>/dev/null || true)"
        case "${tool}" in
          cmake)
            if version_at_least "${v}" "$(wally_pin cmake_minimum)"; then row ok cmake "${v}" "needs $(wally_pin cmake_minimum)+"
            else row bad cmake "${v}" "needs $(wally_pin cmake_minimum)+: $(install_hint cmake)"; missing=1; fi ;;
          rustup)
            row ok rustup "${v}"
            if rustup toolchain list 2>/dev/null | grep -q "^$(wally_pin rust)"; then
              row ok "rust $(wally_pin rust)" "installed" "rust-toolchain.toml"
            else
              row bad "rust $(wally_pin rust)" "missing" "rustup toolchain install $(wally_pin rust) --component rustfmt,clippy"; missing=1
            fi ;;
          *) row ok "${tool}" "${v:-present}" ;;
        esac
      else
        row bad "${tool}" "missing" "$(install_hint "${tool}")"; missing=1
      fi
    done
  fi

  if [ -n "${opt// /}" ]; then
    title optional
    for tool in ${opt}; do
      if ! have "${tool}"; then
        row skip "${tool}" "missing" "$(install_hint "${tool}")"
        continue
      fi
      v="$(tool_version "${tool}" 2>/dev/null || true)"
      case "${tool}" in
        gh)
          if gh auth status >/dev/null 2>&1; then row ok gh "${v}" "signed in, fallback download"
          else row skip gh "${v}" "not signed in; only a download fallback"; fi ;;
        bindgen)
          if [ "${v}" = "${BINDGEN_VERSION}" ]; then row ok bindgen "${v}"
          else row warn bindgen "${v}" "bindings needs ${BINDGEN_VERSION}: $(install_hint bindgen)"; fi ;;
        *) row ok "${tool}" "${v:-present}" ;;
      esac
    done
  fi

  if [ $((needs_kit + needs_sdk + needs_build)) -gt 0 ]; then
    title inputs
    if [ "${needs_kit}" = 1 ]; then
      kit_state
      case "${KIT_STATE}" in
        ready|yours) row ok kit "${KIT_STATE}" "${KIT_DETAIL}" ;;
        stale|absent) row warn kit "${KIT_STATE}" "${KIT_DETAIL}" ;;
        foreign) row bad kit "${KIT_STATE}" "${KIT_DIR}: ${KIT_DETAIL}"; missing=1 ;;
      esac
    fi
    if [ "${needs_sdk}" = 1 ]; then
      sdk_state
      case "${SDK_STATE}" in
        pinned) row ok swift-sdk "${SDK_STATE}" "${SDK_DETAIL}" ;;
        off) row skip swift-sdk "${SDK_STATE}" "${SDK_DETAIL}" ;;
        absent|moved) row warn swift-sdk "${SDK_STATE}" "${SDK_DETAIL}" ;;
        notsdk) row bad swift-sdk "${SDK_STATE}" "${SDK_DIR}: ${SDK_DETAIL}"; missing=1 ;;
      esac
    fi
    if [ "${needs_build}" = 1 ]; then
      console_state
      case "${CONSOLE_STATE}" in
        ok) row ok console "set" "${CONSOLE_DETAIL}" ;;
        optional) row warn console "optional" "${CONSOLE_DETAIL}" ;;
        missing) row bad console "missing" "${CONSOLE_DETAIL}"; missing=1 ;;
      esac
    fi
  fi
  rule

  if [ "${missing}" -ne 0 ]; then
    die "fix the ${G_BAD} rows above, then run again" 2
  fi
}

# --- step bookkeeping ------------------------------------------------------------

# Steps inside one command keep going after a failure so a single run reports
# every broken check; the command fails at the end if any step did.
FAILED=()
run_step() {
  local name="$1"; shift
  log_step "${name}"
  if "$@"; then log_ok "${name}"; else log_fail "${name}"; FAILED+=("${name}"); fi
}
finish_steps() {
  local what="$1" name
  if [ "${#FAILED[@]}" -ne 0 ]; then
    say ""
    say "${C_RED}${C_BOLD}${what} failed:${C_RESET}"
    for name in "${FAILED[@]}"; do log_fail "${name}"; done
    FAILED=()
    return 1
  fi
}

# Product binary for this host: the Swift MLX host on Apple, wally-cxx when the
# host is off, wally.exe on Windows.
product_binary() {
  case "${OS_KIND}" in
    macos)   if [ "${MLX_HOST}" = OFF ]; then echo "${BUILD_DIR}/wally-cxx"; else echo "${BUILD_DIR}/wally"; fi ;;
    windows) echo "${BUILD_DIR}/wally.exe" ;;
    *)       echo "${BUILD_DIR}/wally" ;;
  esac
}

# --- downloads -----------------------------------------------------------------

# The kit downloads into a temporary sibling and replaces KIT_DIR only after
# it verifies. Only a directory that is empty or carries actor.sh's stamp is
# ever replaced; a kit you built or unpacked yourself is used as is.
cmd_kit() {
  local want tmp
  want="$(wally_kit_version)"
  kit_state
  case "${KIT_STATE}" in
    ready)   log_ok "kit ${want} at ${KIT_DIR}"; return 0 ;;
    yours)   log_ok "using your kit at ${KIT_DIR} as is"; return 0 ;;
    foreign) die "${KIT_DIR} is not an actor.sh kit and is not empty; remove it yourself or pick another WALLY_KIT_DIR" 2 ;;
  esac

  log_step "download kit ${want} (${PLATFORM})"
  mkdir -p "$(dirname "${KIT_DIR}")"
  tmp="${KIT_DIR}.download.$$"
  rm -rf "${tmp}"
  # shellcheck disable=SC2064
  trap "rm -rf '${tmp}'" EXIT
  bash scripts/build/fetch-kit.sh "${PLATFORM}" "${tmp}"
  [ -f "${tmp}/include/runanywhere/proto/model_types.pb.h" ] || die "downloaded kit has no generated proto headers"
  [ -f "${tmp}/share/runanywhere/SCHEMA_LOCK" ] || die "downloaded kit has no share/runanywhere/SCHEMA_LOCK"
  printf '%s\n' "${want}" > "${tmp}/${KIT_STAMP}"

  # Re-check right before replacing: only our own stamped or empty dir goes.
  kit_state
  case "${KIT_STATE}" in
    stale|absent) rm -rf "${KIT_DIR}" ;;
    *) die "${KIT_DIR} changed during the download; left it alone (new kit at ${tmp})" ;;
  esac
  mv "${tmp}" "${KIT_DIR}"
  trap - EXIT
  log_ok "kit ${want} at ${KIT_DIR}"
}

# The default tree is checked out at the exact sdk_ci_ref CI uses, so the MLX
# host links the Swift code a release ships. A tree you point
# WALLY_SDK_SWIFT_PATH at is your working clone: it is checked, never moved.
cmd_sdk() {
  local ref
  sdk_state
  case "${SDK_STATE}" in
    off)    log_ok "Swift SDK ${SDK_DETAIL}"; return 0 ;;
    pinned) log_ok "runanywhere-sdks ${SDK_DETAIL}"; return 0 ;;
    notsdk) die "${SDK_DIR} is ${SDK_DETAIL}; set WALLY_SDK_SWIFT_PATH to a runanywhere-sdks clone" 2 ;;
  esac
  ref="$(wally_pin sdk_ci_ref)"
  if [ "${SDK_MANAGED}" = 0 ]; then
    if [ "${SDK_STATE}" = absent ]; then
      die "WALLY_SDK_SWIFT_PATH=${SDK_DIR} does not exist; clone runanywhere-sdks there, or unset it to use ${DEFAULT_SDK_DIR}" 2
    fi
    warn "your runanywhere-sdks is ${SDK_DETAIL}; building with it as is. To match CI:"
    say  "     git -C ${SDK_DIR} fetch origin ${ref} && git -C ${SDK_DIR} checkout ${ref}"
    return 0
  fi
  log_step "check out runanywhere-sdks@${ref:0:12}"
  if [ ! -d "${SDK_DIR}/.git" ]; then
    mkdir -p "${SDK_DIR}"
    git -C "${SDK_DIR}" init -q
    git -C "${SDK_DIR}" remote add origin "${SDK_REMOTE}"
  fi
  git -C "${SDK_DIR}" fetch -q --depth 1 origin "${ref}"
  git -C "${SDK_DIR}" checkout -q --detach FETCH_HEAD
  log_ok "runanywhere-sdks at ${ref:0:12} (${SDK_DIR#"${ROOT}"/})"
}

# --- build ---------------------------------------------------------------------

cmd_configure() {
  [ -f "${KIT_DIR}/share/runanywhere/SCHEMA_LOCK" ] || die "no kit at ${KIT_DIR}; run ./actor.sh kit" 2
  local args=(-B "${BUILD_DIR}" -G Ninja -DCMAKE_BUILD_TYPE="${BUILD_TYPE}" -DCMAKE_PREFIX_PATH="${KIT_DIR}")
  if [ "${OS_KIND}" = macos ]; then
    [ -z "${SDKROOT:-}" ] || args+=(-DCMAKE_OSX_SYSROOT="${SDKROOT}")
    if [ "${MLX_HOST}" = OFF ]; then
      args+=(-DWALLY_APPLE_MLX_HOST=OFF -DWALLY_APPLE_CXX_BINARY=ON)
    else
      export WALLY_SDK_SWIFT_PATH="${SDK_DIR}"
    fi
  fi
  if have ccache; then args+=(-DCMAKE_CXX_COMPILER_LAUNCHER=ccache); fi
  log_step "configure ${BUILD_TYPE} -> ${BUILD_DIR#"${ROOT}"/}"
  cmake "${args[@]}"
}

cmd_build() {
  # build-mlx.sh reads WALLY_SDK_SWIFT_PATH and SDKROOT at build time, not
  # only at configure time, so both stay exported for the build step.
  if [ "${MLX_HOST}" != OFF ]; then
    cmd_sdk
    export WALLY_SDK_SWIFT_PATH="${SDK_DIR}"
  fi
  [ -f "${BUILD_DIR}/build.ninja" ] || cmd_configure
  log_step "build (${JOBS} jobs)"
  cmake --build "${BUILD_DIR}" -j "${JOBS}"
  log_ok "$("$(product_binary)" --version) at $(product_binary)"
}

cmd_clean() {
  log_step "remove ${BUILD_DIR}"
  rm -rf "${BUILD_DIR}"
}

# --- tests ---------------------------------------------------------------------

cmd_test() {
  [ -f "${BUILD_DIR}/build.ninja" ] || die "not configured; run ./actor.sh build" 2
  # Serial, like CI, so a parallel-only flake cannot look like a local failure.
  run_step "ctest (cargo test)" ctest --test-dir "${BUILD_DIR}" --output-on-failure
  run_step "update download failures" python3 scripts/test/test-update-download.py "$(product_binary)"
  finish_steps test
}

cmd_e2e() {
  local bin; bin="$(product_binary)"
  [ -x "${bin}" ] || die "no ${bin}; run ./actor.sh build" 2
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
  run_step "cargo fmt --check" cargo fmt --check
  if [ -f "${BUILD_DIR}/generated/wally-build.env" ]; then
    run_step "cargo clippy -D warnings" clippy
  else
    warn "clippy skipped: it needs a configured build; run ./actor.sh build first"
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

# --- fix -----------------------------------------------------------------------

regen_bindings() {
  python3 contracts/generate_console_binding.py \
    && python3 contracts/generate_console_binding.py \
         --contract contracts/wally-decisions-public-v1.openapi.json \
         --output src/account/decisions_contract.rs \
         --operation-id createDecisions
}

shellcheck_fix() {
  # The diff format patches only simple rewrites; anything it cannot patch is
  # still reported by the lint pass that follows.
  local patch
  patch="$(shellcheck -f diff -s sh install.sh 2>/dev/null; shellcheck -f diff -x actor.sh 2>/dev/null)" || true
  if [ -z "${patch}" ]; then return 0; fi
  printf '%s\n' "${patch}" | git apply --whitespace=nowarn -
}

# Runs every fixer that rewrites files mechanically, then lint, so only what a
# person has to fix is reported. Version pins, the test ledger, retired model
# ids and the install tests need a real change and are left alone.
cmd_fix() {
  run_step "cargo fmt" cargo fmt
  if [ -f "${BUILD_DIR}/generated/wally-build.env" ]; then
    run_step "cargo clippy --fix" env \
      WALLY_BUILD_ENV="${BUILD_DIR}/generated/wally-build.env" \
      CARGO_TARGET_DIR="${BUILD_DIR}/cargo" \
      cargo clippy --fix --allow-dirty --allow-staged --all-targets
    run_step "cargo fmt (after clippy)" cargo fmt
  else
    warn "clippy --fix skipped: it needs a configured build; run ./actor.sh build first"
  fi
  run_step "CLAUDE.md and skills mirror" bash scripts/ci/check-agents-sync.sh --fix
  run_step "console and decisions bindings" regen_bindings
  if [ "${OS_KIND}" = macos ] && have bindgen && [ "$(tool_version bindgen)" = "${BINDGEN_VERSION}" ] \
     && [ -f "${KIT_DIR}/share/runanywhere/SCHEMA_LOCK" ]; then
    run_step "src/sys/bindings.rs" env WALLY_SDK_KIT="${KIT_DIR}" bash scripts/build/gen-sys-bindings.sh
  fi
  if have shellcheck; then run_step "shellcheck auto-fixes" shellcheck_fix; fi
  finish_steps fix

  log_step "what is left to fix by hand"
  cmd_lint
  log_ok "files changed in the tree: $(git status --short | wc -l | tr -d ' ') (git diff to review)"
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
  run_step "agents sync and actor.sh tests" python3 tests/test_agents_sync.py
  if [ "${OS_KIND}" != windows ]; then run_step "install.sh tests" install_tests; fi
  finish_steps check
}

# Needs bindgen and the kit headers; macOS only, like CI.
cmd_bindings() {
  if [ "${OS_KIND}" != macos ]; then log_ok "bindings check runs on macOS only"; return 0; fi
  if ! { have bindgen && [ "$(tool_version bindgen)" = "${BINDGEN_VERSION}" ]; }; then
    die "bindgen ${BINDGEN_VERSION} missing: $(install_hint bindgen)" 2
  fi
  WALLY_SDK_KIT="${KIT_DIR}" bash scripts/build/gen-sys-bindings.sh --check
}

# --- groups --------------------------------------------------------------------

cmd_deps()  { log_ok "every requirement above is met"; }
cmd_setup() { cmd_kit; cmd_sdk; }
cmd_ci()    { cmd_check; cmd_build; cmd_test; cmd_lint; }
cmd_all()   { cmd_setup; cmd_ci; cmd_bindings; }

usage() {
  cat <<EOF
actor.sh — build, test, lint and check wally locally, the way CI does.

Usage: ./actor.sh <command> [command ...]

Every run starts with a preflight: OS, toolchain, and each tool the
requested commands need. A missing required tool stops the run first.

Setup
  deps       show the full preflight for every command, change nothing
  kit        download the pinned C++ kit (skips if current; never deletes yours)
  sdk        check out runanywhere-sdks at sdk_ci_ref (macOS MLX host only)
  setup      kit + sdk

Build
  configure  cmake configure against the kit (pins the Xcode SDK on macOS)
  build      configure if needed, build, print the binary's version
  clean      delete the build tree

Verify
  test       ctest (cargo test) and the update-download check
  lint       cargo fmt, clippy, shellcheck, shell syntax, AGENTS.md sync
  fix        apply every automatic fix, then lint to show what is left
  check      hermetic CI checks: contract, versions, ledger, install tests
  bindings   src/sys/bindings.rs matches the kit (macOS, needs bindgen)
  e2e        product e2e against the kit (scripts/test/e2e.sh)

Groups
  ci         check + build + test + lint
  all        setup + ci + bindings

Kit:   ${KIT_DIR}
Build: ${BUILD_DIR}
EOF
}

# --- main ----------------------------------------------------------------------

[ "$#" -gt 0 ] || { usage; exit 2; }

for command in "$@"; do
  case "${command}" in
    deps|kit|sdk|setup|configure|build|clean|test|lint|fix|check|bindings|e2e|ci|all) ;;
    -h|--help|help) usage; exit 0 ;;
    *) printf 'error: unknown command: %s\n\n' "${command}" >&2; usage >&2; exit 2 ;;
  esac
done

detect_host
case " $* " in
  " clean ") ;;
  *) preflight "$@" ;;
esac

for command in "$@"; do
  "cmd_${command}"
done
say ""
log_ok "${C_BOLD}done:${C_RESET} $*"
