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
#                    <script dir>/results/v2/<utc>-<host>-<rev>).
#   --run-timeout S  Kill one measurement after S seconds (default 1800) and
#                    record it as a failed run. A hung VM must never be
#                    recorded as a merely slow one.
#   --no-vm-stats    Do not ask the VM for its shutdown JIT/GC summaries, and
#                    do not record the optimizing tier's per-phase reach.
#                    The GC and JIT summaries are shutdown-only. The reach
#                    (`ir-compiles`) is NOT: it prints one line per compile
#                    REQUEST, on the compile path. That path is entered a
#                    single-digit number of times per phase — which is the
#                    whole point of MEAS-02 — and the whole extra output was
#                    counted at 0-4 lines / 0-419 bytes per phase, against a
#                    shortest phase of 165 ms. See
#                    meas-02-bench-suite-c2-reach-RETIRED-20260803.md
#                    §6. Keep the flag for strict parity with an older result
#                    set, and re-count if a phase ever starts issuing compile
#                    requests in bulk.
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

# Fixed numeric locale for the whole run: the distribution maths is awk, and
# a comma-decimal locale turns "2564.5" into 2564 (or into a parse failure)
# without any error. It is also one more thing that is now identical between
# any two runs being compared, and it is recorded in the manifest.
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RELIABILITY_GATE="$SCRIPT_DIR/reliability-gate.sh"
# Schema 2 (2026-08-03, MEAS-02): samples/summary gained the optimizing
# tier's per-phase reach — ir_requests / ir_admitted / ir_bodies — and the
# manifest gained an `ir_reach_<phase>` line per phase. Every existing column
# kept its name; every consumer resolves columns BY NAME, so a v1 reader
# reads a v2 directory correctly and simply sees no reach. The version moved
# anyway because "this directory records C2 reach" is exactly the kind of
# fact a reader must not have to infer from a column's presence.
RESULTS_SCHEMA=2
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
# `-encoding UTF-8`, explicitly, and not because anybody prefers it.
#
# `export LC_ALL=C` above is right for the awk arithmetic and wrong for javac:
# on a JDK whose javac derives its default SOURCE encoding from the platform
# charset (17 here — JEP 400 moved `file.encoding`, not this), `C` means
# US-ASCII, and `bench/CratonBench.java` contains em-dashes in its header
# comment. The gate then died at setup with 30 `unmappable character (0xE2)`
# errors and `FATAL: bench compile failed`, before measuring anything.
#
# Measured on the Azure bench host 2026-08-03: the mandatory perf gate could
# not compile its own benchmark. The ambient locale there is `C.UTF-8`, so
# the failure appears only INSIDE the gate — running the same javac by hand
# in the same shell succeeds, which is why it survived. Pinning the encoding
# is also the same argument that pinned the locale: one fewer thing that
# differs between two runs being compared.
javac -encoding UTF-8 -d "$CLASSES" "$ROOT/bench/CratonBench.java" \
    || { echo "FATAL: bench compile failed" >&2; exit 2; }

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

