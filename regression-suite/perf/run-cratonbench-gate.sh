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
WORK=$(mktemp -d)
trap 'rm -rf "$CLASSES" "$WORK"' EXIT
javac -d "$CLASSES" "$ROOT/bench/CratonBench.java" || { echo "FATAL: bench compile failed" >&2; exit 2; }

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
median() { printf '%s\n' "$@" | sort -n | awk '{ a[NR]=$1 } END { print a[int((NR+1)/2)] }'; }

# Full distribution, printed as: min p50 p90 p99 max mean stddev cv_pct.
# Percentiles are NEAREST-RANK (ceil(p/100 * n)) — the same definition the
# VM's own G1 pause summary uses, so a p99 printed here and a p99 printed by
# the VM mean the same thing. No sample is ever discarded, and the tail is
# recorded because a median-only record cannot distinguish "5% slower" from
# "usually identical, occasionally 3x", which is the shape most of this
# repository's real regressions have had.
dist() {
    printf '%s\n' "$@" | sort -n | awk '
        { v[NR] = $1 + 0; s += $1; q += $1 * $1 }
        END {
            n = NR
            if (n == 0) { print "- - - - - - - -"; exit }
            r50 = int((50 * n + 99) / 100); if (r50 < 1) r50 = 1
            r90 = int((90 * n + 99) / 100); if (r90 < 1) r90 = 1
            r99 = int((99 * n + 99) / 100); if (r99 < 1) r99 = 1
            mean = s / n
            var = (n > 1) ? (q - n * mean * mean) / (n - 1) : 0
            if (var < 0) var = 0
            sd = sqrt(var)
            cv = (mean > 0) ? 100 * sd / mean : 0
            printf "%d %d %d %d %d %.1f %.2f %.2f\n", v[1], v[r50], v[r90], v[r99], v[n], mean, sd, cv
        }'
}

dash() { if [ -n "${1:-}" ]; then printf '%s' "$1"; else printf '%s' '-'; fi; }

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" 2>/dev/null | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1
    else printf '%s' '-'; fi
}

read_load1() { if [ -r /proc/loadavg ]; then cut -d' ' -f1 /proc/loadavg; else printf '%s' '-'; fi; }

read_sysfs() {
    local v
    if [ -r "$1" ] && IFS= read -r v < "$1" 2>/dev/null; then printf '%s' "$v"; else printf '%s' '-'; fi
}
read_throttle() { read_sysfs "/sys/devices/system/cpu/cpu$CPU/thermal_throttle/core_throttle_count"; }
read_freq_khz() { read_sysfs "/sys/devices/system/cpu/cpu$CPU/cpufreq/scaling_cur_freq"; }

