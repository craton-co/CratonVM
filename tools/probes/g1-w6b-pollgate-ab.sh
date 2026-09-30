#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Lane W6-B — the four-arm A/B behind
# `docs/internal/g1-2026-09-20/w6b-the-declined-poll-is-the-cost.md`.
#
# THE TWO QUESTIONS IT ANSWERS AT ONCE, on one binary and one workload:
#
#  (a) does `to_space_exhausted` rise when a mark cycle is suppressed on a
#      workload with a GROWING retained set and cycles that reclaim materially?
#      Wave 3 saw one rep of nine at 3 on a probe whose retained set was fixed;
#      wave 5 saw the opposite (70 -> 24) on `G1OldBurstProbe`. This is the
#      SECOND BATCH that has to reproduce the order of that result before it is
#      a measurement (`orchestrator-wave-1-measurements.md` §8.1).
#  (b) what does a DECLINED poll cost when N threads are declining? Wave 3
#      counted 1 616 077 declined polls, wave 5 counted 41 641 213 against 130
#      mark cycles, and both said in terms that their probe had ONE thread and
#      could not price the two global read-modify-writes each poll performs.
#
# WHAT IT ENFORCES, because this round retracted three measurements for want of
# each of these (`orchestrator-wave-1-measurements.md` §7.3):
#
#  * ARMS ARE INTERLEAVED (arm A rep 1, arm B rep 1, ... x N), so host drift
#    lands on every arm equally rather than on whichever ran last.
#  * A NULL ARM IS INCLUDED and it is provably inert: `IHOP_POLL_GATE=1` with
#    the back-off OFF cannot execute, because the fast decline sits inside the
#    branch `check_ihop` guards on `g1_ihop_backoff` — nothing declines with
#    the back-off off, so there is nothing to decline cheaply. That is pinned
#    by `the_poll_gate_is_inert_with_the_back_off_off` in
#    `gc/tests/g1_w6b_ihop_poll_gate.rs`, so the null arm's spread is a
#    measurement of the HOST and nothing else.
#  * THE LEVER IS CHECKED, not assumed. `CRATONVM_G1_JIT_MARK_DRIVER=1` on
#    every arm, because without it a JIT'd workload runs ZERO mark cycles and
#    every marking flag is inert (wave 3 §0) -- AND because that driver is the
#    thing that re-polls at allocation rate, so the cost under test does not
#    exist without it. The parser reports `cleanup_pauses`, `ihop_polls`,
#    `backoff_declined_polls`, `fast_declines` and what each switch resolved to,
#    so no row can be read without knowing whether the path ran and how often.
#  * THE CHECKSUM IS ASSERTED, per thread count. `G1PollStormProbe`'s checksum
#    is deterministic and scheduler-independent but DOES depend on the thread
#    count (each thread folds its own share), so the gate is one distinct value
#    per thread count, which is what an A/B needs since arms are only ever
#    compared within a thread count.
#
# Usage:
#   tools/probes/g1-w6b-pollgate-ab.sh <cratonvm.exe> [reps] [threads-csv] [probe args...]
#
# Example:
#   tools/probes/g1-w6b-pollgate-ab.sh cratonvm-g1w6b.exe 9 1,8,32

set -u

