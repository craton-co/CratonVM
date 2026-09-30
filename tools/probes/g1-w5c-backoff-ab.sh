#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Lane W5-C — the four-arm A/B behind
# `docs/internal/g1-2026-09-20/w5c-the-back-off-on-a-heap-that-grows.md`.
#
# WHAT IT ENFORCES, because this round retracted three measurements for want of
# each of these (see `orchestrator-wave-1-measurements.md` §7.3):
#
#  * ARMS ARE INTERLEAVED (arm A rep 1, arm B rep 1, ... x N), so host drift
#    lands on every arm equally rather than on whichever ran last.
#  * A NULL ARM IS INCLUDED and it is provably inert: `MARK_BACKOFF_DEADLINE=1`
#    with the back-off OFF cannot execute, because the deadline is consulted
#    only inside the branch `check_ihop` guards on `g1_ihop_backoff`. That is
#    pinned by `the_deadline_is_inert_with_the_back_off_off` in
#    `gc/tests/g1_w5c_mark_backoff_deadline.rs`, so the null arm's spread is a
#    measurement of the HOST and nothing else.
#  * THE LEVER IS CHECKED, not assumed. `CRATONVM_G1_JIT_MARK_DRIVER=1` is set
#    on every arm because without it a JIT'd workload runs ZERO mark cycles and
#    every marking flag is inert (wave 3, §0); the parser below reports
#    `cleanup_pauses`, `ihop_polls`, `backoff_declined_polls` and
#    `backoff_deadline_releases` per run so no row can be read without knowing
#    whether the path ran and how often.
#  * THE CHECKSUM IS ASSERTED. A run whose checksum differs from the first
#    run's is a run that lost an object; it fails rather than scores.
#
# Usage:
#   tools/probes/g1-w5c-backoff-ab.sh <cratonvm.exe> [reps] [probe args...]
#
# Example:
#   tools/probes/g1-w5c-backoff-ab.sh target-g1w5/release/cratonvm.exe 9

set -u

BIN="${1:?usage: $0 <cratonvm binary> [reps] [probe args...]}"
shift
REPS="${1:-9}"
if [ $# -gt 0 ]; then shift; fi
PROBE_ARGS=("$@")
if [ ${#PROBE_ARGS[@]} -eq 0 ]; then
    # The settle-half-dominant shape: 48 MiB dropped per phase against 96 MiB
    # of young churn that promotes nothing, so the window in which the back-off
    # is blind is the LONG half of each phase. That is the shape the back-off
    # can get wrong; `16 3 32 10 16 64` is its grow-dominant sibling, where
    # genuine growth re-arms the gate every phase and the back-off looks good.
    PROBE_ARGS=(16 1 48 8 96 64)
fi

PROBE_DIR="$(cd "$(dirname "$0")" && pwd)"
OUT="${OUT_DIR:-$PWD/w5c-ab}"
mkdir -p "$OUT"

# The heap is deliberately tight relative to the probe's peak reachable set
# (coreStart + phases*coreDelta + burst = 86 MiB at the defaults): the failure
# the back-off can cause is "the old generation fills and evacuation fails",
# and a heap with room to spare cannot show it.
# SIZED AGAINST A MEASURED ENGAGEMENT CHECK, not against intuition. At
# `-Xmx128m` and `-Xmx160m` this probe's old generation never crosses the
# threshold on CratonVM: `ihop_polls=0`, `cleanup_pauses=0`, and every arm
# would have been comparing two runs in which the mark cycle never happened.
# `-Xmx192m` with a 25% IHOP gives 19-24 mark cycles and 4-20 M object scans,
# which is a workload the flags can actually move. `CRATONVM_G1_JIT_MARK_DRIVER`
# is set per-arm below for the same reason: without it a JIT'd workload runs
# ZERO cycles (wave 3, §0) and five of six mark flags are silently inert.
JAVA_OPTS=(-XX:+UseG1GC -Xmx192m -XX:InitiatingHeapOccupancyPercent=25)

declare -a ARM_NAMES=(base null backoff deadline)

arm_env() {
    case "$1" in
        base)     echo "" ;;
        # Provably inert: the deadline cannot be reached with the back-off off.
        null)     echo "CRATONVM_G1_MARK_BACKOFF_DEADLINE=1" ;;
        backoff)  echo "CRATONVM_G1_IHOP_BACKOFF=1" ;;
        deadline) echo "CRATONVM_G1_IHOP_BACKOFF=1 CRATONVM_G1_MARK_BACKOFF_DEADLINE=1" ;;
    esac
}

echo "arm,rep,wall_ms,checksum,cleanup_pauses,remark_pauses,scans_total,to_space_exhausted,ihop_polls,backoff_declined_polls,backoff_deadline_releases,evac_failure_pauses,backoff_enabled,backoff_deadline,jit_mark_driver" \
    > "$OUT/results.csv"

