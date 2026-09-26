#!/bin/sh
# POSIX sh, not bash: this is served as `curl ... | sh`, and sh is dash on
# Debian and Ubuntu, which has no [[ ]], no ${var:offset:length} and (before
# 0.5.13) no pipefail. With no pipefail a pipeline's status is its last
# command's, so every download whose failure matters is its own step below.
set -eu

# Installs Wally from the GitHub release tarball for this OS. No Homebrew and no
# tap: the release bottle already stages `wally` with mlx-swift_Cmlx.bundle
# beside it (Metal shaders) and its shared libraries under ../lib with an rpath
# that finds them, so a plain extract-and-symlink keeps every engine working.
#
# Usage:
#   curl -fsSL <install.sh> | sh
#
# Two overrides, for release tests and mirrors rather than everyday use:
#   WALLY_INSTALL_VERSION=X.Y.Z   install that release instead of the latest
#   WALLY_INSTALL_BASE_URL=<url>  fetch the archive and its .sha256 from <url>/
#                                 instead of the GitHub release (http, https or
#                                 file); needs WALLY_INSTALL_VERSION
#
# Everything runs inside main(), called on the last line. A download cut off
# part way is then a function that was never called, not half an installer run.
REPO="RunanywhereAI/wally"
LIB_DIR="${HOME}/.local/lib/wally"
BIN_DIR="${HOME}/.local/bin"

# The oldest glibc the Linux bottle runs on, and the shared libraries it takes
# from the system rather than shipping. Both mirror versions.toml [linux_abi]
# (glibc_max, system_libraries); scripts/ci/check-versions.py fails when they
# drift. libc's own family is left out: glibc is checked by version above.
MIN_GLIBC="2.35"
LINUX_SYSTEM_LIBRARIES="libstdc++.so.6 libgcc_s.so.1 libssl.so.3 libcrypto.so.3 libcurl.so.4"

# The highest GLIBCXX_/CXXABI_ symbol version the bottle's own ELF files ask
# of libstdc++.so.6, mirroring versions.toml [linux_abi] (glibcxx_max,
# cxxabi_max) the same way MIN_GLIBC mirrors glibc_max --
# scripts/release/check-linux-abi.py computes the real values from the built
# bottle and fails the release if they exceed these; scripts/ci/check-versions.py
# fails if these two drift from versions.toml. Checking libstdc++'s own symbol
# versions (not just its presence, which check_linux_system already does)
# catches a glibc-2.35 host whose libstdc++ is otherwise too old: v0.6.0's
# bottle needed GLIBCXX_3.4.32, which a stock 22.04 libstdc++ (3.4.30) does not
# have, and the installer downloaded before finding that out.
MIN_GLIBCXX="3.4.30"
MIN_CXXABI="1.3.13"

# Retries cover a dropped connection or a 5xx from the CDN, which a first-time
# install on a poor network otherwise reports as "check your internet".
CURL_RETRY="--retry 3 --retry-delay 2"

# --- output helpers ---------------------------------------------------------
if [ -t 1 ]; then B=$(printf '\033[1m'); DIM=$(printf '\033[2m'); R=$(printf '\033[0m')
  BLU=$(printf '\033[34m'); GRN=$(printf '\033[32m'); YEL=$(printf '\033[33m'); RED=$(printf '\033[31m')
else B=""; DIM=""; R=""; BLU=""; GRN=""; YEL=""; RED=""; fi

STEP=0
TOTAL=5
step()  { STEP=$((STEP + 1)); printf "%s[%d/%d]%s %s%s%s\n" "$BLU" "$STEP" "$TOTAL" "$R" "$B" "$*" "$R"; }
ok()    { printf "      %s✓%s %s\n" "$GRN" "$R" "$*"; }
warn()  { printf "      %s!%s %s\n" "$YEL" "$R" "$*"; }
fail()  { printf "%serror:%s %s\n" "$RED" "$R" "$*" >&2; exit 1; }

