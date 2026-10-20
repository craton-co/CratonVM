#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Lane W7-D — does a DEFAULT build complete a mark cycle, on each probe of the
# standing battery, and what changes that?
#
# `docs/internal/g1-2026-09-20/w7d-the-cycle-that-no-pause-closes.md` is the
# page; this is the instrument it asks the orchestrator to run.
#
# THE QUESTION, which is a scope question and not an A/B
# ------------------------------------------------------
# W6-M measured `remark_pauses=1, cleanup_pauses=0` on a default build of one
# workload and `remark=0, cleanup=0` on another — one cycle opened and never
# closed, versus no cycle ever opened. Those are different states with
# different fixes, they were not distinguishable from any counter in the tree,
# and NO probe in the battery had been checked for either. So the first output
# of this script is not a delta: it is a TABLE of engagement, one row per
# probe per arm, and a run whose whole result is "every probe reads
# cleanup_pauses=0 on every arm" is a finding, not a failed experiment.
#
# WHAT THE NEW REPORT LINE ADDS
# -----------------------------
# `[GC] g1 mark-doors:` (one line per door) says WHICH driver of the mark-cycle
# lifecycle ran and what it saw:
#
#   door=maybe_gc|alloc_fail|jit_driver|force_full|system_gc
#   visits=  total times that driver was reached
#   idle=    no cycle open, IHOP declined to open one
#   started= no cycle open, this visit opened one
#   waiting= a cycle IS open and the background marker had not drained
#   finished=a cycle was open, the marker had drained, remark+cleanup ran
#   lost_stw=as finished/started, but the brief STW was lost to a peer
#
# `visits=0` on every door is "no driver ran on the path this workload's pauses
# take". Large `waiting` with `finished=0` is "a driver ran constantly and was
# refused every time". Before this line both printed as `cleanup_pauses=0`.
#
# THE ARMS
# --------
#  default  nothing set.  `CRATONVM_G1_ALLOC_MARK_DRIVE` defaults ON, so the
#           allocation-failure pause drives the lifecycle. THIS IS THE
#           CONFIGURATION A USER GETS, and the whole point of the round's §4.2
#           item is that no marking measurement had ever been taken in it.
#  pre      `CRATONVM_G1_ALLOC_MARK_DRIVE=0`.  The kill switch, and the
#           BEFORE arm: byte-for-byte the behaviour every marking measurement
#           in waves 1-6 was taken under.
#  driver   `...=0` plus `CRATONVM_G1_JIT_MARK_DRIVER=1`.  The configuration
#           `w3c-the-satb-registry-walk-census.md` and
#           `w3c-the-six-w2c-flags-measured.md` carry on every arm.
#  both     default plus the JIT driver.  Answers the question the default
#           decision needs: once the allocation pause drives the cycle, does
#           the JIT driver still add anything?
#  null     `CRATONVM_G1_IHOP_POLL_GATE=1` with the back-off OFF.  PROVABLY
#           INERT — the fast decline sits inside the branch `check_ihop`
#           guards on `g1_ihop_backoff`, so with nothing declining there is
#           nothing to decline cheaply, and that is pinned by
#           `the_poll_gate_is_inert_with_the_back_off_off` in
#           `gc/tests/g1_w6b_ihop_poll_gate.rs`.  Its spread is a measurement
#           of THIS HOST and nothing else (README §5 rule 2: a null arm moves
#           the median up to 9% here).
#
# WHAT IT ENFORCES
# ----------------
#  * ARMS INTERLEAVED within a probe, and a `--batches` sweep, because README
#    §5 rule 7 says an engagement count is itself a random variable and
#    establishing its error bar costs a second batch. The same configuration
#    produced 7 and 13 mixed pauses in wave 6 with no treatment in it.
#  * THE LEVER IS CHECKED, not assumed: the binary is grepped for the flag's
#    own name and the run refuses rather than silently producing two copies of
#    one arm (`orchestrator-wave-1-measurements.md` §4).
#  * THE CONTROL FOR THE GREP is a literal that has been in the binary since
#    the collector existed. `strings -a` returned zero for nine new literals
#    AND for `UseG1GC` on a Windows PE (README §5 rule 8), so a grep with no
#    positive control cannot tell "this build lacks the flag" from "this check
#    cannot see anything".
#  * THE CHECKSUM IS ASSERTED, per probe: one distinct value across every arm
#    and rep of that probe, or those rows are void.
#  * `RUST_LOG=error` on every run (W5-C §7 measured one arm at 14 minutes
#    against 6-8 s for its neighbours at the default tracing level), and every
#    run is bounded by `timeout`.
#
# Usage:
#   tools/probes/g1-w7d-markcycle-sweep.sh <cratonvm.exe> [reps] [batches]
#
# Example (the batch the page asks for):
#   OUT_DIR=w7d-b1 tools/probes/g1-w7d-markcycle-sweep.sh cratonvm-g1w7d.exe 3 1
#   OUT_DIR=w7d-b2 tools/probes/g1-w7d-markcycle-sweep.sh cratonvm-g1w7d.exe 3 1