for rep in $(seq 1 "$REPS"); do
    for arm in "${ARM_NAMES[@]}"; do
        log="$OUT/$arm-$rep.log"
        # `RUST_LOG=error` is NOT cosmetic and must not be dropped. MEASURED:
        # at the default tracing level, one arm of the first battery took 14
        # MINUTES against 6-8 s for its neighbours, with the evacuation screens'
        # diagnostics escalating alongside it; at `RUST_LOG=error` the identical
        # configuration is 7.8-8.5 s across six consecutive runs. The arms were
        # not comparable without it.
        #
        # The MECHANISM is unexplained, and this comment deliberately does not
        # guess. "Log I/O" was the first answer and it does not survive: the
        # screen sites are rate-limited on `n <= 8 || n.is_power_of_two()`, so a
        # counter reading `#1048576` is a million EVENTS, not a million lines,
        # and W5-B counted 43 WARN lines total on their probe with no change
        # under `RUST_LOG=error`. Keep the setting; do not cite a cause.
        #
        # The `[GC]` summary lines this script parses are `eprintln!`, not
        # `tracing`, so nothing it reads is lost. `timeout` bounds the damage if
        # an arm escalates anyway; a run that hits it is recorded with an empty
        # wall rather than stalling the battery.
        # shellcheck disable=SC2046
        timeout 300 env $(arm_env "$arm") RUST_LOG=error CRATONVM_G1_JIT_MARK_DRIVER=1 \
            "$BIN" "${JAVA_OPTS[@]}" -cp "$PROBE_DIR" G1OldBurstProbe "${PROBE_ARGS[@]}" \
            > "$log" 2>&1
        status=$?

        wall=$(grep -o 'wallMs=[0-9]*' "$log" | head -1 | cut -d= -f2)
        checksum=$(grep -o 'checksum=[0-9]*' "$log" | head -1 | cut -d= -f2)
        cleanup=$(grep -o 'cleanup_pauses=[0-9]*' "$log" | head -1 | cut -d= -f2)
        remark=$(grep -o 'remark_pauses=[0-9]*' "$log" | head -1 | cut -d= -f2)
        scans=$(grep -o 'scans_total=[0-9]*' "$log" | head -1 | cut -d= -f2)
        late=$(grep -o 'to_space_exhausted=[0-9]*' "$log" | head -1 | cut -d= -f2)
        polls=$(grep -o 'ihop_polls=[0-9]*' "$log" | head -1 | cut -d= -f2)
        declined=$(grep -o 'backoff_declined_polls=[0-9]*' "$log" | head -1 | cut -d= -f2)
        releases=$(grep -o 'backoff_deadline_releases=[0-9]*' "$log" | head -1 | cut -d= -f2)
        # Evacuation failures are per-PAUSE, on the cycle line's `degraded=`
        # field, and they are a different number from `to_space_exhausted`:
        # the latter is the adaptive-IHOP feedback event and is only counted
        # with `CRATONVM_G1_ADAPTIVE_IHOP` on. Both are reported.
        evacfail=$(grep -c 'degraded=[^ ]*evacuation-failure' "$log")
        benabled=$(grep -o 'backoff_enabled=[a-z]*' "$log" | head -1 | cut -d= -f2)
        bdeadline=$(grep -o 'backoff_deadline=[a-z]*' "$log" | head -1 | cut -d= -f2)
        jitdrv=$(grep -o 'jit_mark_driver=[a-z]*' "$log" | head -1 | cut -d= -f2)

        if [ "$status" -ne 0 ]; then
            echo "FAIL arm=$arm rep=$rep status=$status (see $log)" >&2
        fi
        echo "$arm,$rep,${wall:-},${checksum:-},${cleanup:-},${remark:-},${scans:-},${late:-},${polls:-},${declined:-},${releases:-},${evacfail:-0},${benabled:-},${bdeadline:-},${jitdrv:-}" \
            >> "$OUT/results.csv"
        echo "arm=$arm rep=$rep wall=${wall:-?} cycles=${cleanup:-?} late=${late:-?} declined=${declined:-?} released=${releases:-?} evacfail=${evacfail:-0}"
    done
done

# The checksum gate. One distinct value for the whole battery, or the battery
# is void: a run that is faster because it lost an object must fail, not score.
distinct=$(tail -n +2 "$OUT/results.csv" | cut -d, -f4 | sort -u | grep -c .)
echo
if [ "$distinct" -eq 1 ]; then
    echo "checksum: OK, one distinct value across every arm and rep"
else
    echo "checksum: FAILED — $distinct distinct values; this battery is void" >&2
fi
echo "results: $OUT/results.csv"