banner() {
  printf '\n'
  printf '   %s┌───────────────────────────────┐%s\n' "$DIM" "$R"
  printf '   %s│%s   %s● Wally%s  · RunAnywhere CLI   %s│%s\n' "$DIM" "$R" "$B" "$R" "$DIM" "$R"
  printf '   %s└───────────────────────────────┘%s\n' "$DIM" "$R"
}

# Which agent homes get the skill. Claude Code reads ~/.claude/skills; Cursor,
# Codex and other AGENTS.md tools read ~/.agents/skills. Install into every home
# the person already has, so a Codex-only user is not handed a skill their agent
# never reads. A fresh machine with neither is a Claude-first get-started, so it
# defaults to ~/.claude. One dir per line; callers set IFS=newline to be safe
# with a $HOME that contains spaces.
skill_target_dirs() {
    targets=""
    [ -d "${HOME}/.claude" ] && targets="${targets}${HOME}/.claude/skills/runanywhere
"
    [ -d "${HOME}/.agents" ] && targets="${targets}${HOME}/.agents/skills/runanywhere
"
    [ -n "${targets}" ] || targets="${HOME}/.claude/skills/runanywhere
"
    printf '%s' "${targets}"
}

# Refuses a Linux system the bottle cannot run on, before anything is
# downloaded: musl, a glibc older than MIN_GLIBC, or a missing system library.
# Each reason names what would fix it, which the dynamic loader's own message
# never does.
check_linux_system() {
    # One path at a time: a single `ls` over both globs fails when either one
    # matches nothing, which hid Alpine's /lib/ld-musl-x86_64.so.1.
    musl=0
    for loader in /lib/ld-musl-* /usr/lib/ld-musl-*; do
        [ -e "$loader" ] && musl=1
    done
    if [ "$musl" -eq 1 ] && ! getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
        fail "Wally's Linux build needs glibc, and this system uses musl (Alpine and similar). Use a glibc distribution such as Ubuntu 22.04+ or Debian 12+."
    fi
    glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{ print $2 }')"
    if [ -z "$glibc" ]; then
        warn "could not read the glibc version; continuing"
    elif [ "$(printf '%s\n%s\n' "$MIN_GLIBC" "$glibc" | sort -V | head -n1)" != "$MIN_GLIBC" ]; then
        fail "Wally needs glibc ${MIN_GLIBC} or newer; this system has ${glibc}. Ubuntu 22.04+, Debian 12+ and other distributions from 2022 on qualify."
    fi
    ldconfig_bin="$(command -v ldconfig 2>/dev/null || true)"
    [ -n "$ldconfig_bin" ] || { [ -x /sbin/ldconfig ] && ldconfig_bin=/sbin/ldconfig; }
    if [ -z "$ldconfig_bin" ]; then
        warn "could not list system libraries (no ldconfig); continuing"
        return 0
    fi
    known="$("$ldconfig_bin" -p 2>/dev/null || true)"
    missing=""
    for library in $LINUX_SYSTEM_LIBRARIES; do
        printf '%s\n' "$known" | grep -q "^[[:space:]]*${library} " || missing="${missing} ${library}"
    done
    if [ -n "$missing" ]; then
        fail "Wally needs these system libraries, which are not installed:${missing}. On Ubuntu or Debian: sudo apt install libstdc++6 libssl3 libcurl4"
    fi
    check_libstdcxx_symbols
    ok "glibc ${glibc:-unknown}, system libraries present"
}

