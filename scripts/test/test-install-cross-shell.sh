#!/usr/bin/env bash
# Proves install.sh behaves identically under bash, dash and plain `sh` --
# the shell `curl ... | sh` actually resolves to on Debian/Ubuntu (dash),
# on macOS (bash) and wherever `sh` is something else POSIX. Runs the real
# install.sh under all three with a stubbed curl/uname, a fixture release,
# tarball and checksum, and no network, then diffs the output byte for byte.
#
# Covers the cases PR #79 claims are shell-independent: a full happy-path
# install, an unsupported platform, a bad checksum, and a failed release
# lookup (the case that motivated the POSIX rewrite -- with `pipefail`,
# dash died on `set -o pipefail` before printing anything; without it, the
# failure must still reach `fail` and print a message on all three shells).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL="${SCRIPT_DIR}/../../install.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

STUB="$WORK/stub-bin"
mkdir -p "$STUB"

# Fake curl: serves a canned GitHub release response, tarball or checksum
# from $WALLY_STUB_DIR by matching the requested URL, the same way the real
# calls in install.sh shape theirs. A fixture that does not exist fails the
# way a real network error would (curl's own exit code for an HTTP failure).
cat > "$STUB/curl" <<'CURL'
#!/bin/sh
out=""
url=""
while [ $# -gt 0 ]; do
    case "$1" in
        -o) shift; out="$1" ;;
        http*) url="$1" ;;
    esac
    shift
done
case "$url" in
    *api.github.com/repos/*/releases/latest) body="$WALLY_STUB_DIR/release.json" ;;
    *.sha256)                                body="$WALLY_STUB_DIR/asset.sha256" ;;
    *)                                       body="$WALLY_STUB_DIR/asset.tar.gz" ;;
esac
[ -f "$body" ] || exit 22
if [ -n "$out" ]; then cp "$body" "$out"; else cat "$body"; fi
CURL
chmod +x "$STUB/curl"

# Fake uname: reports whatever OS/arch the case under test wants.
cat > "$STUB/uname" <<'UNAME'
#!/bin/sh
case "$1" in
    -s) echo "${WALLY_STUB_OS:-Darwin}" ;;
    -m) echo "${WALLY_STUB_ARCH:-arm64}" ;;
esac
UNAME
chmod +x "$STUB/uname"

# A good fixture: a release tarball whose bin/wally answers --version,
# `account whoami` and `account login` the way the real binary does, plus a
# matching sha256.
#
# The nesting matters. `whoami`/`login` moved under `account`, so a fake that
# still matched on $1 alone saw `account`, fell off the end of the case, and
# exited 0 -- which install.sh reads as "already signed in", so every run
# skipped the sign-in branches this fixture exists to exercise. Dispatching on
# the real two-token grammar puts them back under test. The `*)` arm is the
# other half: an invocation this stub does not know is a hard error, the way
# the real CLI exits 2 on an unknown command, so if install.sh ever drifts back
# to the flat spelling the test fails loudly instead of silently passing.
GOOD="$WORK/fixture-good"
mkdir -p "$GOOD/wally-macos-arm64/bin"
cat > "$GOOD/wally-macos-arm64/bin/wally" <<'WALLY'
#!/bin/sh
case "$1 $2" in
    "--version ") echo "wally 1.2.3 (stub)" ;;
    "account whoami") exit 1 ;;
    "account login")  echo "stub login ok" ;;
    *) echo "stub wally: unexpected invocation: $*" >&2; exit 2 ;;
esac
WALLY
chmod +x "$GOOD/wally-macos-arm64/bin/wally"
( cd "$GOOD" && tar -czf asset.tar.gz wally-macos-arm64 )
echo '{"tag_name": "v1.2.3"}' > "$GOOD/release.json"
shasum -a 256 "$GOOD/asset.tar.gz" | awk '{print $1"  wally-1.2.3-macos-arm64.tar.gz"}' > "$GOOD/asset.sha256"

# A fixture whose checksum does not match its tarball.
BADSUM="$WORK/fixture-badsum"
mkdir -p "$BADSUM"
cp "$GOOD/release.json" "$GOOD/asset.tar.gz" "$BADSUM/"
printf '%s  wally-1.2.3-macos-arm64.tar.gz\n' \
    "0000000000000000000000000000000000000000000000000000000000000000" \
    > "$BADSUM/asset.sha256"

# A fixture with no files at all, so the release lookup fails as if the
# network were down.
EMPTY="$WORK/fixture-empty"
mkdir -p "$EMPTY"

fails=0
check() {
    name="$1"; expected="$2"; actual="$3"
    if [ "$expected" = "$actual" ]; then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s\n  --- expected ---\n%s\n  --- actual ---\n%s\n' \
            "$name" "$expected" "$actual"
        fails=$((fails + 1))
    fi
}

# Runs install.sh under one shell for one case and prints
# "<exit-code>\n<stdout+stderr>", with that run's own $HOME path normalized
# out -- a happy-path install embeds $HOME in its output (install dir, skill
# path, binary path), and each shell gets its own HOME so the three runs
# cannot clobber each other's install tree.
run_case() {
    shell="$1"; stub_dir="$2"; os="$3"; arch="$4"; extra="$5"
    home="$WORK/home-$shell"
    rm -rf "$home"; mkdir -p "$home"
    set +e
    # shellcheck disable=SC2086 -- $extra is a controlled, space-free flag list.
    out="$(WALLY_STUB_DIR="$stub_dir" WALLY_STUB_OS="$os" WALLY_STUB_ARCH="$arch" \
        HOME="$home" PATH="$STUB:$PATH" "$shell" "$INSTALL" $extra 2>&1)"
    code=$?
    set -e
    printf '%s\n%s' "$code" "$(printf '%s' "$out" | sed "s#$home#\$HOME#g")"
}

for case_name in happy-path unsupported-platform bad-checksum failed-release-lookup \
                 update-already-latest update-available; do
    extra=""
    case "$case_name" in
        happy-path)             stub="$GOOD";   os="Darwin"; arch="arm64"  ;;
        unsupported-platform)   stub="$GOOD";   os="Darwin"; arch="x86_64" ;;
        bad-checksum)           stub="$BADSUM"; os="Darwin"; arch="arm64"  ;;
        failed-release-lookup)  stub="$EMPTY";  os="Darwin"; arch="arm64"  ;;
        # `wally update` from a build already current: stops before download.
        update-already-latest)  stub="$GOOD";   os="Darwin"; arch="arm64"; extra="--version=1.2.3" ;;
        # `wally update` from an older build: proceeds to the full install.
        update-available)       stub="$GOOD";   os="Darwin"; arch="arm64"; extra="--version=1.0.0" ;;
    esac
    bash_out="$(run_case bash "$stub" "$os" "$arch" "$extra")"
    dash_out="$(run_case dash "$stub" "$os" "$arch" "$extra")"
    sh_out="$(run_case sh "$stub" "$os" "$arch" "$extra")"
    check "$case_name: dash byte-identical to bash" "$bash_out" "$dash_out"
    check "$case_name: sh byte-identical to bash"    "$bash_out" "$sh_out"
done

[ "$fails" -eq 0 ] || { printf '%d comparison(s) failed\n' "$fails" >&2; exit 1; }
printf 'all cross-shell cases byte-identical\n'