# Any of this run's TSVs -> the JSON twin, keys taken from the '#'-prefixed
# header row. Checksums stay strings: they exceed 2^53 and would silently
# lose precision in every JSON consumer that parses numbers as doubles.
tsv_to_json() {  # tsv_to_json <tsv> <json> <array-key>
    awk -F'\t' -v name="$3" -v schema="$RESULTS_SCHEMA" '
        NR == 1 { for (i = 1; i <= NF; i++) { h = $i; sub(/^#/, "", h); key[i] = h }; cols = NF; next }
        /^#/ { next }
        NF > 1 {
            s = "    {"
            for (i = 1; i <= cols; i++) {
                v = $i
                gsub(/\\/, "\\\\", v); gsub(/"/, "\\\"", v)
                if (key[i] != "checksum" && v ~ /^-?[0-9]+(\.[0-9]+)?$/) q = v; else q = "\"" v "\""
                s = s (i > 1 ? ", " : "") "\"" key[i] "\": " q
            }
            rows[++r] = s "}"
        }
        END {
            printf "{\n  \"schema_version\": %s,\n  \"%s\": [\n", schema, name
            for (i = 1; i <= r; i++) printf "%s%s\n", rows[i], (i < r ? "," : "")
            printf "  ]\n}\n"
        }
    ' "$1" > "$2"
}

# ---------------------------------------------------------------------------
# Environment manifest
# ---------------------------------------------------------------------------
# Written BEFORE anything is measured, so a run can never end up with times
# but no provenance. "-" means "could not read", and the reliability gate
# treats "-" as absent rather than as a value — an unrecorded field is
# exactly the field a later reader would otherwise assume was checked.
REVISION=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null)
[ -n "$REVISION" ] || REVISION="-"
# --untracked-files=no deliberately: enumerating untracked files means walking
# target/, which takes minutes on a built tree. Tracked-file modifications are
# what makes a revision non-reproducible; a stray untracked file does not.
if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)" ]; then REV_DIRTY=yes; else REV_DIRTY=no; fi
BIN_SHA=$(sha256_of "$EXE")
BENCH_SHA=$(sha256_of "$ROOT/bench/CratonBench.java")
BASELINE_SHA=$(sha256_of "$BASELINE")
GATE_SHA=$(sha256_of "${BASH_SOURCE[0]}")
BIN_MTIME=$(date -u -r "$EXE" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)
CPU_MODEL=$(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo 2>/dev/null)
[ -n "$CPU_MODEL" ] || CPU_MODEL=$(uname -p 2>/dev/null)
CPU_COUNT=$(getconf _NPROCESSORS_ONLN 2>/dev/null)
JDK_VERSION=$(javac -version 2>&1 | head -1)
JAVA_VERSION=$(java -version 2>&1 | head -1)
HOST=$(hostname 2>/dev/null)
KERNEL=$(uname -srm 2>/dev/null)
CREATED=$(date -u +%Y-%m-%dT%H:%M:%SZ)
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-${HOST:-host}-$(printf '%s' "$REVISION" | cut -c1-12)"
[ -n "$RESULTS" ] || RESULTS="$SCRIPT_DIR/results/v$RESULTS_SCHEMA/$RUN_ID"
mkdir -p "$RESULTS" || { echo "FATAL: cannot create results directory: $RESULTS" >&2; exit 2; }

CHILD_CMDLINE="taskset -c $CPU $EXE $VM_FLAGS -cp <classes> CratonBench <phase>"
MANIFEST="$RESULTS/manifest.tsv"
{
    printf '# CratonBench run manifest — schema %s. key<TAB>value.\n' "$RESULTS_SCHEMA"
    printf 'schema_version\t%s\n' "$RESULTS_SCHEMA"
    printf 'run_id\t%s\n' "$RUN_ID"
    printf 'created_utc\t%s\n' "$CREATED"
    printf 'host\t%s\n' "$(dash "$HOST")"
    printf 'kernel\t%s\n' "$(dash "$KERNEL")"
    printf 'cpu_model\t%s\n' "$(dash "$CPU_MODEL")"
    printf 'cpu_count\t%s\n' "$(dash "$CPU_COUNT")"
    printf 'cpu_pinned\t%s\n' "$CPU"
    printf 'cpu_affinity_mask\t%s\n' "$CPU"
    printf 'revision\t%s\n' "$REVISION"
    printf 'revision_dirty\t%s\n' "$REV_DIRTY"
    printf 'binary_path\t%s\n' "$EXE"
    printf 'binary_sha256\t%s\n' "$(dash "$BIN_SHA")"
    printf 'binary_mtime_utc\t%s\n' "$(dash "$BIN_MTIME")"
    printf 'bench_source_sha256\t%s\n' "$(dash "$BENCH_SHA")"
    printf 'gate_script_sha256\t%s\n' "$(dash "$GATE_SHA")"
    printf 'vm_flags\t%s\n' "$VM_FLAGS"
    printf 'vm_stats_enabled\t%s\n' "$VM_STATS"
    printf 'command_line\t%s\n' "$CHILD_CMDLINE"
    printf 'gate_command_line\t%s %s\n' "${BASH_SOURCE[0]}" "$GATE_ARGV"
    printf 'jdk_version\t%s\n' "$(dash "$JDK_VERSION")"
    printf 'java_version\t%s\n' "$(dash "$JAVA_VERSION")"
    printf 'baseline_file\t%s\n' "$BASELINE"
    printf 'baseline_sha256\t%s\n' "$(dash "$BASELINE_SHA")"
    printf 'phases_requested\t%s\n' "$(dash "$PHASES")"
    printf 'reps\t%s\n' "$REPS"
    printf 'min_samples\t%s\n' "$MIN_SAMPLES"
    printf 'tolerance_pct\t%s\n' "$TOL"
    printf 'max_load\t%s\n' "$MAX_LOAD"
    printf 'max_cv_pct\t%s\n' "$MAX_CV"
    printf 'max_freq_drift_pct\t%s\n' "$MAX_FREQ_DRIFT"
    printf 'run_timeout_s\t%s\n' "$RUN_TIMEOUT"
    printf 'calibrate\t%s\n' "$CALIBRATE"
    printf 'load1_start\t%s\n' "$LOAD1"
} > "$MANIFEST"

# ---------------------------------------------------------------------------
# Reliability gate — preflight
# ---------------------------------------------------------------------------
run_reliability() {  # run_reliability <preflight|postflight> [extra args...]
    local mode="$1"
    shift
    if [ "$SKIP_RELIABILITY" = 1 ]; then
        echo "WARNING: reliability gate $mode SKIPPED (--skip-reliability)." >&2
        echo "         This run is NOT citable evidence and compare.py will refuse it." >&2
        return 0
    fi
    local extra=""
    [ "$REQUIRE_FREQ" = 1 ] && extra="$extra --require-freq-data"
    [ "$CALIBRATE" = 1 ] && extra="$extra --calibrate"
    # shellcheck disable=SC2086  # deliberate word-splitting of flag lists
    bash "$RELIABILITY_GATE" "$mode" \
        --results "$RESULTS" \
        --baseline "$BASELINE" \
        ${PHASES:+--phases $PHASES} \
        --max-load "$MAX_LOAD" \
        --min-samples "$MIN_SAMPLES" \
        --max-cv "$MAX_CV" \
        --max-freq-drift "$MAX_FREQ_DRIFT" \
        $extra "$@"
}

if [ "$SKIP_RELIABILITY" = 1 ]; then
    printf 'reliability_gate\tskipped\n' >> "$MANIFEST"
else
    printf 'reliability_gate\tenforced\n' >> "$MANIFEST"
fi

run_reliability preflight --cpu "$CPU" --reps "$REPS" || exit $?

# ---------------------------------------------------------------------------
# Measurement
# ---------------------------------------------------------------------------
SAMPLES="$RESULTS/samples.tsv"
SUMMARY="$RESULTS/summary.tsv"
printf '#phase\trep\tms\tchecksum\tcpu_pinned\tcpu_observed\tload1\tthrottle_delta\tkhz_min\tkhz_max\tpeak_rss_kb\tcompiles_c1\tcompiles_c2\tcompiles_osr\tdeopts\tcompile_ms\tgc_young_count\tgc_young_p50_us\tgc_young_p99_us\tgc_young_max_us\tgc_minor\tgc_major\texit_code\n' > "$SAMPLES"
printf '#phase\tn\tmin_ms\tp50_ms\tp90_ms\tp99_ms\tmax_ms\tmean_ms\tstddev_ms\tcv_pct\tchecksum\tbaseline_ms\tbaseline_status\tbudget_ms\tverdict\tpeak_rss_kb_max\tcompiles_c1_max\tcompiles_c2_max\tgc_young_count_max\tgc_young_p50_us_max\tgc_young_p99_us_max\tgc_young_max_us_max\n' > "$SUMMARY"

# One isolated, pinned, instrumented measurement. Sets RUN_MS, RUN_SUM,
# RUN_EXIT and the per-run environment fields. The process is started in the
# background and polled rather than run synchronously, because the CPU it
# actually executed on, the pinned core's frequency excursion and its peak
# RSS are only observable while it is alive — and "which CPU did it really
# run on" is precisely the question BENCHMARK.md's retracted HashMap result
# could not answer after the fact.
run_one() {
    local phase="$1"
    local out="$WORK/out" err="$WORK/err"
    local thr0 thr1 pid line c khz deadline
    local -a arr
    RUN_MS=""; RUN_SUM=""; RUN_EXIT=0
    RUN_CPUS=""; RUN_KMIN=""; RUN_KMAX=""; RUN_RSS=0; RUN_TIMEOUT_HIT=0
    RUN_LOAD=$(read_load1)
    thr0=$(read_throttle)

    if [ "$VM_STATS" = 1 ]; then
        env CRATONVM_GC_STATS=1 CRATONVM_DBG_JIT_METHOD_STATS=1 \
            taskset -c "$CPU" "$EXE" $VM_FLAGS -cp "$CLASSES" CratonBench "$phase" \
            >"$out" 2>"$err" </dev/null &
    else
        taskset -c "$CPU" "$EXE" $VM_FLAGS -cp "$CLASSES" CratonBench "$phase" \
            >"$out" 2>"$err" </dev/null &
    fi
    pid=$!
    deadline=$(( $(date +%s) + RUN_TIMEOUT ))

    while kill -0 "$pid" 2>/dev/null; do
        # /proc/<pid>/stat field 39 is the CPU the task last ran on. comm can
        # contain spaces and parentheses, so everything up to the last ") "
        # is dropped first and the remainder is indexed from field 3.
        if IFS= read -r line < "/proc/$pid/stat" 2>/dev/null; then
            line=${line#*') '}
            arr=($line)
            c=${arr[36]:-}
            if [ -n "$c" ]; then
                case ",$RUN_CPUS," in
                    *",$c,"*) ;;
                    *) RUN_CPUS="${RUN_CPUS:+$RUN_CPUS,}$c" ;;
                esac
            fi
        fi
        khz=$(read_freq_khz)
        if [ "$khz" != "-" ]; then
            [ -n "$RUN_KMIN" ] || RUN_KMIN=$khz
            [ -n "$RUN_KMAX" ] || RUN_KMAX=$khz
            [ "$khz" -lt "$RUN_KMIN" ] && RUN_KMIN=$khz
            [ "$khz" -gt "$RUN_KMAX" ] && RUN_KMAX=$khz
        fi
        while IFS= read -r line; do
            case "$line" in
                VmHWM:*)
                    line=${line//[!0-9]/}
                    [ -n "$line" ] && [ "$line" -gt "$RUN_RSS" ] && RUN_RSS=$line
                    ;;
            esac
        done < "/proc/$pid/status" 2>/dev/null
        if [ "$(date +%s)" -ge "$deadline" ]; then
            kill -9 "$pid" 2>/dev/null
            RUN_TIMEOUT_HIT=1
            break
        fi
        sleep 0.25
    done
    wait "$pid"; RUN_EXIT=$?
    [ "$RUN_TIMEOUT_HIT" = 1 ] && RUN_EXIT=124

    thr1=$(read_throttle)
    if [ "$thr0" != "-" ] && [ "$thr1" != "-" ]; then
        RUN_THROTTLE=$(( thr1 - thr0 ))
    else
        RUN_THROTTLE="-"
    fi

    line=$(grep -E "^[0-9]\." "$out" | head -1)
    RUN_MS=$(printf '%s' "$line" | grep -oE '[0-9]+ ms' | grep -oE '[0-9]+')
    RUN_SUM=$(printf '%s' "$line" | grep -oE '\[[-0-9]+\]' | tr -d '[]')

    # Shutdown summaries the VM already produces, when it produced them.
    RUN_C1=$(grep -oE 'compiles: c1=[0-9]+' "$err" | head -1 | grep -oE '[0-9]+$')
    RUN_C2=$(grep -oE 'c2=[0-9]+ osr=' "$err" | head -1 | grep -oE '[0-9]+' | head -1)
    RUN_OSR=$(grep -oE 'osr=[0-9]+' "$err" | head -1 | grep -oE '[0-9]+')
    RUN_DEOPT=$(grep -oE 'deopts=[0-9]+' "$err" | head -1 | grep -oE '[0-9]+')
    RUN_CTIME=$(grep -oE 'total_compile_time_ms=[0-9]+' "$err" | head -1 | grep -oE '[0-9]+')
    local gcline
    gcline=$(grep -E '^\[GC-SUMMARY\] young ' "$err" | head -1)
    RUN_GCN=$(printf '%s' "$gcline" | grep -oE 'count=[0-9]+' | grep -oE '[0-9]+')
    RUN_GCP50=$(printf '%s' "$gcline" | grep -oE 'p50_us=[0-9]+' | grep -oE '[0-9]+')
    RUN_GCP99=$(printf '%s' "$gcline" | grep -oE 'p99_us=[0-9]+' | grep -oE '[0-9]+')
    RUN_GCMAX=$(printf '%s' "$gcline" | grep -oE 'max_us=[0-9]+' | grep -oE '[0-9]+')
    RUN_MINOR=$(grep -oE 'generational: minor=[0-9]+' "$err" | head -1 | grep -oE '[0-9]+$')
    RUN_MAJOR=$(grep -oE 'minor=[0-9]+ major=[0-9]+' "$err" | head -1 | grep -oE 'major=[0-9]+' | grep -oE '[0-9]+')
    cp "$err" "$RESULTS/stderr-$phase-last.txt" 2>/dev/null
}