# libstdc++.so.6 being present (checked above) is not the same as it being new
# enough: a glibc-2.35 host can still carry a libstdc++ built before GCC 12, so
# it has the *soname* the bottle needs but not every GLIBCXX_/CXXABI_ symbol
# version in it. v0.6.0's bottle needed GLIBCXX_3.4.32; a stock 22.04
# libstdc++ (3.4.30) does not have it, and the installer found out only after
# downloading, from the loader's own error. Reads the versions libstdc++
# itself provides straight out of its string table (the same printable text
# the loader reads), the way `check_linux_system` already found the library's
# path via ldconfig. WALLY_LIBSTDCXX_SYMBOLS overrides the listing itself
# (one tag per line) rather than the path, so a test can feed a fixture
# listing without a real libstdc++ on disk.
check_libstdcxx_symbols() {
    if [ -n "${WALLY_LIBSTDCXX_SYMBOLS:-}" ]; then
        listing="$(cat "$WALLY_LIBSTDCXX_SYMBOLS" 2>/dev/null || true)"
    else
        lib="$(printf '%s\n' "$known" | awk '/^[[:space:]]*libstdc\+\+\.so\.6[[:space:]]/{print $NF; exit}')"
        if [ -z "$lib" ] || [ ! -r "$lib" ]; then
            warn "could not locate libstdc++.so.6 to check its symbol versions; continuing"
            return 0
        fi
        listing="$(grep -aoE '(GLIBCXX|CXXABI)_[0-9]+(\.[0-9]+)*' "$lib" 2>/dev/null || true)"
    fi
    if [ -z "$listing" ]; then
        warn "could not read libstdc++'s symbol versions; continuing"
        return 0
    fi
    max_glibcxx="$(printf '%s\n' "$listing" | grep '^GLIBCXX_' | sed 's/^GLIBCXX_//' | sort -V | tail -1)"
    max_cxxabi="$(printf '%s\n' "$listing" | grep '^CXXABI_' | sed 's/^CXXABI_//' | sort -V | tail -1)"
    if [ -n "$max_glibcxx" ] && [ "$(printf '%s\n%s\n' "$MIN_GLIBCXX" "$max_glibcxx" | sort -V | head -n1)" != "$MIN_GLIBCXX" ]; then
        fail "Wally needs a libstdc++ with GLIBCXX_${MIN_GLIBCXX} or newer (from GCC 12+); this system's libstdc++ only provides up to GLIBCXX_${max_glibcxx}. Ubuntu 22.04+, Debian 12+ and other distributions from 2022 on qualify."
    fi
    if [ -n "$max_cxxabi" ] && [ "$(printf '%s\n%s\n' "$MIN_CXXABI" "$max_cxxabi" | sort -V | head -n1)" != "$MIN_CXXABI" ]; then
        fail "Wally needs a libstdc++ with CXXABI_${MIN_CXXABI} or newer (from GCC 12+); this system's libstdc++ only provides up to CXXABI_${max_cxxabi}. Ubuntu 22.04+, Debian 12+ and other distributions from 2022 on qualify."
    fi
}

# Runs a binary once and keeps what it printed. A binary that cannot start
# (a missing shared library, a glibc older than it was built against) prints
# the loader's reason here, and that reason is the only useful thing to show:
# discarding it left "Installed Wally vunknown", which says nothing.
probe_binary() {
    binary="$1"
    probe_status=0
    probe_output="$("$binary" --version 2>&1)" || probe_status=$?
    if [ "$probe_status" -ne 0 ]; then
        printf '%s\n' "$probe_output" | head -5 >&2
        case "$probe_output" in
            *"cannot open shared object file"*)
                missing_lib="$(printf '%s\n' "$probe_output" \
                    | sed -nE 's/.*: ([^:]+): cannot open shared object file.*/\1/p' | head -1)"
                fail "wally cannot start: ${missing_lib:-a shared library} is not on this system and not in the download." ;;
            *"GLIBC"*"not found"*)
                fail "wally cannot start: it needs a newer glibc/libstdc++ than this system has ($(ldd --version 2>/dev/null | head -1))." ;;
            *)
                fail "wally cannot start (output above)." ;;
        esac
    fi
    printf '%s\n' "$probe_output" | sed -nE 's/^wally ([0-9]+\.[0-9]+\.[0-9]+).*/\1/p' | head -1
}

# The install swap renames ${LIB_DIR} aside, then renames the new tree into its
# place. If this run is leaving with the first rename done and the second not,
# put the old tree back so the existing `wally` keeps working.
restore_previous_install() {
    if [ -n "${retired:-}" ] && [ -d "$retired" ] && [ ! -e "$LIB_DIR" ]; then
        mv "$retired" "$LIB_DIR" 2>/dev/null || true
    fi
}

