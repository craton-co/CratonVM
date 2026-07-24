#!/bin/bash
# CratonBench performance gate — the mandatory no-perf-regression check.
#
# Runs every CratonBench phase as an isolated fresh process (pinned CPU,
# -Xmx8g, median of N reps), verifies the exact checksum on EVERY run, and
# fails if any phase's median exceeds its baseline by more than the
# tolerance. See regression-suite/README.md "Performance gate" for the
# policy this script enforces.
#
# Usage:
#   run-cratonbench-gate.sh -Exe /abs/path/to/cratonvm [options]
#
# Options:
#   -Exe PATH        CratonVM binary under test (required; absolute path —
#                    see the suite-runner convention: env vars do not
#                    propagate through nested shells, the path does).
#   --cpu N          CPU to pin with taskset (default 13, the documented
#                    bench-host CPU).
#   --reps N         Runs per phase (default 5; medians, no discards).
#   --tolerance PCT  Allowed regression over baseline (default 5).
#   --baseline FILE  Baseline TSV (default: cratonbench-baseline-azure-epyc.tsv
#                    next to this script).
#   --max-load L     Refuse to measure if 1-min load average exceeds L
#                    (default 2.0). A refused run exits 3 — a gate that
#                    measures under load produces garbage passes AND garbage
#                    failures, so it must not measure at all.
#   --calibrate      Instead of gating, print a fresh baseline TSV body from
#                    this run's medians (for re-anchoring; the policy in the
#                    README governs when that is allowed).
#   --phases LIST    Comma-separated subset (default: all baseline rows).
#
# Exit codes: 0 = pass, 1 = perf regression or checksum mismatch,
#             2 = usage/setup error, 3 = host too loaded to measure.
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
EXE=""
CPU=13
REPS=5
TOL=5
BASELINE="$SCRIPT_DIR/cratonbench-baseline-azure-epyc.tsv"
MAX_LOAD="2.0"
CALIBRATE=0
PHASES=""

while [ $# -gt 0 ]; do
    case "$1" in
        -Exe) EXE="$2"; shift 2 ;;
        --cpu) CPU="$2"; shift 2 ;;
        --reps) REPS="$2"; shift 2 ;;
        --tolerance) TOL="$2"; shift 2 ;;
        --baseline) BASELINE="$2"; shift 2 ;;
        --max-load) MAX_LOAD="$2"; shift 2 ;;
        --calibrate) CALIBRATE=1; shift ;;
        --phases) PHASES="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

[ -n "$EXE" ] && [ -x "$EXE" ] || { echo "FATAL: -Exe PATH required (executable)" >&2; exit 2; }
[ -f "$BASELINE" ] || { echo "FATAL: baseline not found: $BASELINE" >&2; exit 2; }
command -v taskset >/dev/null || { echo "FATAL: taskset required (Linux bench host)" >&2; exit 2; }
command -v javac >/dev/null || { echo "FATAL: javac required to compile CratonBench" >&2; exit 2; }

# Load guard: never measure on a busy box.
LOAD1=$(cut -d' ' -f1 /proc/loadavg)
awk -v l="$LOAD1" -v m="$MAX_LOAD" 'BEGIN { exit !(l > m) }' && {
    echo "REFUSED: 1-min load $LOAD1 > $MAX_LOAD — rerun on a quiet host" >&2
    exit 3
}

CLASSES=$(mktemp -d)
trap 'rm -rf "$CLASSES"' EXIT
javac -d "$CLASSES" "$ROOT/bench/CratonBench.java" || { echo "FATAL: bench compile failed" >&2; exit 2; }

median() { printf '%s\n' "$@" | sort -n | awk '{ a[NR]=$1 } END { print a[int((NR+1)/2)] }'; }

FAILURES=0
CAL_OUT=""
echo "CratonBench gate: exe=$EXE cpu=$CPU reps=$REPS tol=${TOL}% load=$LOAD1"
while IFS=$'\t' read -r phase base_ms want_sum status _evidence; do
    case "$phase" in \#*|"") continue ;; esac
    if [ -n "$PHASES" ]; then
        case ",$PHASES," in *",$phase,"*) ;; *) continue ;; esac
    fi
    times=()
    for _ in $(seq 1 "$REPS"); do
        line=$(taskset -c "$CPU" "$EXE" -Xmx8g -cp "$CLASSES" CratonBench "$phase" 2>/dev/null \
               | grep -E "^[0-9]\." | head -1)
        ms=$(echo "$line" | grep -oE '[0-9]+ ms' | grep -oE '[0-9]+')
        sum=$(echo "$line" | grep -oE '\[[-0-9]+\]' | tr -d '[]')
        if [ -z "$ms" ] || [ -z "$sum" ]; then
            echo "  $phase: FAIL (no output — crash or wrong phase name)"; FAILURES=$((FAILURES+1)); continue 2
        fi
        if [ "$sum" != "$want_sum" ]; then
            echo "  $phase: FAIL (checksum $sum != expected $want_sum) — CORRECTNESS regression"
            FAILURES=$((FAILURES+1)); continue 2
        fi
        times+=("$ms")
    done
    med=$(median "${times[@]}")
    if [ "$CALIBRATE" = 1 ]; then
        CAL_OUT="$CAL_OUT$phase\t$med\t$want_sum\tprovisional\tcalibrated $(date +%F) on $(hostname)\n"
        echo "  $phase: median ${med}ms (runs: ${times[*]})"
        continue
    fi
    budget=$(( base_ms + base_ms * TOL / 100 ))
    if [ "$med" -gt "$budget" ]; then
        echo "  $phase: FAIL median ${med}ms > budget ${budget}ms (baseline ${base_ms}ms +${TOL}%, $status) runs: ${times[*]}"
        FAILURES=$((FAILURES+1))
    else
        note=""
        improved=$(( base_ms * 90 / 100 ))
        [ "$med" -lt "$improved" ] && note="  (>10% better — consider re-anchoring baseline)"
        echo "  $phase: PASS median ${med}ms <= ${budget}ms$note"
    fi
done < "$BASELINE"

if [ "$CALIBRATE" = 1 ]; then
    echo; echo "# calibrated baseline body:"; printf "$CAL_OUT"
    exit 0
fi
echo "---------------------------------------------"
if [ "$FAILURES" -gt 0 ]; then
    echo "CRATONBENCH GATE: FAILED ($FAILURES phase(s) regressed)"
    exit 1
fi
echo "CRATONBENCH GATE: PASS"
