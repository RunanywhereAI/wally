#!/usr/bin/env bash
# Proves install.sh's install lock (acquire_install_lock / release_install_lock)
# is race-free: exactly one holder at a time, a stale lock from a dead pid is
# recovered, a live holder is never treated as stale, and a run only ever
# removes the lock it created itself.
#
# Drives the real install.sh through its --hold-install-lock=<seconds> debug
# seam (acquire, print "acquired", sleep, release, exit) against a throwaway
# HOME so the real ${HOME}/.local/lib/wally.lock is never touched. No network,
# no docker, no download.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL="${SCRIPT_DIR}/../../install.sh"
INSTALL_SH="${WALLY_INSTALL_SH:-sh}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

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

lock_path() { printf '%s/.local/lib/wally.lock' "$1"; }

# --- exactly one holder at a time, many acquirers released by one barrier ---
scratch="$WORK/concurrent"; mkdir -p "$scratch"
outdir="$WORK/concurrent-out"; mkdir -p "$outdir"
barrier="$WORK/go"
count=15
for i in $(seq 1 "$count"); do
    (
        while [ ! -e "$barrier" ]; do :; done
        status=0
        HOME="$scratch" "$INSTALL_SH" "$INSTALL" --hold-install-lock=1 \
            >"$outdir/$i.out" 2>"$outdir/$i.err" || status=$?
        echo "$status" >"$outdir/$i.exit"
    ) &
done
sleep 0.2
touch "$barrier"
wait

acquired=0
refused=0
for i in $(seq 1 "$count"); do
    code="$(cat "$outdir/$i.exit")"
    if [ "$code" = "0" ] && grep -q "acquired" "$outdir/$i.out"; then
        acquired=$((acquired + 1))
    elif [ "$code" = "1" ] && grep -q "another Wally install" "$outdir/$i.err"; then
        refused=$((refused + 1))
    fi
done
check "exactly one of ${count} concurrent acquirers wins, the rest are refused" \
    "$([ "$acquired" -eq 1 ] && [ "$refused" -eq $((count - 1)) ] && echo 1 || echo 0)"

check "the lock is removed after the winner exits" \
    "$([ ! -e "$(lock_path "$scratch")" ] && echo 1 || echo 0)"

scratch2="$WORK/reuse"; mkdir -p "$scratch2"
reuse_status=0
HOME="$scratch2" "$INSTALL_SH" "$INSTALL" --hold-install-lock=0 \
    >"$WORK/reuse.out" 2>"$WORK/reuse.err" || reuse_status=$?
check "the lock is reusable after a clean release" \
    "$([ "$reuse_status" -eq 0 ] && grep -q acquired "$WORK/reuse.out" && echo 1 || echo 0)"

# --- a stale lock from a dead pid is recovered ---
scratch3="$WORK/stale"; mkdir -p "$scratch3/.local/lib"
( : ) & deadpid=$!
wait "$deadpid" 2>/dev/null || true
printf '%s\n' "$deadpid" >"$(lock_path "$scratch3")"
stale_status=0
HOME="$scratch3" "$INSTALL_SH" "$INSTALL" --hold-install-lock=0 \
    >"$WORK/stale.out" 2>"$WORK/stale.err" || stale_status=$?
check "a stale lock from a dead pid is recovered, not treated as held" \
    "$([ "$stale_status" -eq 0 ] && grep -q acquired "$WORK/stale.out" && echo 1 || echo 0)"

# --- a lock whose owner is alive is respected, never recovered as stale ---
scratch4="$WORK/live"; mkdir -p "$scratch4"
HOME="$scratch4" "$INSTALL_SH" "$INSTALL" --hold-install-lock=2 \
    >"$WORK/live-a.out" 2>"$WORK/live-a.err" &
apid=$!
for _ in $(seq 1 50); do [ -e "$(lock_path "$scratch4")" ] && break; sleep 0.05; done
live_b_status=0
HOME="$scratch4" "$INSTALL_SH" "$INSTALL" --hold-install-lock=0 \
    >"$WORK/live-b.out" 2>"$WORK/live-b.err" || live_b_status=$?
wait "$apid"
check "a lock whose owner is alive is refused, not deleted out from under it" \
    "$([ "$live_b_status" -eq 1 ] && grep -q "another Wally install" "$WORK/live-b.err" && echo 1 || echo 0)"

# --- release removes only the lock a run created itself ---
scratch5="$WORK/ownership"; mkdir -p "$scratch5"
HOME="$scratch5" "$INSTALL_SH" "$INSTALL" --hold-install-lock=2 \
    >"$WORK/own-a.out" 2>"$WORK/own-a.err" &
apid=$!
for _ in $(seq 1 50); do [ -e "$(lock_path "$scratch5")" ] && break; sleep 0.05; done
# Simulate the path now belonging to a different run (what a second run's own
# stale-recovery would leave behind): overwrite the lock's content in place.
printf '999999999\n' >"$(lock_path "$scratch5")"
wait "$apid"
check "release leaves a lock behind once its content no longer names this run" \
    "$([ -e "$(lock_path "$scratch5")" ] && [ "$(cat "$(lock_path "$scratch5")")" = "999999999" ] && echo 1 || echo 0)"

[ "$fails" -eq 0 ] || { printf '%d test(s) failed\n' "$fails" >&2; exit 1; }
printf 'all install-lock cases pass\n'