# One install at a time per ${LIB_DIR}. The recovery below renames and deletes
# ${LIB_DIR}.previous.* and ${LIB_DIR}.incoming.*, which a second run working at
# the same moment would own; the lock makes every leftover it finds dead.
#
# The lock is a *file* containing our pid, put in place with `ln`: link(2) is
# atomic and fails with the name already existing, so unlike `mkdir` followed
# by a separate write, there is no window where the lock exists but is still
# empty. A directory-plus-pid-file lock has exactly that window: a second run
# can see the freshly made directory, find no pid in it yet, decide the lock
# is stale, and delete out from under the first run. Writing the pid to a temp
# file before the `ln` means the lock's content is correct from the instant it
# has its final name.
acquire_install_lock() {
    lock="${LIB_DIR}.lock"
    pid_tmp="${lock}.${$}.tmp"
    printf '%s\n' "$$" > "$pid_tmp"
    if ! ln "$pid_tmp" "$lock" 2>/dev/null; then
        owner="$(cat "$lock" 2>/dev/null || true)"
        if [ -n "$owner" ] && kill -0 "$owner" 2>/dev/null; then
            rm -f "$pid_tmp"
            fail "another Wally install (pid ${owner}) is running; let it finish, then run this again."
        fi
        # Stale: its owner is gone. Replace it with our own lock in one step:
        # unlink the stale file, then link ours into place. If another run
        # wins that same race, our `ln` fails and we stop instead of both
        # runs believing they hold the lock.
        rm -f "$lock"
        if ! ln "$pid_tmp" "$lock" 2>/dev/null; then
            rm -f "$pid_tmp"
            fail "another Wally install took ${lock} just now; run this again once it finishes."
        fi
    fi
    rm -f "$pid_tmp"
    held_lock="$lock"
}

# Removes ${held_lock} only if it is still the file we created. A stale-lock
# recovery by another run (above) may have replaced it with its own after we
# finished acquiring; blindly removing whatever now has that name would let a
# third run start while the second is still mid-install.
release_install_lock() {
    [ -n "${held_lock:-}" ] || return 0
    [ "$(cat "$held_lock" 2>/dev/null || true)" = "$$" ] && rm -f "$held_lock"
    return 0
}

# A run killed outright between the two renames cannot clean up after itself:
# it leaves ${LIB_DIR}.previous.<pid> and no ${LIB_DIR}. Put that tree back
# before anything else, and drop leftovers from earlier runs, so every run
# starts from one install or none.
recover_interrupted_install() {
    for leftover in "${LIB_DIR}".previous.*; do
        [ -d "$leftover" ] || continue
        if [ ! -e "$LIB_DIR" ]; then
            mv "$leftover" "$LIB_DIR" && warn "restored the install an earlier run left half replaced"
        else
            rm -rf "$leftover"
        fi
    done
    for leftover in "${LIB_DIR}".incoming.*; do
        [ -d "$leftover" ] && rm -rf "$leftover"
    done
    return 0
}

main() {

# --- arguments --------------------------------------------------------------
# The version the caller already has, passed by `wally update` so the script can
# tell it apart from a fresh install and skip the download when nothing is newer.
CURRENT_VERSION=""
for arg in "$@"; do
    case "$arg" in
        nightly|--nightly) fail "nightly/dev installs are no longer published; this installer only supports production releases" ;;
        --version=*) CURRENT_VERSION="${arg#--version=}" ;;
        # Debug-only: print the resolved skill targets and exit before any
        # network work. Exercised by scripts/test/test-install-skill-dirs.sh.
        --print-skill-dirs) skill_target_dirs; exit 0 ;;
        # Debug-only: acquire the real install lock for ${HOME}, print
        # "acquired" once held, hold it for <seconds>, then release and exit
        # 0 -- or exit 1 with the normal contention message if another holder
        # is running. No download, no network. Exercised by
        # scripts/test/test-install-lock.sh to prove the lock is race-free.
        --hold-install-lock=*)
            mkdir -p "$(dirname "$LIB_DIR")"
            held_lock=""
            trap 'release_install_lock' EXIT
            acquire_install_lock
            ok "acquired"
            sleep "${arg#--hold-install-lock=}"
            exit 0 ;;
    esac