FAILURES=0
CAL_OUT=""
echo "CratonBench gate: exe=$EXE cpu=$CPU reps=$REPS tol=${TOL}% load=$LOAD1"
echo "  results: $RESULTS"

# The baseline is slurped first so the measurement loop does not hold the
# file on stdin while it forks children (and so a CRLF checkout cannot leak
# a \r into a checksum comparison).
BASELINE_BODY=$(tr -d '\r' < "$BASELINE" | awk -F'\t' '!/^#/ && NF >= 3 && $1 != "" { print }')

while IFS=$'\t' read -r phase base_ms want_sum status _evidence; do
    [ -n "$phase" ] || continue
    if [ -n "$PHASES" ]; then
        case ",$PHASES," in *",$phase,"*) ;; *) continue ;; esac
    fi
    times=()
    phase_broken=0
    for rep in $(seq 1 "$REPS"); do
        run_one "$phase"
        # The raw sample is recorded FIRST and unconditionally — including
        # for a run that crashed or timed out. A results directory that
        # silently drops the runs that went wrong is how a bad series comes
        # to look like a clean one.
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$phase" "$rep" "$(dash "$RUN_MS")" "$(dash "$RUN_SUM")" \
            "$CPU" "$(dash "$RUN_CPUS")" "$(dash "$RUN_LOAD")" "$(dash "$RUN_THROTTLE")" \
            "$(dash "$RUN_KMIN")" "$(dash "$RUN_KMAX")" "$(dash "$RUN_RSS")" \
            "$(dash "$RUN_C1")" "$(dash "$RUN_C2")" "$(dash "$RUN_OSR")" "$(dash "$RUN_DEOPT")" \
            "$(dash "$RUN_CTIME")" "$(dash "$RUN_GCN")" "$(dash "$RUN_GCP50")" \
            "$(dash "$RUN_GCP99")" "$(dash "$RUN_GCMAX")" "$(dash "$RUN_MINOR")" \
            "$(dash "$RUN_MAJOR")" "$RUN_EXIT" >> "$SAMPLES"

        if [ "$RUN_EXIT" != 0 ] || [ -z "$RUN_MS" ] || [ -z "$RUN_SUM" ]; then
            if [ "$RUN_EXIT" = 124 ]; then
                echo "  $phase: FAIL (rep $rep exceeded --run-timeout ${RUN_TIMEOUT}s and was killed — a hung VM is not a slow one)"
            else
                echo "  $phase: FAIL (rep $rep produced no result — crash or wrong phase name; exit $RUN_EXIT)"
            fi
            FAILURES=$((FAILURES+1)); phase_broken=1; break
        fi
        if [ "$RUN_SUM" != "$want_sum" ]; then
            echo "  $phase: FAIL (checksum $RUN_SUM != expected $want_sum) — CORRECTNESS regression"
            FAILURES=$((FAILURES+1)); phase_broken=1; break
        fi
        times+=("$RUN_MS")
    done
    [ "$phase_broken" = 1 ] && continue

    med=$(median "${times[@]}")
    read -r d_min d_p50 d_p90 d_p99 d_max d_mean d_sd d_cv <<EOD