BIN="${1:?usage: $0 <cratonvm binary> [reps] [threads-csv] [probe args...]}"
shift
REPS="${1:-9}"
if [ $# -gt 0 ]; then shift; fi
# 1, 4, 16 rather than 1, 8, 32. MEASURED on this host (32 logical cores, four
# concurrent release builds): threads=1 runs in ~7 s, 4 in ~8 s, 16 in ~7.8 s,
# and **32 did not complete in 560 s** — the same superlinear cliff wave 3 hit
# with `MtChurnProbe` at 256 threads and recorded as the workload's own, not a
# hang. A run that does not finish is not a measurement and is not reported as
# one, so the sweep stops at half the machine.
THREADS_CSV="${1:-1,4,16}"
if [ $# -gt 0 ]; then shift; fi
PROBE_ARGS=("$@")
if [ ${#PROBE_ARGS[@]} -eq 0 ]; then
    # `G1OldBurstProbe 16 1 48 8 96 64`'s settle-half-dominant shape, with
    # every MiB read as a PROCESS TOTAL and divided by the thread count -- so
    # sweeping threads holds the live set, the total allocation and the heap
    # pressure fixed and varies only how many threads poll the gate.
    PROBE_ARGS=(16 1 48 8 96 64)
fi

PROBE_DIR="$(cd "$(dirname "$0")" && pwd)"
OUT="${OUT_DIR:-$PWD/w6b-ab}"
mkdir -p "$OUT"

# Sized against W5-C's measured engagement check, not against intuition: at
# `-Xmx128m` and `-Xmx160m` this shape's old generation never crosses the
# threshold on CratonVM (`ihop_polls=0`, `cleanup_pauses=0`) and every arm
# would compare two runs in which the mark cycle never happened. `-Xmx192m`
# with a 25% IHOP is the configuration that engages.
JAVA_OPTS=(-XX:+UseG1GC -Xmx192m -XX:InitiatingHeapOccupancyPercent=25)

# WHICH BINARY IS THIS? Asked, not assumed.
#
# A binary that predates `CRATONVM_G1_IHOP_POLL_GATE` ignores the flag
# silently: it runs the battery, produces four arms, and two of them are the
# same arm under different names. That is `orchestrator-wave-1-measurements.md`
# §4 exactly -- a lever read on a path the binary does not have -- and it is
# how `w3c-the-six-w2c-flags-measured.md` §2 came to publish the back-off's
# null arm twice under two names. So the flag's own name is looked for IN the
# binary, and the arm list follows what is found.
#
# On a binary without it the null arm falls back to
# `CRATONVM_G1_MARK_BACKOFF_DEADLINE=1` with the back-off off, which W5-C
# established is inert for the same structural reason (the deadline is
# consulted only inside the branch `check_ihop` guards on `g1_ihop_backoff`),
# and the `gated` arm is dropped rather than faked.
if grep -a -q 'CRATONVM_G1_IHOP_POLL_GATE' "$BIN" 2>/dev/null; then
    HAS_POLL_GATE=1
    declare -a ARM_NAMES=(base null backoff gated)
else
    HAS_POLL_GATE=0
    declare -a ARM_NAMES=(base null backoff)
    echo "NOTE: '$BIN' does not carry CRATONVM_G1_IHOP_POLL_GATE." >&2
    echo "      Running three arms; null = MARK_BACKOFF_DEADLINE with the back-off off." >&2
fi

arm_env() {
    case "$1" in
        base)    echo "" ;;
        # Provably inert either way: the fast decline cannot be reached with
        # nothing declining, and neither can the deadline. See the header and
        # the tests named there.
        null)    if [ "$HAS_POLL_GATE" = 1 ]; then
                     echo "CRATONVM_G1_IHOP_POLL_GATE=1"
                 else
                     echo "CRATONVM_G1_MARK_BACKOFF_DEADLINE=1"
                 fi ;;
        backoff) echo "CRATONVM_G1_IHOP_BACKOFF=1" ;;
        gated)   echo "CRATONVM_G1_IHOP_BACKOFF=1 CRATONVM_G1_IHOP_POLL_GATE=1" ;;
    esac
}

echo "threads,arm,rep,wall_ms,checksum,cleanup_pauses,scans_total,to_space_exhausted,ihop_polls,backoff_declined_polls,fast_declines,drain_passes,drain_pass_us,young_count,mixed_count,evac_failure_pauses,backoff_enabled,poll_gate,jit_mark_driver,status" \
    > "$OUT/results.csv"

IFS=',' read -r -a THREADS <<< "$THREADS_CSV"