done

CHANNEL="production"

banner
printf '   %sInstalling the %s%s%s build%s\n\n' "$DIM" "$R$B" "$CHANNEL" "$R$DIM" "$R"

step "Resolving the latest release"
if [ -n "${WALLY_INSTALL_VERSION:-}" ]; then
    VERSION="${WALLY_INSTALL_VERSION#v}"
    printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
        || fail "WALLY_INSTALL_VERSION must look like 1.2.3, not '${WALLY_INSTALL_VERSION}'"
else
    [ -z "${WALLY_INSTALL_BASE_URL:-}" ] \
        || fail "WALLY_INSTALL_BASE_URL needs WALLY_INSTALL_VERSION: a mirror has no latest-release lookup"
    # shellcheck disable=SC2086 # CURL_RETRY is two options, split on purpose
    latest=$(curl -fsSL $CURL_RETRY "https://api.github.com/repos/${REPO}/releases/latest") \
        || fail "Could not determine latest release version. Check your internet connection."
    VERSION=$(printf '%s\n' "$latest" \
        | grep '"tag_name"' \
        | sed 's/.*"v\([^"]*\)".*/\1/')
fi
[ -n "$VERSION" ] || fail "Could not determine latest release version. Check your internet connection."
ok "v${VERSION}"

# An update check: the caller told us its version. If nothing newer is out,
# there is nothing to do -- say so and stop before downloading anything. The
# version-sorted higher of the two decides, so a build already ahead of the
# latest release (a dev build) is left alone rather than downgraded.
if [ -n "$CURRENT_VERSION" ]; then
    newest=$(printf '%s\n%s\n' "$CURRENT_VERSION" "$VERSION" | sort -V | tail -n1)
    if [ "$CURRENT_VERSION" = "$VERSION" ] || [ "$newest" = "$CURRENT_VERSION" ]; then
        ok "You already have the latest version (v${CURRENT_VERSION}) installed on this machine."
        exit 0
    fi
    printf '      updating v%s → v%s\n' "$CURRENT_VERSION" "$VERSION"
fi

