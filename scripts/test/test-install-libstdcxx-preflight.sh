#!/usr/bin/env bash
# Proves install.sh's libstdc++ preflight (check_libstdcxx_symbols) refuses a
# Linux host whose libstdc++ lacks the GLIBCXX_/CXXABI_ symbol versions the
# bottle needs, before downloading anything -- and that a host whose
# libstdc++ has them proceeds to a normal install. libstdc++.so.6 merely
# being present (checked separately) is not enough: v0.6.0's bottle needed
# GLIBCXX_3.4.32, and a stock Ubuntu 22.04 libstdc++ (3.4.30) has the soname
# but not that symbol version, so it passed the old preflight, downloaded,
# and only then failed to start.
#
# Stubs uname/getconf/ldconfig/curl so this runs the same on macOS as it
# would on the Linux hosts it protects, and feeds a fixture symbol listing
# through WALLY_LIBSTDCXX_SYMBOLS rather than a real libstdc++.so.6. No
# network, no docker.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL="${SCRIPT_DIR}/../../install.sh"
INSTALL_SH="${WALLY_INSTALL_SH:-sh}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

STUB="$WORK/stub-bin"
mkdir -p "$STUB"

cat > "$STUB/uname" <<'UNAME'
#!/bin/sh
case "$1" in
    -s) echo Linux ;;
    -m) echo x86_64 ;;
esac
UNAME
chmod +x "$STUB/uname"

cat > "$STUB/getconf" <<'GETCONF'
#!/bin/sh
[ "$1" = "GNU_LIBC_VERSION" ] && echo "glibc 2.35"
GETCONF
chmod +x "$STUB/getconf"

# Names every library check_linux_system requires present, at fake paths:
# check_libstdcxx_symbols reads WALLY_LIBSTDCXX_SYMBOLS directly instead of
# opening libstdc++.so.6's path, so these never need to exist on disk.
cat > "$STUB/ldconfig" <<'LDCONFIG'
#!/bin/sh
[ "$1" = "-p" ] || exit 0
cat <<'EOF'
	libstdc++.so.6 (libc6,x86-64) => /fake/libstdc++.so.6
	libgcc_s.so.1 (libc6,x86-64) => /fake/libgcc_s.so.1
	libssl.so.3 (libc6,x86-64) => /fake/libssl.so.3
	libcrypto.so.3 (libc6,x86-64) => /fake/libcrypto.so.3
	libcurl.so.4 (libc6,x86-64) => /fake/libcurl.so.4
EOF
LDCONFIG
chmod +x "$STUB/ldconfig"

# Fake curl, the same shape as scripts/test/test-install-cross-shell.sh's:
# serves a canned release/tarball/checksum from $WALLY_STUB_DIR by matching
# the requested URL.
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

# A release good enough to install: bin/wally answers --version and
# `account whoami`/`account login` the way the real binary does.
GOOD="$WORK/fixture-good"
mkdir -p "$GOOD/wally-linux-x86_64/bin"
cat > "$GOOD/wally-linux-x86_64/bin/wally" <<'WALLY'
#!/bin/sh
case "$1 $2" in
    "--version ") echo "wally 1.2.3 (stub)" ;;
    "account whoami") exit 1 ;;
    "account login")  echo "stub login ok" ;;
    *) echo "stub wally: unexpected invocation: $*" >&2; exit 2 ;;
esac
WALLY
chmod +x "$GOOD/wally-linux-x86_64/bin/wally"
( cd "$GOOD" && tar -czf asset.tar.gz wally-linux-x86_64 )
echo '{"tag_name": "v1.2.3"}' > "$GOOD/release.json"
shasum -a 256 "$GOOD/asset.tar.gz" | awk '{print $1"  wally-1.2.3-linux-x86_64.tar.gz"}' > "$GOOD/asset.sha256"

fails=0
check() {
    name="$1"; ok="$2"
    if [ "$ok" = "1" ]; then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s\n' "$name"
        fails=$((fails + 1))
    fi
}

run() {
    home="$1"; symbols="$2"
    status=0
    out="$(WALLY_LIBSTDCXX_SYMBOLS="$symbols" WALLY_STUB_DIR="$GOOD" \
        HOME="$home" PATH="$STUB:$PATH" "$INSTALL_SH" "$INSTALL" 2>&1)" || status=$?
    printf '%s\n%s' "$status" "$out"
}

# --- a libstdc++ below the floor is refused before "Downloading" -----------
old="$WORK/old-libstdcxx.txt"
printf 'GLIBCXX_3.4.25\nCXXABI_1.3.11\n' >"$old"
home_old="$WORK/home-old"; mkdir -p "$home_old"
result_old="$(run "$home_old" "$old")"
old_code="$(printf '%s\n' "$result_old" | head -1)"
old_body="$(printf '%s\n' "$result_old" | tail -n +2)"
check "an old libstdc++ is refused (exit 1)" "$([ "$old_code" = "1" ] && echo 1 || echo 0)"
check "the refusal names the required GLIBCXX floor" \
    "$(printf '%s' "$old_body" | grep -qF 'GLIBCXX_3.4.30' && echo 1 || echo 0)"
check "nothing was downloaded before the refusal" \
    "$(printf '%s' "$old_body" | grep -qF Downloading && echo 0 || echo 1)"

# --- a libstdc++ at or above the floor is accepted and the install proceeds --
new="$WORK/new-libstdcxx.txt"
printf 'GLIBCXX_3.4.30\nCXXABI_1.3.13\nGLIBCXX_3.4.31\n' >"$new"
home_new="$WORK/home-new"; mkdir -p "$home_new"
result_new="$(run "$home_new" "$new")"
new_code="$(printf '%s\n' "$result_new" | head -1)"
new_body="$(printf '%s\n' "$result_new" | tail -n +2)"
check "a libstdc++ at the floor installs (exit 0)" "$([ "$new_code" = "0" ] && echo 1 || echo 0)"
check "wally v1.2.3 is reported installed" \
    "$(printf '%s' "$new_body" | grep -qF 'wally v1.2.3' && echo 1 || echo 0)"

[ "$fails" -eq 0 ] || { printf '%d test(s) failed\n' "$fails" >&2; exit 1; }
printf 'all libstdc++ preflight cases pass\n'