$(dist "${times[@]}")
EOD
    # Per-phase maxima of the VM-reported counters. "-" when the VM reported
    # nothing (e.g. the generational collector keeps no pause history).
    read -r a_rss a_c1 a_c2 a_gcn a_gcp50 a_gcp99 a_gcmax <<EOD
$(awk -F'\t' -v p="$phase" '
    function v(i) { return (i in m) ? m[i] : "-" }
    !/^#/ && $1 == p {
        split("11 12 13 17 18 19 20", cols, " ")
        for (k in cols) { i = cols[k]; if ($i != "-" && $i + 0 > (i in m ? m[i] : -1)) m[i] = $i + 0 }
    }
    END { printf "%s %s %s %s %s %s %s\n", v(11), v(12), v(13), v(17), v(18), v(19), v(20) }
' "$SAMPLES")
EOD

    if [ "$CALIBRATE" = 1 ]; then
        CAL_OUT="$CAL_OUT$phase\t$med\t$want_sum\tprovisional\tcalibrated $(date +%F) on $(hostname), n=${#times[@]} p50=${d_p50} p99=${d_p99} cv=${d_cv}%\n"
        echo "  $phase: median ${med}ms  p90 ${d_p90}ms  p99 ${d_p99}ms  CV ${d_cv}%  (runs: ${times[*]})"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$phase" "${#times[@]}" "$d_min" "$d_p50" "$d_p90" "$d_p99" "$d_max" "$d_mean" "$d_sd" "$d_cv" \
            "$want_sum" "$base_ms" "$status" "-" "calibrate" \
            "$a_rss" "$a_c1" "$a_c2" "$a_gcn" "$a_gcp50" "$a_gcp99" "$a_gcmax" >> "$SUMMARY"
        continue
    fi

    budget=$(( base_ms + base_ms * TOL / 100 ))
    if [ "$med" -gt "$budget" ]; then
        verdict=FAIL
        echo "  $phase: FAIL median ${med}ms > budget ${budget}ms (baseline ${base_ms}ms +${TOL}%, $status) runs: ${times[*]}"
        echo "         p90 ${d_p90}ms  p99 ${d_p99}ms  min ${d_min}ms  max ${d_max}ms  CV ${d_cv}%  n=${#times[@]}"
        FAILURES=$((FAILURES+1))
    else
        verdict=PASS
        note=""
        improved=$(( base_ms * 90 / 100 ))
        [ "$med" -lt "$improved" ] && note="  (>10% better — consider re-anchoring baseline)"
        echo "  $phase: PASS median ${med}ms <= ${budget}ms$note"
        echo "         p90 ${d_p90}ms  p99 ${d_p99}ms  min ${d_min}ms  max ${d_max}ms  CV ${d_cv}%  n=${#times[@]}"
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$phase" "${#times[@]}" "$d_min" "$d_p50" "$d_p90" "$d_p99" "$d_max" "$d_mean" "$d_sd" "$d_cv" \
        "$want_sum" "$base_ms" "$status" "$budget" "$verdict" \
        "$a_rss" "$a_c1" "$a_c2" "$a_gcn" "$a_gcp50" "$a_gcp99" "$a_gcmax" >> "$SUMMARY"
done <<EOF
$BASELINE_BODY
EOF

# ---------------------------------------------------------------------------
# Results: JSON twins, manifest close-out, reliability postflight
# ---------------------------------------------------------------------------
printf 'load1_end\t%s\n' "$(read_load1)" >> "$MANIFEST"
printf 'finished_utc\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$MANIFEST"
printf 'perf_failures\t%s\n' "$FAILURES" >> "$MANIFEST"

tsv_to_json "$SAMPLES" "$RESULTS/samples.json" samples
tsv_to_json "$SUMMARY" "$RESULTS/summary.json" phases
awk -F'\t' -v schema="$RESULTS_SCHEMA" '
    /^#/ { next }
    NF >= 2 {
        v = $2
        gsub(/\\/, "\\\\", v); gsub(/"/, "\\\"", v)
        rows[++r] = sprintf("  \"%s\": \"%s\"", $1, v)
    }
    END {
        printf "{\n"
        for (i = 1; i <= r; i++) printf "%s%s\n", rows[i], (i < r ? "," : "")
        printf "}\n"
    }
' "$MANIFEST" > "$RESULTS/manifest.json"

RELIABILITY_RC=0
run_reliability postflight || RELIABILITY_RC=$?

if [ "$CALIBRATE" = 1 ]; then
    echo; echo "# calibrated baseline body:"; printf "$CAL_OUT"
    echo "# full distributions: $RESULTS/summary.tsv (+ .json); raw samples: $RESULTS/samples.tsv"
    if [ "$RELIABILITY_RC" != 0 ]; then
        echo "REFUSED: the reliability gate rejected this calibration run (exit $RELIABILITY_RC)." >&2
        echo "         Do NOT record these numbers as a baseline — see" >&2
        echo "         docs/benchmarking/methodology.md 'Recording a new baseline'." >&2
        exit "$RELIABILITY_RC"
    fi
    exit 0
fi
echo "---------------------------------------------"
echo "results: $RESULTS"
echo "         manifest.tsv/.json  samples.tsv/.json  summary.tsv/.json  reliability.json"
# A refused run's PASS is worth exactly as much as its FAIL, so the
# reliability verdict is reported first and wins the exit code.
if [ "$RELIABILITY_RC" != 0 ]; then
    echo "CRATONBENCH GATE: REFUSED — the reliability gate rejected this run (exit $RELIABILITY_RC)."
    echo "The perf comparison above is NOT evidence of anything. See"
    echo "docs/benchmarking/reliability-gate.md for what that exit code means."
    [ "$FAILURES" -gt 0 ] && echo "(for information only: $FAILURES phase(s) were over budget)"
    exit "$RELIABILITY_RC"
fi
if [ "$FAILURES" -gt 0 ]; then
    echo "CRATONBENCH GATE: FAILED ($FAILURES phase(s) regressed)"
    exit 1
fi
echo "CRATONBENCH GATE: PASS"