for th in "${THREADS[@]}"; do
for rep in $(seq 1 "$REPS"); do
    for arm in "${ARM_NAMES[@]}"; do
        log="$OUT/t$th-$arm-$rep.log"
        # `RUST_LOG=error` is NOT cosmetic and must not be dropped: W5-C §7
        # measured one arm at 14 MINUTES against 6-8 s for its neighbours at
        # the default tracing level, and the arm it happened to was the arm
        # under test. The `[GC]` lines this script parses are `eprintln!`, not
        # `tracing`, so nothing it reads is lost. The mechanism is unexplained
        # and this comment deliberately does not guess at one.
        # shellcheck disable=SC2046
        #
        # `CRATONVM_GC_STATS=1` is what prints every `[GC]` line this script
        # parses. Without it the run still happens and the checksum still
        # agrees, and every counter column comes back EMPTY — which is the
        # §7.1 trap in its purest form, a battery that looks complete and
        # reports no engagement at all.
        timeout 300 env $(arm_env "$arm") RUST_LOG=error CRATONVM_GC_STATS=1 CRATONVM_G1_JIT_MARK_DRIVER=1 \
            "$BIN" "${JAVA_OPTS[@]}" -cp "$PROBE_DIR" G1PollStormProbe "$th" "${PROBE_ARGS[@]}" \
            > "$log" 2>&1
        status=$?

        g() { grep -o "$1=[0-9]*" "$log" | head -1 | cut -d= -f2; }
        gb() { grep -o "$1=[a-z]*" "$log" | head -1 | cut -d= -f2; }

        wall=$(g wallMs)
        checksum=$(g checksum)
        cleanup=$(g cleanup_pauses)
        scans=$(g scans_total)
        late=$(g to_space_exhausted)
        polls=$(g ihop_polls)
        declined=$(g backoff_declined_polls)
        fast=$(g fast_declines)
        drainp=$(grep -o 'drain_passes count=[0-9]*' "$log" | head -1 | grep -o '[0-9]*')
        drainus=$(grep -o 'drain_passes count=[0-9]* total_us=[0-9]*' "$log" | head -1 | sed 's/.*total_us=//')
        young=$(grep -o '\[GC-SUMMARY\] young count=[0-9]*' "$log" | head -1 | grep -o '[0-9]*$')
        mixed=$(grep -o '\[GC-SUMMARY\] mixed count=[0-9]*' "$log" | head -1 | grep -o '[0-9]*$')
        evacfail=$(grep -c 'degraded=[^ ]*evacuation-failure' "$log")
        benabled=$(gb backoff_enabled)
        pgate=$(gb poll_gate)
        jitdrv=$(gb jit_mark_driver)

        if [ "$status" -ne 0 ]; then
            echo "FAIL threads=$th arm=$arm rep=$rep status=$status (see $log)" >&2
        fi
        echo "$th,$arm,$rep,${wall:-},${checksum:-},${cleanup:-},${scans:-},${late:-},${polls:-},${declined:-},${fast:-},${drainp:-},${drainus:-},${young:-},${mixed:-},${evacfail:-0},${benabled:-},${pgate:-},${jitdrv:-},$status" \
            >> "$OUT/results.csv"
        echo "t=$th arm=$arm rep=$rep wall=${wall:-?} cycles=${cleanup:-?} late=${late:-?} polls=${polls:-?} declined=${declined:-?} fast=${fast:-?} evacfail=${evacfail:-0}"
    done
done
done

# The checksum gate, PER THREAD COUNT. One distinct value within a thread count
# or that thread count's rows are void: a run that is faster because it lost an
# object must fail, not score.
echo
bad=0
for th in "${THREADS[@]}"; do
    distinct=$(awk -F, -v t="$th" 'NR>1 && $1==t {print $5}' "$OUT/results.csv" | sort -u | grep -c .)
    if [ "$distinct" -eq 1 ]; then
        echo "checksum threads=$th: OK, one distinct value across every arm and rep"
    else
        echo "checksum threads=$th: FAILED — $distinct distinct values; those rows are void" >&2
        bad=1
    fi
done
echo "results: $OUT/results.csv"
exit "$bad"