# Every TSV cell goes through dash(): empty becomes "-" (which the
# reliability gate reads as "not recorded", never as a value), and any
# embedded tab or newline is flattened, because one stray newline in one cell
# silently splits a sample row into two malformed rows.
dash() {
    local v=${1:-}
    v=${v//$'\n'/ }
    v=${v//$'\r'/ }
    v=${v//$'\t'/ }
    if [ -n "$v" ]; then printf '%s' "$v"; else printf '%s' '-'; fi
}

# MEAS-02: one manifest line per phase saying how far into the optimizing
# tier that phase actually got. It goes in the MANIFEST, not only in the
# summary, because the manifest is what a reader opens to find out what a
# results directory is — and "these numbers were taken on a workload that
# never reached the tier they are being quoted about" belongs there, next to
# the CPU model and the binary hash, rather than in a column somebody has to
# already suspect exists.
REACH_REQ_TOTAL=0
REACH_ADM_TOTAL=0
REACH_BOD_TOTAL=0
REACH_MEASURED=0
REACH_BROKEN=0
record_reach() {  # record_reach <phase> <requests> <admitted> <bodies> <c1> <c2> <osr>
    case "$2$3$4" in
        *-*)
            printf 'ir_reach_%s\tNOT RECORDED (phase produced no usable run)\n' "$1" >> "$MANIFEST"
            return 0
            ;;
    esac

    # The reach is scraped from two `[ir] …` lines the VM prints under
    # `CRATONVM_DBG=ir-compiles`. A scrape fails OPEN: reword either line and
    # every phase records a confident zero, which reads exactly like the
    # finding this record exists to carry. Two independent checks close it.
    #
    # 1. Monotonicity. Every body came from an admitted method and every
    #    admitted method came from a request, so requests >= admitted >=
    #    bodies is a property of the pipeline, not of this run.
    # 2. More non-OSR compiles than requests. `compiles_c1`/`compiles_c2`/
    #    `compiles_osr` come from a DIFFERENT line, emitted by a different
    #    module (the tier manager's shutdown summary), and a non-OSR compile
    #    cannot happen without passing the chain — the C1 ones report
    #    `optimize=false` there and are counted in `requests` too. So
    #    `c1 + c2 - osr > requests` is not a workload fact; it is this scrape
    #    reading a log that no longer says what it expects.
    #
    #    OSR is subtracted, and that correction is measured rather than
    #    assumed. An OSR compile goes through `compile_osr_artifact`, which
    #    calls the backend directly — a second compile door that does not
    #    pass this chain — but the tier manager still counts it under the
    #    TIER it was requested at, so a C2-tier OSR compile lands in `c2=`.
    #    On the Azure bench host 2026-08-03 the `arithmetic` phase reports
    #    `c1=0 c2=1 osr=1` with zero admission lines: that single C2 compile
    #    IS the OSR one. Without the subtraction this check called that
    #    phase's genuine reach of zero a broken scrape — which is what a
    #    phase that is one long loop inside one method looks like, and that
    #    is most of CratonBench.
    _c1=${5:--}; _c2=${6:--}; _osr=${7:--}
    [ "$_c1" = "-" ] && _c1=0
    [ "$_c2" = "-" ] && _c2=0
    [ "$_osr" = "-" ] && _osr=0
    _nonosr=$(( _c1 + _c2 - _osr ))
    [ "$_nonosr" -lt 0 ] && _nonosr=0
    _suffix=""
    if [ "$2" -lt "$3" ] || [ "$3" -lt "$4" ]; then
        echo "  $1: REACH SCRAPE BROKEN — requests=$2 admitted=$3 bodies=$4 is not monotone." >&2
        _suffix=" SCRAPE-BROKEN (not monotone)"
        REACH_BROKEN=$((REACH_BROKEN + 1))
    elif [ "$_nonosr" -gt "$2" ]; then
        echo "  $1: REACH SCRAPE BROKEN — the tier manager reports c1=$_c1 c2=$_c2 osr=$_osr" >&2
        echo "      ($_nonosr non-OSR compiles) but only $2 request(s) reached the admission" >&2
        echo "      chain. The '[ir] admission' line in jit/src/lib.rs has moved or been" >&2
        echo "      reworded; this run's reach must not be read as measured." >&2
        _suffix=" SCRAPE-BROKEN ($_nonosr non-OSR compiles vs $2 requests; NOT measured)"
        REACH_BROKEN=$((REACH_BROKEN + 1))
    fi

    # The caveat goes on the PHASE's own line, not only in a run-level tally.
    # A broken scrape's per-phase reading is `requests=0 admitted=0 bodies=0`,
    # which is character-for-character what the expected finding looks like —
    # so a reader who greps one phase out of the manifest has to be told
    # there, or they will not be told at all.
    printf 'ir_reach_%s\trequests=%s admitted=%s bodies=%s%s\n' \
        "$1" "$2" "$3" "$4" "$_suffix" >> "$MANIFEST"

    REACH_REQ_TOTAL=$(( REACH_REQ_TOTAL + $2 ))
    REACH_ADM_TOTAL=$(( REACH_ADM_TOTAL + $3 ))
    REACH_BOD_TOTAL=$(( REACH_BOD_TOTAL + $4 ))
    REACH_MEASURED=$(( REACH_MEASURED + 1 ))
}

# The reach, said out loud at the end of a run rather than left in a TSV.
# The message a reader needs is not the counts; it is what the counts license
# them to conclude, so it says that instead of making them work it out.
print_reach_note() {
    if [ "$VM_STATS" != 1 ]; then
        echo "optimizing-tier reach: NOT RECORDED (--no-vm-stats)."
        echo "         This run cannot support any claim about the C2 tier."
        return 0
    fi
    echo "optimizing-tier reach (MEAS-02): requests=$REACH_REQ_TOTAL admitted=$REACH_ADM_TOTAL bodies=$REACH_BOD_TOTAL across $REACH_MEASURED phase(s)"
    if [ "$REACH_BROKEN" -gt 0 ]; then
        echo "         NOT TRUSTWORTHY: $REACH_BROKEN phase(s) failed the scrape's own"
        echo "         consistency checks (see the REACH SCRAPE BROKEN lines above). Fix the"
        echo "         scrape before reading any reach number from this run — a broken scrape"
        echo "         reports zeros, and zero is the answer this record is usually expected"
        echo "         to give, so it is the one value nobody double-checks."
        return 0
    fi
    if [ "$REACH_BOD_TOTAL" -eq 0 ]; then
        echo "         The optimizing backend produced NO bodies in this run. Every"
        echo "         number above measures the single-pass backend, and none of"
        echo "         them is evidence about C2 in either direction."
    else
        echo "         Per phase: manifest.tsv 'ir_reach_<phase>'. A delta on a phase"
        echo "         whose bodies=0 says nothing about the optimizing tier."
    fi
}

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
FREQ_FILE="/sys/devices/system/cpu/cpu$CPU/cpufreq/scaling_cur_freq"
THROTTLE_FILE="/sys/devices/system/cpu/cpu$CPU/thermal_throttle/core_throttle_count"
read_throttle() { read_sysfs "$THROTTLE_FILE"; }

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
    printf 'locale\t%s\n' "$LC_ALL"
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
printf '#phase\trep\tms\tchecksum\tcpu_pinned\tcpu_observed\tload1\tthrottle_delta\tkhz_min\tkhz_max\tpeak_rss_kb\tcompiles_c1\tcompiles_c2\tcompiles_osr\tdeopts\tcompile_ms\tgc_young_count\tgc_young_p50_us\tgc_young_p99_us\tgc_young_max_us\tgc_minor\tgc_major\texit_code\tir_requests\tir_admitted\tir_bodies\n' > "$SAMPLES"
printf '#phase\tn\tmin_ms\tp50_ms\tp90_ms\tp99_ms\tmax_ms\tmean_ms\tstddev_ms\tcv_pct\tchecksum\tbaseline_ms\tbaseline_status\tbudget_ms\tverdict\tpeak_rss_kb_max\tcompiles_c1_max\tcompiles_c2_max\tgc_young_count_max\tgc_young_p50_us_max\tgc_young_p99_us_max\tgc_young_max_us_max\tir_requests_max\tir_admitted_max\tir_bodies_max\n' > "$SUMMARY"

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
    local thr0 thr1 pid line c khz started
    local -a arr
    RUN_MS=""; RUN_SUM=""; RUN_EXIT=0
    RUN_CPUS=""; RUN_KMIN=""; RUN_KMAX=""; RUN_RSS=""; RUN_TIMEOUT_HIT=0
    RUN_LOAD=$(read_load1)
    thr0=$(read_throttle)

    if [ "$VM_STATS" = 1 ]; then
        # Grouped spelling. The per-flag names (CRATONVM_GC_STATS=1,
        # CRATONVM_DBG_JIT_METHOD_STATS=1) still work, but every run made the
        # VM print a deprecation line as the FIRST line of the stderr this
        # script then parses — and a harness whose stderr opens with a
        # configuration warning is one nobody reads twice.
        env CRATONVM_DBG='gc-stats,jit-method-stats,ir-compiles' \
            taskset -c "$CPU" "$EXE" $VM_FLAGS -cp "$CLASSES" CratonBench "$phase" \
            >"$out" 2>"$err" </dev/null &
    else
        taskset -c "$CPU" "$EXE" $VM_FLAGS -cp "$CLASSES" CratonBench "$phase" \
            >"$out" 2>"$err" </dev/null &
    fi
    pid=$!
    # SECONDS and the `read` builtin, not `date` and `cat`: the poll loop runs
    # while the measurement is running, and every fork it makes is a process
    # the scheduler can place on the very core we are trying to keep clean.
    started=$SECONDS

    while kill -0 "$pid" 2>/dev/null; do
        # /proc/<pid>/stat field 39 is the CPU the task last ran on. comm can
        # contain spaces and parentheses, so everything up to the last ") "
        # is dropped first and the remainder is indexed from field 3.
        # 2>/dev/null BEFORE the input redirection, not after: redirections are
        # applied left to right, so the other order lets the "No such file"
        # from a process that exited between `kill -0` and here reach the
        # console.
        if IFS= read -r line 2>/dev/null < "/proc/$pid/stat"; then
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
        if [ -r "$FREQ_FILE" ] && IFS= read -r khz < "$FREQ_FILE" && [ -n "$khz" ]; then
            [ -n "$RUN_KMIN" ] || RUN_KMIN=$khz
            [ -n "$RUN_KMAX" ] || RUN_KMAX=$khz
            [ "$khz" -lt "$RUN_KMIN" ] && RUN_KMIN=$khz
            [ "$khz" -gt "$RUN_KMAX" ] && RUN_KMAX=$khz
        fi
        # VmHWM is the kernel's own high-water mark, so sampling it at any
        # cadence still yields the true peak as long as we read it once.
        if [ -r "/proc/$pid/status" ]; then
            while IFS= read -r line; do
                case "$line" in
                    VmHWM:*)
                        line=${line//[!0-9]/}
                        if [ -n "$line" ] && { [ -z "$RUN_RSS" ] || [ "$line" -gt "$RUN_RSS" ]; }; then
                            RUN_RSS=$line
                        fi
                        ;;
                esac
            done < "/proc/$pid/status"
        fi
        if [ $((SECONDS - started)) -ge "$RUN_TIMEOUT" ]; then
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
    # Every extraction anchors the digits with `[0-9]+$` on a single matched
    # token: `grep -oE 'p50_us=[0-9]+' | grep -oE '[0-9]+'` returns TWO lines
    # ("50" and "9800"), which is exactly how a stray newline gets into a TSV
    # cell and silently splits a sample row in half.
    local cseg gcline
    cseg=$(grep -oE 'compiles: c1=[0-9]+ c2=[0-9]+ osr=[0-9]+ deopts=[0-9]+ c2_bailouts=[0-9]+ total_compile_time_ms=[0-9]+' "$err" | head -1)
    RUN_C1=$(printf '%s' "$cseg" | grep -oE 'c1=[0-9]+' | head -1 | grep -oE '[0-9]+$')
    RUN_C2=$(printf '%s' "$cseg" | grep -oE ' c2=[0-9]+' | head -1 | grep -oE '[0-9]+$')
    RUN_OSR=$(printf '%s' "$cseg" | grep -oE 'osr=[0-9]+' | head -1 | grep -oE '[0-9]+$')
    RUN_DEOPT=$(printf '%s' "$cseg" | grep -oE 'deopts=[0-9]+' | head -1 | grep -oE '[0-9]+$')
    RUN_CTIME=$(printf '%s' "$cseg" | grep -oE 'total_compile_time_ms=[0-9]+' | head -1 | grep -oE '[0-9]+$')
    gcline=$(grep -E '^\[GC-SUMMARY\] young ' "$err" | head -1)
    RUN_GCN=$(printf '%s' "$gcline" | grep -oE 'count=[0-9]+' | head -1 | grep -oE '[0-9]+$')
    RUN_GCP50=$(printf '%s' "$gcline" | grep -oE 'p50_us=[0-9]+' | head -1 | grep -oE '[0-9]+$')
    RUN_GCP99=$(printf '%s' "$gcline" | grep -oE 'p99_us=[0-9]+' | head -1 | grep -oE '[0-9]+$')
    RUN_GCMAX=$(printf '%s' "$gcline" | grep -oE ' max_us=[0-9]+' | head -1 | grep -oE '[0-9]+$')
    RUN_MINOR=$(grep -oE 'generational: minor=[0-9]+' "$err" | head -1 | grep -oE '[0-9]+$')
    RUN_MAJOR=$(grep -oE 'major=[0-9]+' "$err" | head -1 | grep -oE '[0-9]+$')

    # MEAS-02: the OPTIMIZING tier's reach, per phase.
    #
    # `compiles_c2` above is not this number and must never be read as it.
    # `TieredCompiler`'s `c2_compilations` counts a successful compile whose
    # requested TIER was C2 — including every one that entered the optimizing
    # pipeline, was declined by it, and had its body produced by the
    # single-pass backend instead. A phase can therefore report `c2=5` while
    # the optimizing backend emitted nothing at all, which is precisely the
    # inference MEAS-02 exists to stop.
    #
    # Three counts, from the three points a request can die at:
    #   requests — reached the admission chain at all (C1 requests included:
    #              `optimize=false` is the commonest verdict and is normal
    #              tiering, not a gap)
    #   admitted — the chain's verdict was "admitted to the optimizing
    #              pipeline"
    #   bodies   — the optimizing backend actually produced one
    # `admitted - bodies` is the pipeline admitting a method it then cannot
    # lower; `requests - admitted` is the admission chain declining up front.
    #
    # `grep -c` is deliberate over `grep | wc -l`: an empty match is 0 with a
    # non-zero exit, which `dash()` must NOT turn into "-" here. A phase that
    # reaches the tier zero times is a measured zero, not an unrecorded field
    # — telling those two apart is the whole deliverable.
    if [ "$VM_STATS" = 1 ]; then
        RUN_IRREQ=$(grep -c '^\[ir\] admission ' "$err" 2>/dev/null)
        RUN_IRADM=$(grep -c ': admitted to the optimizing pipeline$' "$err" 2>/dev/null)
        RUN_IRBOD=$(grep -c '^\[ir\] optimizing backend produced a body ' "$err" 2>/dev/null)
        RUN_IRREQ=${RUN_IRREQ:-0}; RUN_IRADM=${RUN_IRADM:-0}; RUN_IRBOD=${RUN_IRBOD:-0}
    else
        RUN_IRREQ="-"; RUN_IRADM="-"; RUN_IRBOD="-"
    fi

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
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$phase" "$rep" "$(dash "$RUN_MS")" "$(dash "$RUN_SUM")" \
            "$CPU" "$(dash "$RUN_CPUS")" "$(dash "$RUN_LOAD")" "$(dash "$RUN_THROTTLE")" \
            "$(dash "$RUN_KMIN")" "$(dash "$RUN_KMAX")" "$(dash "$RUN_RSS")" \
            "$(dash "$RUN_C1")" "$(dash "$RUN_C2")" "$(dash "$RUN_OSR")" "$(dash "$RUN_DEOPT")" \
            "$(dash "$RUN_CTIME")" "$(dash "$RUN_GCN")" "$(dash "$RUN_GCP50")" \
            "$(dash "$RUN_GCP99")" "$(dash "$RUN_GCMAX")" "$(dash "$RUN_MINOR")" \
            "$(dash "$RUN_MAJOR")" "$RUN_EXIT" \
            "$(dash "$RUN_IRREQ")" "$(dash "$RUN_IRADM")" "$(dash "$RUN_IRBOD")" >> "$SAMPLES"

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
    if [ "$phase_broken" = 1 ]; then
        # A broken phase has no reach, as opposed to a reach of zero. Say so
        # rather than leaving the key out: an absent key reads as "the gate
        # predates this record", which is the wrong conclusion here.
        record_reach "$phase" - - -
        continue
    fi

    med=$(median "${times[@]}")
    read -r d_min d_p50 d_p90 d_p99 d_max d_mean d_sd d_cv <<EOD
$(dist "${times[@]}")
EOD
    # Per-phase maxima of the VM-reported counters. "-" when the VM reported
    # nothing (e.g. the generational collector keeps no pause history).
    #
    # Columns are resolved from the header row BY NAME. They used to be a
    # hardcoded index list (`split("11 12 13 17 18 19 20", ...)`) sitting a
    # hundred lines away from the `printf` that decides what column 11 is —
    # so adding one column in the middle of the sample row would have
    # silently re-pointed every maximum at its neighbour, with no error and
    # no obviously wrong number.
    read -r a_rss a_c1 a_c2 a_osr a_gcn a_gcp50 a_gcp99 a_gcmax a_irreq a_iradm a_irbod <<EOD
$(awk -F'\t' -v p="$phase" \
    -v want="peak_rss_kb compiles_c1 compiles_c2 compiles_osr gc_young_count gc_young_p50_us gc_young_p99_us gc_young_max_us ir_requests ir_admitted ir_bodies" '
    BEGIN { nw = split(want, wname, " ") }
    /^#/ && NR == 1 {
        for (i = 1; i <= NF; i++) { h = $i; sub(/^#/, "", h); idx[h] = i }
        for (k = 1; k <= nw; k++) if (!(wname[k] in idx)) missing = missing " " wname[k]
        if (missing != "") { printf "samples.tsv header has no column(s):%s\n", missing > "/dev/stderr" }
        next
    }
    /^#/ { next }
    $1 == p {
        for (k = 1; k <= nw; k++) {
            i = idx[wname[k]]
            if (i == "" || $i == "-" || $i == "") continue
            if (!(k in m) || $i + 0 > m[k]) m[k] = $i + 0
        }
    }
    END {
        for (k = 1; k <= nw; k++) printf "%s%s", (k in m) ? m[k] "" : "-", (k < nw ? " " : "\n")
    }
' "$SAMPLES")
EOD

    if [ "$CALIBRATE" = 1 ]; then
        CAL_OUT="$CAL_OUT$phase\t$med\t$want_sum\tprovisional\tcalibrated $(date +%F) on $(hostname), n=${#times[@]} p50=${d_p50} p99=${d_p99} cv=${d_cv}%\n"
        echo "  $phase: median ${med}ms  p90 ${d_p90}ms  p99 ${d_p99}ms  CV ${d_cv}%  (runs: ${times[*]})"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$phase" "${#times[@]}" "$d_min" "$d_p50" "$d_p90" "$d_p99" "$d_max" "$d_mean" "$d_sd" "$d_cv" \
            "$want_sum" "$base_ms" "$status" "-" "calibrate" \
            "$a_rss" "$a_c1" "$a_c2" "$a_gcn" "$a_gcp50" "$a_gcp99" "$a_gcmax" \
            "$a_irreq" "$a_iradm" "$a_irbod" >> "$SUMMARY"
        record_reach "$phase" "$a_irreq" "$a_iradm" "$a_irbod" "$a_c1" "$a_c2" "$a_osr"
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
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$phase" "${#times[@]}" "$d_min" "$d_p50" "$d_p90" "$d_p99" "$d_max" "$d_mean" "$d_sd" "$d_cv" \
        "$want_sum" "$base_ms" "$status" "$budget" "$verdict" \
        "$a_rss" "$a_c1" "$a_c2" "$a_gcn" "$a_gcp50" "$a_gcp99" "$a_gcmax" \
        "$a_irreq" "$a_iradm" "$a_irbod" >> "$SUMMARY"
    record_reach "$phase" "$a_irreq" "$a_iradm" "$a_irbod" "$a_c1" "$a_c2" "$a_osr"
done <<EOF
$BASELINE_BODY
EOF

# ---------------------------------------------------------------------------
# Results: JSON twins, manifest close-out, reliability postflight
# ---------------------------------------------------------------------------
printf 'load1_end\t%s\n' "$(read_load1)" >> "$MANIFEST"
printf 'finished_utc\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$MANIFEST"
printf 'perf_failures\t%s\n' "$FAILURES" >> "$MANIFEST"
if [ "$VM_STATS" = 1 ]; then
    printf 'ir_reach_recorded\tyes\n' >> "$MANIFEST"
else
    printf 'ir_reach_recorded\tno (--no-vm-stats)\n' >> "$MANIFEST"
fi
printf 'ir_reach_total\trequests=%s admitted=%s bodies=%s over %s phase(s)\n' \
    "$REACH_REQ_TOTAL" "$REACH_ADM_TOTAL" "$REACH_BOD_TOTAL" "$REACH_MEASURED" >> "$MANIFEST"
printf 'ir_reach_scrape_broken\t%s\n' "$REACH_BROKEN" >> "$MANIFEST"

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
    # Calibration is the moment the reach matters most: a baseline is a
    # commitment that future deltas against it will be read as meaning
    # something, and this says what they will be able to mean.
    echo
    print_reach_note
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
print_reach_note
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
