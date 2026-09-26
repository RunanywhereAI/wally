#!/usr/bin/env bash
# Proves install.sh's install lock (acquire_install_lock / release_install_lock)
# holds under contention: exactly one of many concurrent runs acquires it, a
# stale lock from a dead pid is
# recovered, a live holder is never treated as stale, a run only ever
# removes the lock it created itself, and the stale-lock takeover itself is a
# real mutex -- many runs racing the same stale lock still produce exactly
# one winner, and a takeover directory left behind by a crash is cleared and
# retried rather than wedging every later install.
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

# --- many contenders race the same stale lock; exactly one ever wins ---
# Every contender here sees the same dead-pid lock and would race the same
# rm+ln pair without the takeover mutex; repeat the trial several times
# because that race, if it existed, would flake rather than fail every time.
# Hold, like the plain-contention case above, rather than release
# immediately: a holder that lets go right away lets many contenders win in
# turn (fine -- the lock is meant to be reusable) with no window for a
# straggler to steal it, which would pass whether or not the theft the
# takeover mutex closes is actually closed. A held winner gives every
# straggler its full sub-second race a real target to try to steal from.
stale_race_trials=5
stale_race_count=10
for trial in $(seq 1 "$stale_race_trials"); do
    scratch="$WORK/stale-race-$trial"; mkdir -p "$scratch/.local/lib"
    ( : ) & deadpid=$!
    wait "$deadpid" 2>/dev/null || true
    printf '%s\n' "$deadpid" >"$(lock_path "$scratch")"
    outdir="$WORK/stale-race-$trial-out"; mkdir -p "$outdir"
    barrier="$WORK/stale-race-$trial-go"
    for i in $(seq 1 "$stale_race_count"); do
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
    won=0
    for i in $(seq 1 "$stale_race_count"); do
        code="$(cat "$outdir/$i.exit")"
        [ "$code" = "0" ] && grep -q "acquired" "$outdir/$i.out" && won=$((won + 1))
    done
    check "trial ${trial}: exactly one of ${stale_race_count} contenders racing one stale lock wins" \
        "$([ "$won" -eq 1 ] && echo 1 || echo 0)"
    check "trial ${trial}: no takeover directory is left behind once the race settles" \
        "$([ ! -e "$(lock_path "$scratch").takeover" ] && echo 1 || echo 0)"
done

# --- a takeover directory left behind by a crash is cleared and retried ---
scratch6="$WORK/takeover-stale"; mkdir -p "$scratch6/.local/lib"
( : ) & deadpid=$!
wait "$deadpid" 2>/dev/null || true
printf '%s\n' "$deadpid" >"$(lock_path "$scratch6")"
takeover_dir="$(lock_path "$scratch6").takeover"
mkdir "$takeover_dir"
# A fixed, long-past timestamp rather than "N minutes ago": both BSD and GNU
# touch accept -t in this form, so the test doesn't need OS-specific date math
# to make the directory look older than the ~60s takeover is ever held for.
touch -t 202001010000.00 "$takeover_dir"
takeover_status=0
HOME="$scratch6" "$INSTALL_SH" "$INSTALL" --hold-install-lock=0 \
    >"$WORK/takeover.out" 2>"$WORK/takeover.err" || takeover_status=$?
check "a takeover directory stale enough to be a crash is cleared and retried" \
    "$([ "$takeover_status" -eq 0 ] && grep -q acquired "$WORK/takeover.out" && echo 1 || echo 0)"
check "the stale takeover directory does not survive the retry" \
    "$([ ! -e "$takeover_dir" ] && echo 1 || echo 0)"

[ "$fails" -eq 0 ] || { printf '%d test(s) failed\n' "$fails" >&2; exit 1; }
printf 'all install-lock cases pass\n'
