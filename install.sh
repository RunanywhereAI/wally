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
#   curl -fsSL <install.sh> | sh                 # production build
#   curl -fsSL <install.sh> | sh -s -- nightly   # nightly (dev-endpoint) build
#
# `nightly` (or --nightly) installs the -dev bottle, which is baked to talk to
# the development console and APIs. Same binary otherwise; it only changes which
# backend it points at, so it does not disturb the production install path.
REPO="RunanywhereAI/wally"
LIB_DIR="${HOME}/.local/lib/wally"
BIN_DIR="${HOME}/.local/bin"

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

# --- arguments --------------------------------------------------------------
NIGHTLY=0
# The version the caller already has, passed by `wally update` so the script can
# tell it apart from a fresh install and skip the download when nothing is newer.
CURRENT_VERSION=""
for arg in "$@"; do
    case "$arg" in
        nightly|--nightly) NIGHTLY=1 ;;
        --version=*) CURRENT_VERSION="${arg#--version=}" ;;
        # Debug-only: print the resolved skill targets and exit before any
        # network work. Exercised by scripts/test/test-install-skill-dirs.sh.
        --print-skill-dirs) skill_target_dirs; exit 0 ;;
    esac
done

if [ "$NIGHTLY" = 1 ]; then
    SUFFIX="-dev"; CHANNEL="nightly (development endpoints)"
else
    SUFFIX="";     CHANNEL="production"
fi

banner
printf '   %sInstalling the %s%s%s build%s\n\n' "$DIM" "$R$B" "$CHANNEL" "$R$DIM" "$R"

step "Resolving the latest release"
latest=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest") \
    || fail "Could not determine latest release version. Check your internet connection."
VERSION=$(printf '%s\n' "$latest" \
    | grep '"tag_name"' \
    | sed 's/.*"v\([^"]*\)".*/\1/')
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
    Linux/x86_64 | Linux/amd64) PLATFORM="linux-x86_64" ;;
    Linux/*)                   fail "Wally has no Linux ${arch} build yet — x86_64 only. Build from source: https://github.com/${REPO}#build-from-source" ;;
    *)                         fail "Wally has no build for ${os}. On Windows, use install.ps1." ;;
esac
ok "${PLATFORM}"

ASSET="wally-${VERSION}-${PLATFORM}${SUFFIX}.tar.gz"
URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ASSET}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

step "Downloading ${ASSET}"
# A clean progress bar on a real terminal; silent (errors only) when the output
# is captured or piped, so a log does not fill with redraw frames.
if [ -t 1 ]; then dl="-#"; else dl="-sS"; fi
curl -fSL "$dl" "$URL" -o "${tmp}/${ASSET}" || fail "Download failed: ${URL}"
curl -fsSL "${URL}.sha256" -o "${tmp}/${ASSET}.sha256" || fail "Could not download the checksum for ${ASSET}"
# The sidecar is `<sha>  <filename>`; verify from inside tmp so the name resolves.
expected_sha="$(awk 'NF == 2 { print $1 }' "${tmp}/${ASSET}.sha256" | head -1)"
( cd "$tmp" && shasum -a 256 -c "${ASSET}.sha256" >/dev/null 2>&1 ) \
    || fail "Checksum verification failed for ${ASSET}. Do not use the download."
ok "sha256 $(printf '%.16s' "$expected_sha")… verified"

step "Installing to ${LIB_DIR}"
tar -xzf "${tmp}/${ASSET}" -C "$tmp"
staged="${tmp}/wally-${PLATFORM}"
[ -x "${staged}/bin/wally" ] || fail "Archive did not contain bin/wally as expected."
# Replace the install tree wholesale. rm before copy is deliberate: overwriting a
# code-signed Mach-O in place while a copy may still be mapped kills it with
# SIGKILL (137). A fresh dir sidesteps that.
rm -rf "$LIB_DIR"
mkdir -p "$(dirname "$LIB_DIR")" "$BIN_DIR"
cp -R "$staged" "$LIB_DIR"
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
installed_version="$(wally --version 2>/dev/null \
    | sed -nE 's/^wally ([0-9]+\.[0-9]+\.[0-9]+).*/\1/p' \
    | head -1)"
if [ "${installed_version}" != "${VERSION}" ]; then
    fail "Installed Wally v${installed_version:-unknown}, but the latest release is v${VERSION}."
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
    if mkdir -p "$skill_dir" 2>/dev/null && curl -fsSL "$SKILL_URL" -o "${skill_dir}/SKILL.md"; then
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
if wally whoami >/dev/null 2>&1; then
    ok "already signed in"
elif [ ! -t 0 ] || [ ! -t 1 ]; then
    # No terminal: piped into bash over SSH, or a CI step. The browser flow
    # would try to open a browser that is not there and then block until the
    # request expires, which reads as the installer hanging.
    warn "not an interactive terminal — run \`wally login\` yourself"
else
    wally login || warn "sign-in did not finish. Run \`wally login\` when you are ready."
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
printf '     wally usage                               credit left and what you spent\n'
printf '     wally pull qwen3-0.6b                     download a model to this machine\n'
printf '   In Claude Code, ask: %s"get me started with RunAnywhere Wally"%s\n\n' "$DIM" "$R"
