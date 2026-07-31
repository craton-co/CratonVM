#!/bin/bash
# CratonBench performance gate — the mandatory no-perf-regression check.
#
# Runs every CratonBench phase as an isolated fresh process (pinned CPU,
# -Xmx8g, >= 7 reps), verifies the exact checksum on EVERY run, records the
# FULL distribution of every phase — p50/p90/p99/min/max/CV, every raw
# sample, compile counts, GC pause percentiles and peak RSS — under a
# versioned results directory, and fails if any phase's median exceeds its
# baseline by more than the tolerance. See regression-suite/README.md
# "Performance gate" for the policy this script enforces.
#
# Every measurement is bracketed by `reliability-gate.sh` (next to this
# script): PREFLIGHT before anything is measured, POSTFLIGHT before any
# verdict is printed. The reliability gate refuses runs whose measurement
# cannot be trusted at all — placeholder/zero baseline, host load or
# co-tenancy, CPU migration, thermal throttling, excessive variance, too few
# samples, missing environment manifest, checksum drift. A perf verdict is
# only meaningful on top of a run that passed it; see
# docs/benchmarking/reliability-gate.md and docs/benchmarking/methodology.md.
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
#   --reps N         Runs per phase (default 7; medians AND tails, no
#                    discards). Below --min-samples the run is refused at
#                    preflight rather than after an hour of measuring.
#   --tolerance PCT  Allowed regression over baseline (default 5).
#   --baseline FILE  Baseline TSV (default: cratonbench-baseline-azure-epyc.tsv
#                    next to this script).
#   --max-load L     Refuse to measure if 1-min load average exceeds L
#                    (default 2.0). A refused run exits 3 — a gate that
#                    measures under load produces garbage passes AND garbage
#                    failures, so it must not measure at all. The load is
#                    re-read per sample, so a run that goes loud mid-way is
#                    refused too.
#   --min-samples N  Samples per phase the reliability gate requires
#                    (default 7).
#   --max-cv PCT     Per-phase coefficient-of-variation ceiling (default 5).
#   --max-freq-drift PCT  Allowed within-run frequency spread on the pinned
#                    core (default 20).
#   --require-freq-data  Treat unreadable cpufreq sysfs as a failure rather
#                    than a warning.
#   --results-dir D  Where to write this run's results (default:
#                    <script dir>/results/v1/<utc>-<host>-<rev>).
#   --run-timeout S  Kill one measurement after S seconds (default 1800) and
#                    record it as a failed run. A hung VM must never be
#                    recorded as a merely slow one.
#   --no-vm-stats    Do not ask the VM for its shutdown JIT/GC summaries.
#                    (Both knobs are shutdown-only, so they do not perturb
#                    the measurement; this exists for strict A/B parity with
#                    an older result set that lacks them.)
#   --skip-reliability  Measure without the reliability gate. The manifest
#                    records reliability_gate=skipped and compare.py REFUSES
#                    to render a verdict from such a run. Debugging only.
#   --calibrate      Instead of gating, print a fresh baseline TSV body from
#                    this run's medians (for re-anchoring; the policy in the
#                    README governs when that is allowed). Baseline-threshold
#                    checks are skipped; checksum, load, pin and sample-count
#                    checks are NOT.
#   --phases LIST    Comma-separated subset (default: all baseline rows).
#
# Exit codes: 0 = pass, 1 = perf regression or checksum mismatch,
#             2 = usage/setup error, 3 = host too loaded to measure,
#             10/11/12/13/14 = reliability-gate refusal, passed through
#             unchanged (checksum / placeholder baseline / too few samples /
#             instability / missing manifest — see reliability-gate.sh).
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RELIABILITY_GATE="$SCRIPT_DIR/reliability-gate.sh"
RESULTS_SCHEMA=1
VM_FLAGS="-Xmx8g"
EXE=""
CPU=13
REPS=7
TOL=5
BASELINE="$SCRIPT_DIR/cratonbench-baseline-azure-epyc.tsv"
MAX_LOAD="2.0"
MIN_SAMPLES=7
MAX_CV=5
MAX_FREQ_DRIFT=20
REQUIRE_FREQ=0
RESULTS=""
RUN_TIMEOUT=1800
VM_STATS=1
SKIP_RELIABILITY=0
CALIBRATE=0
PHASES=""

GATE_ARGV="$*"

while [ $# -gt 0 ]; do
    case "$1" in
        -Exe) EXE="$2"; shift 2 ;;
        --cpu) CPU="$2"; shift 2 ;;
        --reps) REPS="$2"; shift 2 ;;
        --tolerance) TOL="$2"; shift 2 ;;
        --baseline) BASELINE="$2"; shift 2 ;;
        --max-load) MAX_LOAD="$2"; shift 2 ;;
        --min-samples) MIN_SAMPLES="$2"; shift 2 ;;
        --max-cv) MAX_CV="$2"; shift 2 ;;
        --max-freq-drift) MAX_FREQ_DRIFT="$2"; shift 2 ;;
        --require-freq-data) REQUIRE_FREQ=1; shift ;;
        --results-dir) RESULTS="$2"; shift 2 ;;
        --run-timeout) RUN_TIMEOUT="$2"; shift 2 ;;
        --no-vm-stats) VM_STATS=0; shift ;;
        --skip-reliability) SKIP_RELIABILITY=1; shift ;;
        --calibrate) CALIBRATE=1; shift ;;
        --phases) PHASES="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

[ -n "$EXE" ] && [ -x "$EXE" ] || { echo "FATAL: -Exe PATH required (executable)" >&2; exit 2; }
[ -f "$BASELINE" ] || { echo "FATAL: baseline not found: $BASELINE" >&2; exit 2; }
command -v taskset >/dev/null || { echo "FATAL: taskset required (Linux bench host)" >&2; exit 2; }
command -v javac >/dev/null || { echo "FATAL: javac required to compile CratonBench" >&2; exit 2; }
[ "$SKIP_RELIABILITY" = 1 ] || [ -f "$RELIABILITY_GATE" ] || {
    echo "FATAL: reliability gate not found: $RELIABILITY_GATE" >&2
    echo "       (run with --skip-reliability only if you accept a NON-CITABLE run)" >&2
    exit 2
}

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