os=$(uname -s)
arch=$(uname -m)
case "${os}/${arch}" in
    Darwin/arm64)              PLATFORM="macos-arm64" ;;
    # MLX is Metal and NeuRT is the Apple Neural Engine, so an Intel Mac gets
    # neither and there is no build for it.
    Darwin/*)                  fail "Wally needs an Apple Silicon Mac. Detected: ${arch}" ;;
    Linux/x86_64 | Linux/amd64) PLATFORM="linux-x86_64"; check_linux_system ;;
    Linux/*)                   fail "Wally has no Linux ${arch} build yet — x86_64 only. Build from source: https://github.com/${REPO}#build-from-source" ;;
    *)                         fail "Wally has no build for ${os}. On Windows, use install.ps1." ;;
esac
ok "${PLATFORM}"

ASSET="wally-${VERSION}-${PLATFORM}.tar.gz"
URL="${WALLY_INSTALL_BASE_URL:-https://github.com/${REPO}/releases/download/v${VERSION}}/${ASSET}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

step "Downloading ${ASSET}"
# A clean progress bar on a real terminal; silent (errors only) when the output
# is captured or piped, so a log does not fill with redraw frames.
if [ -t 1 ]; then dl="-#"; else dl="-sS"; fi
# shellcheck disable=SC2086 # CURL_RETRY is two options, split on purpose
curl -fSL $CURL_RETRY "$dl" "$URL" -o "${tmp}/${ASSET}" || fail "Download failed: ${URL}"
# shellcheck disable=SC2086
curl -fsSL $CURL_RETRY "${URL}.sha256" -o "${tmp}/${ASSET}.sha256" || fail "Could not download the checksum for ${ASSET}"
# The sidecar is `<sha>  <filename>`; verify from inside tmp so the name resolves.
expected_sha="$(awk 'NF == 2 { print $1 }' "${tmp}/${ASSET}.sha256" | head -1)"
( cd "$tmp" && {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "${ASSET}.sha256"
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "${ASSET}.sha256"
    else
        fail "Neither shasum nor sha256sum found on system to verify archive."
    fi
} >/dev/null 2>&1 ) || fail "Checksum verification failed for ${ASSET}. Do not use the download."
ok "sha256 $(printf '%.16s' "$expected_sha")… verified"

step "Installing to ${LIB_DIR}"
tar -xzf "${tmp}/${ASSET}" -C "$tmp"
staged="${tmp}/wally-${PLATFORM}"
[ -x "${staged}/bin/wally" ] || fail "Archive did not contain bin/wally as expected."

# The new tree is copied beside the old one, started once, and only then
# renamed into place. A build that cannot run on this machine therefore fails
# here with the loader's reason and leaves a working install untouched, and a
# run killed part way leaves either the old tree or the new one, never half of
# each. The copy is a fresh directory rather than an overwrite: replacing a
# code-signed Mach-O in place while a copy may still be mapped kills it with
# SIGKILL (137).
mkdir -p "$(dirname "$LIB_DIR")" "$BIN_DIR"
incoming="${LIB_DIR}.incoming.$$"
retired="${LIB_DIR}.previous.$$"
held_lock=""
# Between the two renames below there is no ${LIB_DIR}. An interrupted or
# failing run puts the previous tree back on the way out; a run killed outright
# (SIGKILL, power loss) is repaired by recover_interrupted_install next time.
trap 'restore_previous_install; rm -rf "$tmp" "$incoming"; release_install_lock' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
acquire_install_lock
recover_interrupted_install
rm -rf "$incoming"
cp -R "$staged" "$incoming"
staged_version="$(probe_binary "${incoming}/bin/wally")"
if [ "${staged_version}" != "${VERSION}" ]; then
    fail "The downloaded build reports v${staged_version:-unknown}, but the release is v${VERSION}."
fi
if [ -e "$LIB_DIR" ]; then
    mv "$LIB_DIR" "$retired"
fi
mv "$incoming" "$LIB_DIR" \
    || fail "Could not move the new build into ${LIB_DIR}; the previous install is unchanged."
rm -rf "$retired"
ln -sfn "${LIB_DIR}/bin/wally" "${BIN_DIR}/wally"

# Whether a future shell will find wally is decided by the PATH the user already
# had, so it is read before this script puts BIN_DIR on its own PATH. Testing
# afterwards always found BIN_DIR and so never wrote the startup file, and the
# install reported success while a fresh terminal could not run wally.
case ":${PATH}:" in
    *":${BIN_DIR}:"*) PATH_ALREADY_HAS_BIN_DIR=1 ;;
    *)                PATH_ALREADY_HAS_BIN_DIR=0 ;;
esac

export PATH="${BIN_DIR}:${PATH}"

if ! command -v wally >/dev/null 2>&1; then
    fail "Installation failed. wally not found after install."
fi
# What PATH resolves to may still be another copy of wally earlier on it.
installed_version="$(probe_binary "$(command -v wally)")"
if [ "${installed_version}" != "${VERSION}" ]; then
    fail "wally on PATH is v${installed_version:-unknown} ($(command -v wally)), not the v${VERSION} just installed in ${BIN_DIR}. Remove the other copy or put ${BIN_DIR} first on PATH."
fi
ok "wally v${VERSION} on PATH"

# Put BIN_DIR on PATH for future shells if the user's own PATH did not have it.
if [ "${PATH_ALREADY_HAS_BIN_DIR}" -eq 0 ]; then
    # $HOME is left unexpanded so the rc file keeps working if the home
    # directory ever moves; the user's shell expands it when it runs.
    case "$BIN_DIR" in
        "${HOME}/"*) path_entry="\$HOME/${BIN_DIR#"${HOME}"/}" ;;
        *)           path_entry="$BIN_DIR" ;;
    esac
    line="export PATH=\"${path_entry}:\$PATH\""
    case "$(basename "${SHELL:-}")" in
        zsh)  rc="${HOME}/.zshrc" ;;
        bash) rc="${HOME}/.bashrc" ;;
        *)    rc="${HOME}/.profile" ;;
    esac
    # Matching the directory rather than our exact line: somebody who added
    # ~/.local/bin to their own rc file by hand wrote it their own way, and
    # appending a second entry for a directory already on PATH helps nobody.
    if [ -f "$rc" ] && grep -q "$path_entry" "$rc" 2>/dev/null; then
        ok "${BIN_DIR} is already on PATH in ${rc} (open a new shell)"
    elif [ -e "$rc" ] && [ ! -f "$rc" ]; then
        # A directory or a device where the rc file should be. Nothing to append
        # to, and the shell's own redirection error would reach the terminal.
        warn "${rc} is not a regular file; add this line to your shell startup: ${line}"
    elif { [ -w "$rc" ] || [ ! -e "$rc" ]; } &&
        printf '\n# Added by the Wally installer\n%s\n' "$line" >> "$rc" 2>/dev/null; then
        warn "added ${BIN_DIR} to your PATH in ${rc} (open a new shell)"
    else
        # Saying "added" when the write failed is how somebody ends up with a
        # terminal that cannot find wally and no idea why.
        warn "could not write ${rc}; add this line to it yourself: ${line}"
    fi
fi

# The skill is what makes the next step self-explanatory in Claude Code: it
# teaches the assistant the commands, the harnesses, and what to do when one is
# missing. Pulled from the release tag, not from main, so an already-installed
# assistant cannot be changed by a push to main; it is the same tag the binary
# came from, so the two cannot drift.
step "Installing the RunAnywhere skill for your coding agent"
SKILL_URL="https://raw.githubusercontent.com/${REPO}/v${VERSION}/skills/runanywhere/SKILL.md"
skill_installed=0
old_ifs="$IFS"
IFS='
'
for skill_dir in $(skill_target_dirs); do
    IFS="$old_ifs"
    # shellcheck disable=SC2086 # CURL_RETRY is two options, split on purpose
    if mkdir -p "$skill_dir" 2>/dev/null && curl -fsSL $CURL_RETRY "$SKILL_URL" -o "${skill_dir}/SKILL.md"; then
        ok "${skill_dir}/SKILL.md"
        skill_installed=1
    else
        warn "could not install the skill at ${skill_dir}"
    fi
    IFS='
'
done
IFS="$old_ifs"
[ "$skill_installed" -eq 1 ] || warn "could not install the RunAnywhere skill. Everything else still works."

# Signing in is the point of the whole flow, so it happens here rather than
# being left as an instruction the person has to notice. Already signed in is a
# no-op, and a failure is not fatal: the CLI is installed either way.
step "Signing in"
if wally account whoami >/dev/null 2>&1; then
    ok "already signed in"
elif [ ! -t 0 ] || [ ! -t 1 ]; then
    # No terminal: piped into bash over SSH, or a CI step. The browser flow
    # would try to open a browser that is not there and then block until the
    # request expires, which reads as the installer hanging.
    warn "not an interactive terminal — run \`wally account login\` yourself"
else
    wally account login || warn "sign-in did not finish. Run \`wally account login\` when you are ready."
fi

# --- summary ----------------------------------------------------------------
printf '\n   %s┌─ Installed ───────────────────────────────%s\n' "$DIM" "$R"
printf '   %s│%s  wally     %sv%s%s\n'   "$DIM" "$R" "$B" "${VERSION}" "$R"
printf '   %s│%s  channel   %s\n'        "$DIM" "$R" "${CHANNEL}"
printf '   %s│%s  binary    %s\n'        "$DIM" "$R" "${BIN_DIR}/wally"
printf '   %s│%s  models    ~/.local/share/runanywhere\n' "$DIM" "$R"
printf '   %s└───────────────────────────────────────────%s\n\n' "$DIM" "$R"

printf '   %sNext:%s\n' "$B" "$R"
printf '     wally opencode --cloud -m glm-5.3-flash   code against a hosted model\n'
printf '     wally account usage                       credit left and what you spent\n'
printf '     wally models pull qwen3-0.6b              download a model to this machine\n'
printf '   In Claude Code, ask: %s"get me started with RunAnywhere Wally"%s\n\n' "$DIM" "$R"
}

main "$@"