set -u

BIN="${1:?usage: $0 <cratonvm binary> [reps] [batches]}"
shift
REPS="${1:-3}"
if [ $# -gt 0 ]; then shift; fi
BATCHES="${1:-1}"
if [ $# -gt 0 ]; then shift; fi

PROBE_DIR="$(cd "$(dirname "$0")" && pwd)"
OUT="${OUT_DIR:-$PWD/w7d-sweep}"
mkdir -p "$OUT"

# WHICH BINARY IS THIS? Asked, with a control, not assumed.
#
# `CRATONVM_G1_JIT_MARK_DRIVER` is the positive control: it has been in the
# binary since 2026-09-05 and every arm below that mentions it depends on it.
# If the control misses, the CHECK is broken and the script says so instead of
# reporting four arms of which two are the same arm.
if ! grep -a -q 'CRATONVM_G1_JIT_MARK_DRIVER' "$BIN" 2>/dev/null; then
    echo "ABORT: the positive control 'CRATONVM_G1_JIT_MARK_DRIVER' is not" >&2
    echo "       findable in '$BIN'. That is a broken INSTRUMENT, not a" >&2
    echo "       missing flag — do not read a negative from this check." >&2
    exit 2
fi
if ! grep -a -q 'CRATONVM_G1_ALLOC_MARK_DRIVE' "$BIN" 2>/dev/null; then
    echo "ABORT: '$BIN' predates CRATONVM_G1_ALLOC_MARK_DRIVE (the control" >&2
    echo "       literal WAS found, so this check works). Its 'default' and" >&2
    echo "       'pre' arms would be the same arm under two names." >&2
    exit 2
fi

# The standing battery, at the heap and IHOP each probe was last measured at.
# `-Xmx192m -XX:InitiatingHeapOccupancyPercent=25` is the configuration W5-C
# established ENGAGES: at 128m and 160m these shapes never cross the threshold
# and every arm compares two runs in which the mark cycle never happened.
#
# G1MixedRepeatProbe appears twice on purpose. The `...0` form is the default
# arm of W6-M's recipe; the `...4` form adds a `System.gc()` every 4 phases,
# which is the FLAG-FREE route W6-M measured completing 6 cycles on a build
# with no other door. It is the positive control for the whole sweep: a batch
# in which even that row reads `cleanup_pauses=0` has an instrument problem.
declare -a PROBES=(
    "churn|G1ChurnPauseProbe|24 200"
    "mixed|G1MixedRepeatProbe|12 6 24 24 4096 0"
    "mixed_sysgc|G1MixedRepeatProbe|12 6 24 24 4096 4"
    "oldburst|G1OldBurstProbe|16 1 48 8 96 64"
    "pollstorm|G1PollStormProbe|4 16 1 48 8 96 64"
    "rsetwide|G1RsetWideProbe|96 200 2048"
)

JAVA_OPTS=(-XX:+UseG1GC -Xmx192m -XX:InitiatingHeapOccupancyPercent=25)

declare -a ARM_NAMES=(default pre driver both null)

arm_env() {
    case "$1" in
        default) echo "" ;;
        pre)     echo "CRATONVM_G1_ALLOC_MARK_DRIVE=0" ;;
        driver)  echo "CRATONVM_G1_ALLOC_MARK_DRIVE=0 CRATONVM_G1_JIT_MARK_DRIVER=1" ;;
        both)    echo "CRATONVM_G1_JIT_MARK_DRIVER=1" ;;
        # Provably inert; see the header.
        null)    echo "CRATONVM_G1_IHOP_POLL_GATE=1" ;;
    esac
}

echo "batch,probe,arm,rep,wall_ms,checksum,remark_pauses,cleanup_pauses,young_count,mixed_count,ihop_polls,to_space_exhausted,door_maybe_gc_visits,door_maybe_gc_finished,door_alloc_fail_visits,door_alloc_fail_waiting,door_alloc_fail_finished,door_jit_visits,door_jit_finished,door_force_full_finished,door_system_gc_finished,jit_mark_driver,status" \
    > "$OUT/results.csv"

for batch in $(seq 1 "$BATCHES"); do
for entry in "${PROBES[@]}"; do
    IFS='|' read -r pname pclass pargs <<< "$entry"
    for rep in $(seq 1 "$REPS"); do
        for arm in "${ARM_NAMES[@]}"; do
            log="$OUT/b$batch-$pname-$arm-$rep.log"
            # `CRATONVM_GC_STATS=1` is what prints every `[GC]` line parsed
            # below. Without it the run still happens, the checksum still
            # agrees, and every counter column comes back EMPTY — a battery
            # that looks complete and reports no engagement at all.
            # shellcheck disable=SC2046,SC2086
            timeout 300 env $(arm_env "$arm") RUST_LOG=error CRATONVM_GC_STATS=1 \
                "$BIN" "${JAVA_OPTS[@]}" -cp "$PROBE_DIR" "$pclass" $pargs \
                > "$log" 2>&1
            status=$?

            g() { grep -o "$1=[0-9]*" "$log" | head -1 | cut -d= -f2; }
            gb() { grep -o "$1=[a-z]*" "$log" | head -1 | cut -d= -f2; }
            # One door's one column, off its own line.
            door() {
                grep -o "door=$1 .*" "$log" | head -1 \
                    | grep -o "$2=[0-9]*" | head -1 | cut -d= -f2
            }

            wall=$(g wallMs)
            checksum=$(g checksum)
            remark=$(g remark_pauses)
            cleanup=$(g cleanup_pauses)
            young=$(grep -o '\[GC-SUMMARY\] young count=[0-9]*' "$log" | head -1 | grep -o '[0-9]*$')
            mixed=$(grep -o '\[GC-SUMMARY\] mixed count=[0-9]*' "$log" | head -1 | grep -o '[0-9]*$')
            polls=$(g ihop_polls)
            late=$(g to_space_exhausted)
            jitdrv=$(gb jit_mark_driver)

            echo "$batch,$pname,$arm,$rep,${wall:-},${checksum:-},${remark:-},${cleanup:-},${young:-},${mixed:-},${polls:-},${late:-},$(door maybe_gc visits),$(door maybe_gc finished),$(door alloc_fail visits),$(door alloc_fail waiting),$(door alloc_fail finished),$(door jit_driver visits),$(door jit_driver finished),$(door force_full finished),$(door system_gc finished),${jitdrv:-},$status" \
                >> "$OUT/results.csv"
            if [ "$status" -ne 0 ]; then
                echo "FAIL probe=$pname arm=$arm rep=$rep status=$status (see $log)" >&2
            fi
            echo "b=$batch $pname arm=$arm rep=$rep wall=${wall:-?} remark=${remark:-?} cleanup=${cleanup:-?} mixed=${mixed:-?} alloc_fail_finished=$(door alloc_fail finished)"
        done
    done
done
done

# The checksum gate, PER PROBE. A run that is faster because it lost an object
# must fail, not score.
echo
bad=0
for entry in "${PROBES[@]}"; do
    IFS='|' read -r pname _ _ <<< "$entry"
    distinct=$(awk -F, -v p="$pname" 'NR>1 && $2==p {print $6}' "$OUT/results.csv" | sort -u | grep -c .)
    if [ "$distinct" -eq 1 ]; then
        echo "checksum $pname: OK, one distinct value across every arm and rep"
    else
        echo "checksum $pname: FAILED — $distinct distinct values; those rows are void" >&2
        bad=1
    fi
done

# THE ENGAGEMENT TABLE, which is the actual deliverable. Printed rather than
# left in the CSV because the number that decides the default is "how many of
# these rows completed a cycle", and a reader who has to pivot a CSV to find it
# will report the wall clock instead.
echo
echo "cycles completed (cleanup_pauses), summed over reps and batches:"
awk -F, 'NR>1 {k=$2" "$3; s[k]+=$8; n[k]++} END {for (k in s) printf "  %-24s %6d  over %d runs\n", k, s[k], n[k]}' \
    "$OUT/results.csv" | sort
echo
echo "results: $OUT/results.csv"
exit "$bad"
