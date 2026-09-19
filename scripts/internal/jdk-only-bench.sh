#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jdk-only-bench.sh — the JDK-only mode benchmark harness.
#
# Implements the metric table and the proposed non-regression budgets in
# docs/benchmarks/jdk-only.md. That document is normative; this script does not
# get to invent metrics, and where the two disagree the document wins and the
# disagreement is a bug report, not a local fix.
#
# See bench/jdk-only/README.md for what each metric means, how to regenerate a
# baseline, and how to read a regression.
#
#   bash scripts/jdk-only-bench.sh --help
#
# THE CENTRAL DESIGN RULE OF THIS FILE
# ------------------------------------
# `--jdk-only` moves work in two opposite directions at once: more real JDK
# bytecode is interpreted (cost) while thousands of native registrations and
# lookups disappear (saving). A single wall-clock number hides the mechanism,
# so every metric is emitted separately and nothing is aggregated.
#
# The second rule follows from the first: a missing or failed input is an
# EXPLICIT GAP, never a fabricated or zero-filled number. A strict run that
# fails to boot is a recorded outcome, not a 0 ms startup — wave 1 of JDK-only
# mode is diagnostic-only and such failures are expected data. Every emitted
# row carries a `status` of `ok`, `gap` or `fail`, and `value` is `-` for the
# latter two. There is no path in this file that writes a number it did not
# measure, and finalise() re-checks that property before the file is published.
#
# OUTPUT
#   <out>/results.tsv        the diffable result set: stable ordering, no
#                            timestamps, no absolute paths. Committable.
#   <out>/run-metadata.txt   host, OS, CPU, JDK, VM commit, harness settings —
#                            deliberately NOT inside results.tsv, which would
#                            otherwise differ on every row of every run.
#   <out>/gate.txt           budget verdict, when --gate is used.
#   <out>/drift.txt          baseline comparison, when --baseline is used.
#   <out>/raw/               per-run stdout/stderr and JSON dumps. May contain
#                            absolute paths; never diffed.
#
# EXIT CODES
#   0  measurement completed (with or without gaps); gate passed if requested
#   1  a requested gate failed
#   3  a prerequisite is missing — nothing ran
#   4  internal consistency check failed (the harness caught itself lying)

set -u
set -o pipefail

# Determinism: C collation for sort, C numerics for EPOCHREALTIME's decimal
# separator (a de_DE locale renders it with a comma, which would silently turn
# every startup sample into garbage).
LC_ALL=C
export LC_ALL

# MSYS/Git Bash rewrites arguments that look like POSIX paths. Same guard the
# census script and the regression suite use.
MSYS2_ARG_CONV_EXCL='*'
MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL MSYS_NO_PATHCONV

TAB=$'\t'

# ---------------------------------------------------------------------------
# Repository layout
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$ROOT" ]; then
    ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi

BENCH_DIR="$ROOT/bench/jdk-only"
CRATONBENCH_SRC="$ROOT/bench/CratonBench.java"
# Checksum oracle of last resort. Owned by the CratonBench performance gate,
# read-only here — reusing it is what lets this harness still enforce the house
# "checksums must match" rule on a host where HotSpot was not run.
CRATONBENCH_CHECKSUMS="$ROOT/regression-suite/perf/cratonbench-baseline-azure-epyc.tsv"
VECTOR_SRC_DIR="$ROOT/regression-suite/src"
# Trivial startup vector: the already-compiled HelloWorld the vm-cli tests
# already run, and the exact program docs/benchmarks/jdk-only.md names for
# metric 1. Reused rather than re-authored so there is no second definition of
# "trivial main" to drift.
HELLO_CP="$ROOT/vm-cli/tests/resources"
HELLO_CLASS="HelloWorld"

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------

OUT="${OUT:-$ROOT/target/jdk-only-bench}"
CV="${CV:-}"
JDK="${JDK:-${JAVA_HOME:-}}"
LAUNCHES=20
WARMUPS=3
MEM_LAUNCHES=5
REPS=3
TIMEOUT_SEC=300
PHASES="arithmetic,fib,sieve,matrix,hashmap,stringregex,bintrees"
# Reflection, concurrency and NIO are not CratonBench phases. Rather than
# author a competing set of Java programs, the harness times the existing
# RJdk* corpus. These are the vectors that need neither a module path nor
# staged META-INF/services resources, so they run from a plain class path —
# RJdkModule and RJdkServices are excluded for exactly that reason.
VECTORS="RJdkCollections,RJdkReflect,RJdkExecutors,RJdkForkJoin,RJdkNio"
# NOT named GROUPS: that is a bash special array holding the caller's group IDs,
# and assignments to it are SILENTLY IGNORED. The obvious spelling turns
# --groups into a no-op that then dies on "unknown group: 197121".
GROUP_LIST="startup,memory,census,jit,throughput,vectors,gc,growth,size"
MODES="jdk-only,real-jdk,hotspot"
JAVAC_RELEASE=""
HOST_LABEL=""
BUDGETS="$BENCH_DIR/budgets.tsv"
BASELINE=""
DO_GATE=0
GATE_DRIFT=0
DRIFT_TOLERANCE=10

usage() {
    sed -n '4,46p' "$0" | sed 's/^#\{1,\} \{0,1\}//'
    cat <<'EOF'

OPTIONS
  --out DIR              output directory (default target/jdk-only-bench)
  --cv PATH              cratonvm launcher (default target/release then debug)
  --jdk PATH             JDK runtime image (default $JAVA_HOME)
  --groups LIST          comma list of metric groups. Default: all of
                         startup,memory,census,jit,throughput,vectors,gc,growth,size
  --modes LIST           comma list of jdk-only,real-jdk,hotspot (default all)
  --launches N           startup launches per mode (default 20, the doc's >=20)
  --warmups N            discarded launches before timing (default 3)
  --mem-launches N       peak-memory launches per mode (default 5)
  --reps N               repetitions per throughput phase / vector (default 3)
  --phases LIST          CratonBench phases (default all seven)
  --vectors LIST         RJdk* vectors to time end-to-end
  --javac-release N      pass `javac --release N` (default: javac's own level)
  --timeout SEC          per-launch timeout (default 300). A timeout is a
                         recorded outcome, not a discarded sample.
  --label TAG            host tag recorded in run-metadata.txt
  --budgets FILE         budget table (default bench/jdk-only/budgets.tsv)
  --gate                 evaluate the budgets; exit 1 on breach
  --baseline FILE        diff against a committed baseline results.tsv
  --gate-drift           make baseline drift beyond --drift-tolerance fail
  --drift-tolerance PCT  default 10
  -h, --help             this text
EOF
}

die() { echo "ERROR: $*" >&2; exit 3; }

require_posint() {
    case "$2" in
        '' | *[!0-9]*) die "$1 expects a non-negative integer, got '$2'" ;;
    esac
}

while [ $# -gt 0 ]; do
    case "$1" in
        --out)             [ $# -ge 2 ] || die "--out needs a value"; OUT="$2"; shift 2 ;;
        --cv)              [ $# -ge 2 ] || die "--cv needs a value"; CV="$2"; shift 2 ;;
        --jdk)             [ $# -ge 2 ] || die "--jdk needs a value"; JDK="$2"; shift 2 ;;
        --groups)          [ $# -ge 2 ] || die "--groups needs a value"; GROUP_LIST="$2"; shift 2 ;;
        --modes)           [ $# -ge 2 ] || die "--modes needs a value"; MODES="$2"; shift 2 ;;
        --launches)        [ $# -ge 2 ] || die "--launches needs a value"; require_posint --launches "$2"; LAUNCHES="$2"; shift 2 ;;
        --warmups)         [ $# -ge 2 ] || die "--warmups needs a value"; require_posint --warmups "$2"; WARMUPS="$2"; shift 2 ;;
        --mem-launches)    [ $# -ge 2 ] || die "--mem-launches needs a value"; require_posint --mem-launches "$2"; MEM_LAUNCHES="$2"; shift 2 ;;
        --reps)            [ $# -ge 2 ] || die "--reps needs a value"; require_posint --reps "$2"; REPS="$2"; shift 2 ;;
        --phases)          [ $# -ge 2 ] || die "--phases needs a value"; PHASES="$2"; shift 2 ;;
        --vectors)         [ $# -ge 2 ] || die "--vectors needs a value"; VECTORS="$2"; shift 2 ;;
        --javac-release)   [ $# -ge 2 ] || die "--javac-release needs a value"; require_posint --javac-release "$2"; JAVAC_RELEASE="$2"; shift 2 ;;
        --timeout)         [ $# -ge 2 ] || die "--timeout needs a value"; require_posint --timeout "$2"; TIMEOUT_SEC="$2"; shift 2 ;;
        --label)           [ $# -ge 2 ] || die "--label needs a value"; HOST_LABEL="$2"; shift 2 ;;
        --budgets)         [ $# -ge 2 ] || die "--budgets needs a value"; BUDGETS="$2"; shift 2 ;;
        --baseline)        [ $# -ge 2 ] || die "--baseline needs a value"; BASELINE="$2"; shift 2 ;;
        --drift-tolerance) [ $# -ge 2 ] || die "--drift-tolerance needs a value"; require_posint --drift-tolerance "$2"; DRIFT_TOLERANCE="$2"; shift 2 ;;
        --gate)            DO_GATE=1; shift ;;
        --gate-drift)      GATE_DRIFT=1; shift ;;
        -h | --help)       usage; exit 0 ;;
        *)                 die "unknown argument: $1 (try --help)" ;;
    esac
done

[ "$LAUNCHES" -gt 0 ]     || die "--launches must be > 0"
[ "$MEM_LAUNCHES" -gt 0 ] || die "--mem-launches must be > 0"
[ "$REPS" -gt 0 ]         || die "--reps must be > 0"

has_group() { case ",$GROUP_LIST," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }
has_mode()  { case ",$MODES,"  in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

find_cv() {
    if [ -n "$CV" ]; then printf '%s\n' "$CV"; return; fi
    for c in "$ROOT/target/release/cratonvm" "$ROOT/target/release/cratonvm.exe" \
             "$ROOT/target/debug/cratonvm" "$ROOT/target/debug/cratonvm.exe"; do
        if [ -x "$c" ]; then printf '%s\n' "$c"; return; fi
    done
}

CV="$(find_cv)"
if [ -z "$CV" ] || [ ! -x "$CV" ]; then
    die "cratonvm binary not found under $ROOT/target/{release,debug}.
       Build it (cargo build --release -p cratonvm-cli) or pass --cv <path>."
fi

# `--jdk-only` requires a real runtime image and refuses to start without one,
# so a missing JDK cannot be degraded around: the harness would emit a full set
# of failures that a careless reader would mistake for a slow VM.
[ -n "$JDK" ] || die "no JDK: set JAVA_HOME or pass --jdk <path>"
if [ ! -d "$JDK/jmods" ] && [ ! -f "$JDK/lib/modules" ]; then
    die "$JDK does not look like a JDK runtime image (no jmods/, no lib/modules)"
fi

JAVA="$JDK/bin/java";   [ -x "$JAVA" ]  || JAVA="$JDK/bin/java.exe"
JAVAC="$JDK/bin/javac"; [ -x "$JAVAC" ] || JAVAC="$JDK/bin/javac.exe"
[ -x "$JAVA" ] || die "java not found under $JDK/bin"

RAW="$OUT/raw"
mkdir -p "$RAW" || die "cannot create $RAW"
RESULTS="$OUT/results.tsv"
RESULTS_RAW="$OUT/.results.unsorted"
: > "$RESULTS_RAW" || die "cannot write into $OUT"

# Alternate slash spellings, for redaction and for the leak self-check: Git
# Bash hands us /c/craton/... while JAVA_HOME is usually C:\Program Files\...,
# and either spelling can end up in a diagnostic string.
ROOT_ALT="$(printf '%s' "$ROOT" | tr '\\' '/')"
JDK_ALT="$(printf '%s' "$JDK" | tr '\\' '/')"

# ---------------------------------------------------------------------------
# Platform detection
# ---------------------------------------------------------------------------

UNAME_S="$(uname -s 2>/dev/null || echo unknown)"
case "$UNAME_S" in
    Linux*)                   PLATFORM=linux ;;
    MINGW* | MSYS* | CYGWIN*) PLATFORM=windows ;;
    Darwin*)                  PLATFORM=macos ;;
    *)                        PLATFORM=other ;;
esac

# Peak-memory backend. Each supported platform gets its own; a platform with
# neither is reported as a gap with the reason spelled out, never skipped.
MEM_BACKEND=none
MEM_BACKEND_WHY="no peak-memory source on platform $PLATFORM"
GNU_TIME=""
POWERSHELL=""
if [ "$PLATFORM" = linux ]; then
    for t in /usr/bin/time /bin/time; do
        if [ -x "$t" ] && "$t" -f '%M' true >/dev/null 2>&1; then GNU_TIME="$t"; break; fi
    done
    if [ -n "$GNU_TIME" ]; then
        MEM_BACKEND=gnu-time
        MEM_BACKEND_WHY=""
    else
        MEM_BACKEND_WHY="GNU time not found; the bash builtin and busybox time cannot report max RSS"
    fi
elif [ "$PLATFORM" = windows ]; then
    for ps in powershell.exe pwsh.exe powershell pwsh; do
        if command -v "$ps" >/dev/null 2>&1; then POWERSHELL="$ps"; break; fi
    done
    if [ -n "$POWERSHELL" ] && [ -f "$BENCH_DIR/peak-memory.ps1" ]; then
        MEM_BACKEND=powershell
        MEM_BACKEND_WHY=""
    elif [ -z "$POWERSHELL" ]; then
        MEM_BACKEND_WHY="powershell not on PATH; /usr/bin/time -v does not exist on Windows"
    else
        MEM_BACKEND_WHY="bench/jdk-only/peak-memory.ps1 missing"
    fi
elif [ "$PLATFORM" = macos ]; then
    MEM_BACKEND_WHY="macOS is outside this harness's stated scope (Linux and Windows); /usr/bin/time -l uses a different format and a different unit"
fi

# Wall-clock source. EPOCHREALTIME is a bash builtin (no fork, microsecond
# resolution) and is present on both CI legs; date +%s%N is the fallback. A
# host with neither gets a recorded gap for every timed metric rather than
# second-resolution noise dressed up as a measurement.
TIMER=none
if [ -n "${EPOCHREALTIME:-}" ] && [ "${EPOCHREALTIME#*.}" != "${EPOCHREALTIME}" ]; then
    TIMER=epochrealtime
elif [ "$(date +%N 2>/dev/null)" != "N" ] && [ -n "$(date +%N 2>/dev/null)" ]; then
    TIMER=date-ns
fi

now_us() {
    local t
    case "$TIMER" in
        epochrealtime) t="${EPOCHREALTIME}"; printf '%s' "${t/./}" ;;
        date-ns)       t="$(date +%s%N)";    printf '%s' "${t%???}" ;;
        *)             printf '0' ;;
    esac
}

TIMEOUT_BIN=""
command -v timeout >/dev/null 2>&1 && TIMEOUT_BIN="timeout"

# ---------------------------------------------------------------------------
# Emission — the only way a row reaches results.tsv
# ---------------------------------------------------------------------------

ROWS_OK=0
ROWS_GAP=0
ROWS_FAIL=0

# Literal (non-regex) path redaction. Windows paths contain backslashes, which
# would be interpreted as escapes by sed, so this uses awk's index/substr.
redact() {
    printf '%s\n' "$1" | awk \
        -v out="$OUT" -v cv="$CV" -v root="$ROOT" -v rootalt="$ROOT_ALT" \
        -v jdk="$JDK" -v jdkalt="$JDK_ALT" '
        function repl(s, needle, tok,   i, r) {
            if (needle == "") return s
            r = ""
            while ((i = index(s, needle)) > 0) {
                r = r substr(s, 1, i - 1) tok
                s = substr(s, i + length(needle))
            }
            return r s
        }
        {
            s = $0
            s = repl(s, out, "<out>")
            s = repl(s, cv, "<cratonvm>")
            s = repl(s, jdk, "<jdk>")
            s = repl(s, jdkalt, "<jdk>")
            s = repl(s, root, "<root>")
            s = repl(s, rootalt, "<root>")
            gsub(/\t/, " ", s)
            print s
        }'
}

emit() { # metric mode unit status value low high n detail
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$(redact "${9:-}")" >> "$RESULTS_RAW"
    case "$4" in
        ok)   ROWS_OK=$((ROWS_OK + 1)) ;;
        gap)  ROWS_GAP=$((ROWS_GAP + 1)) ;;
        fail) ROWS_FAIL=$((ROWS_FAIL + 1)) ;;
    esac
}

emit_ok()   { emit "$1" "$2" "$3" ok "$4" "${5:--}" "${6:--}" "${7:-1}" "${8:-}"; }
# A gap is "the harness could not obtain this number, and here is exactly why".
emit_gap()  { emit "$1" "$2" "$3" gap  - - - - "$4"; }
# A fail is "the input existed and did not work". Distinct from a gap on
# purpose: a strict run that refuses to boot is a finding, not a limitation.
emit_fail() { emit "$1" "$2" "$3" fail - - - - "$4"; }

# Every group emits exactly one of these per mode it was asked about, so a
# group that produced nothing is visible in the results file rather than being
# an absence nobody notices.
emit_group() { emit "group.$1.status" "$2" state "$3" - - - - "$4"; }

# ---------------------------------------------------------------------------
# Statistics
# ---------------------------------------------------------------------------
#
# Median with an interquartile spread, never a bare mean: one scheduler hiccup
# ruins a mean over twenty samples, and the doc says so explicitly. Percentiles
# are nearest-rank on the sorted sample; below n = 4 the quartiles collapse
# onto the extremes, which is honest — they are min/max at that point.
stats() { # stdin: one number per line -> "median p25 p75 n"
    sort -n | awk '
        function pct(p,   i) {
            i = int((p / 100) * (n - 1) + 0.5) + 1
            if (i < 1) i = 1
            if (i > n) i = n
            return v[i]
        }
        { v[++n] = $1 }
        END {
            if (n == 0) { print "- - - 0"; exit }
            printf "%s %s %s %d\n", pct(50), pct(25), pct(75), n
        }'
}

# ---------------------------------------------------------------------------
# Launching
# ---------------------------------------------------------------------------

# Fills VM_ARGV with the launcher and its mode-selecting prefix. `hotspot` is
# the JDK's own java, which of course takes none of CratonVM's flags.
VM_ARGV=()
vm_prefix() {
    case "$1" in
        jdk-only) VM_ARGV=("$CV" --jdk-only --java-home "$JDK") ;;
        real-jdk) VM_ARGV=("$CV" --real-jdk --java-home "$JDK") ;;
        hotspot)  VM_ARGV=("$JAVA") ;;
        *)        die "unknown mode: $1" ;;
    esac
}

RUN_EXIT=0
RUN_MS=0
# run_timed <stdout-file> <stderr-file> <argv...>
# Sets RUN_EXIT (124 when `timeout` fires) and RUN_MS.
run_timed() {
    local outf="$1" errf="$2" t0 t1
    shift 2
    t0="$(now_us)"
    if [ -n "$TIMEOUT_BIN" ]; then
        "$TIMEOUT_BIN" "$TIMEOUT_SEC" "$@" > "$outf" 2> "$errf"
    else
        "$@" > "$outf" 2> "$errf"
    fi
    RUN_EXIT=$?
    t1="$(now_us)"
    RUN_MS="$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.1f", (b - a) / 1000.0 }')"
}

# ---------------------------------------------------------------------------
# Dump parsing
# ---------------------------------------------------------------------------
#
# No jq: the dumps are hand-rolled with one key per line, CI already re-derives
# these numbers with grep, and no dependency is worth adding for two integers.
#
# Emits "<block>\t<key>\t<value>" for every integer member of the top-level
# `counts` and `invocations` objects. Block-scoped on purpose: the schema-2
# native census has a `counts` block (registrations), an `invocations` block
# (dispatches) AND a `natives` array whose every row carries its own
# `invocations` key. A flat grep would conflate all three.
dump_counts() { # <json file>
    awk '
        /^[[:space:]]*"counts"[[:space:]]*:[[:space:]]*\{/      { sec = "counts"; next }
        /^[[:space:]]*"invocations"[[:space:]]*:[[:space:]]*\{/ { sec = "invocations"; next }
        /^[[:space:]]*"(natives|classes|violations)"[[:space:]]*:/ { sec = ""; next }
        /^[[:space:]]*[}\]]/ { sec = ""; next }
        sec != "" && match($0, /"[^"]+"[[:space:]]*:[[:space:]]*[0-9]+/) {
            s = substr($0, RSTART, RLENGTH)
            split(s, kv, "\"")
            val = s
            sub(/^"[^"]*"[[:space:]]*:[[:space:]]*/, "", val)
            printf "%s\t%s\t%s\n", sec, kv[2], val
        }' "$1" 2>/dev/null
}

dump_lookup() { # <parsed dump> <block> <key> -> value, or empty
    awk -F'\t' -v b="$2" -v k="$3" '$1 == b && $2 == k { print $3; exit }' "$1" 2>/dev/null
}

is_int() { case "${1:-}" in '' | *[!0-9-]* | -) return 1 ;; *) return 0 ;; esac; }

# ---------------------------------------------------------------------------
# CratonBench compilation (shared by several groups)
# ---------------------------------------------------------------------------

BENCH_CLASSES=""
BENCH_COMPILE_WHY=""
BENCH_COMPILED=0
ensure_cratonbench() {
    if [ "$BENCH_COMPILED" = 1 ]; then
        [ -n "$BENCH_CLASSES" ] && return 0
        return 1
    fi
    BENCH_COMPILED=1
    if [ ! -f "$CRATONBENCH_SRC" ]; then
        BENCH_COMPILE_WHY="bench/CratonBench.java not found"
        return 1
    fi
    if [ ! -x "$JAVAC" ]; then
        BENCH_COMPILE_WHY="javac not found under the JDK's bin directory"
        return 1
    fi
    local d="$OUT/classes/bench" rel=()
    mkdir -p "$d"
    [ -n "$JAVAC_RELEASE" ] && rel=(--release "$JAVAC_RELEASE")
    if "$JAVAC" ${rel[@]+"${rel[@]}"} -d "$d" "$CRATONBENCH_SRC" > "$RAW/javac-bench.out" 2>&1; then
        BENCH_CLASSES="$d"
        return 0
    fi
    BENCH_COMPILE_WHY="javac failed (see raw/javac-bench.out): $(head -1 "$RAW/javac-bench.out" 2>/dev/null)"
    return 1
}

# ---------------------------------------------------------------------------
# Metric 1 — startup wall time
# ---------------------------------------------------------------------------

group_startup() {
    local mode i s ok bad codes samples
    for mode in jdk-only real-jdk hotspot; do
        has_mode "$mode" || continue
        if [ "$TIMER" = none ]; then
            emit_gap startup_wall "$mode" ms "no sub-second wall clock: bash EPOCHREALTIME absent and date +%N unsupported"
            emit_group startup "$mode" gap "no usable wall clock"
            continue
        fi

        samples="$RAW/startup-$mode.samples"
        : > "$samples"
        ok=0; bad=0; codes=""
        # Warmups are discarded, but a failing warmup still counts: a mode that
        # cannot boot must not be rescued by the sample window.
        for i in $(seq 1 $((WARMUPS + LAUNCHES))); do
            vm_prefix "$mode"
            run_timed "$RAW/startup-$mode.out" "$RAW/startup-$mode.err" \
                "${VM_ARGV[@]}" -cp "$HELLO_CP" "$HELLO_CLASS"
            if [ "$RUN_EXIT" -ne 0 ]; then
                bad=$((bad + 1))
                case " $codes " in *" $RUN_EXIT "*) ;; *) codes="$codes $RUN_EXIT" ;; esac
                continue
            fi
            ok=$((ok + 1))
            if [ "$i" -gt "$WARMUPS" ]; then printf '%s\n' "$RUN_MS" >> "$samples"; fi
        done

        emit_ok startup_launches_ok     "$mode" count "$ok"  - - "$((WARMUPS + LAUNCHES))" "exit-0 launches, warmups included"
        emit_ok startup_launches_failed "$mode" count "$bad" - - "$((WARMUPS + LAUNCHES))" "non-zero exits; codes:${codes:- none}"

        if [ "$bad" -gt 0 ]; then
            # Deliberately no median over the survivors. A partially-failing
            # mode has no startup time, and reporting one would be exactly the
            # fabrication this harness exists to avoid.
            emit_fail startup_wall "$mode" ms "$bad of $((WARMUPS + LAUNCHES)) launches exited non-zero (codes:${codes}); no median is reported over the survivors"
            emit_group startup "$mode" fail "launch failures"
            continue
        fi
        s="$(stats < "$samples")"
        emit_ok startup_wall "$mode" ms $s "median of $LAUNCHES timed launches after $WARMUPS warmups; HelloWorld; timer=$TIMER"
        emit_group startup "$mode" ok "$LAUNCHES timed launches"
    done
}

# ---------------------------------------------------------------------------
# Metric 2 — peak memory
# ---------------------------------------------------------------------------

MEM_KIB=""
MEM_EXIT=""
MEM_SOURCE=""
# run_mem <tag> <argv...>
run_mem() {
    local tag="$1" tf af rf exe a w_exe w_af w_rf w_ps bytes
    shift
    MEM_KIB=""; MEM_EXIT=""
    case "$MEM_BACKEND" in
        gnu-time)
            MEM_SOURCE="max RSS (/usr/bin/time %M)"
            tf="$RAW/mem-$tag.time"
            "$GNU_TIME" -f '%M' -o "$tf" "$@" > "$RAW/mem-$tag.out" 2> "$RAW/mem-$tag.err"
            MEM_EXIT=$?
            # GNU time appends a diagnostic line of its own when the child is
            # signalled; take the last all-digit line and nothing else.
            MEM_KIB="$(awk '/^[0-9]+$/ { v = $0 } END { if (v != "") print v }' "$tf" 2>/dev/null)"
            ;;
        powershell)
            MEM_SOURCE="peak working set (System.Diagnostics.Process)"
            exe="$1"; shift
            af="$RAW/mem-$tag.args"; rf="$RAW/mem-$tag.result"
            : > "$af"
            for a in "$@"; do printf '%s\n' "$a" >> "$af"; done
            w_exe="$exe"; w_af="$af"; w_rf="$rf"; w_ps="$BENCH_DIR/peak-memory.ps1"
            if command -v cygpath >/dev/null 2>&1; then
                w_exe="$(cygpath -w "$exe")"; w_af="$(cygpath -w "$af")"
                w_rf="$(cygpath -w "$rf")";   w_ps="$(cygpath -w "$w_ps")"
            fi
            "$POWERSHELL" -NoProfile -ExecutionPolicy Bypass -File "$w_ps" \
                -Exe "$w_exe" -ArgFile "$w_af" -ResultFile "$w_rf" -TimeoutSec "$TIMEOUT_SEC" \
                > "$RAW/mem-$tag.out" 2> "$RAW/mem-$tag.err"
            MEM_EXIT="$(awk -F= '$1 == "exit" { print $2; exit }' "$rf" 2>/dev/null)"
            bytes="$(awk -F= '$1 == "peak_working_set_bytes" { print $2; exit }' "$rf" 2>/dev/null)"
            case "${bytes:-}" in
                '' | *[!0-9]*) MEM_KIB="" ;;
                *)             MEM_KIB=$((bytes / 1024)) ;;
            esac
            ;;
        *) MEM_SOURCE=""; MEM_EXIT="" ;;
    esac
}

# measure_mem <metric> <mode> <argv...>
measure_mem() {
    local metric="$1" mode="$2" samples ok bad codes i s tag
    shift 2
    tag="$(printf '%s' "$metric" | tr '.' '_')"
    samples="$RAW/$tag-$mode.samples"
    : > "$samples"
    ok=0; bad=0; codes=""
    for i in $(seq 1 "$MEM_LAUNCHES"); do
        run_mem "$tag-$mode-$i" "$@"
        if [ "${MEM_EXIT:-x}" != "0" ]; then
            bad=$((bad + 1))
            case " $codes " in *" ${MEM_EXIT:-?} "*) ;; *) codes="$codes ${MEM_EXIT:-?}" ;; esac
            continue
        fi
        if [ -z "$MEM_KIB" ]; then
            bad=$((bad + 1))
            case " $codes " in *" no-metric "*) ;; *) codes="$codes no-metric" ;; esac
            continue
        fi
        ok=$((ok + 1))
        printf '%s\n' "$MEM_KIB" >> "$samples"
    done
    if [ "$ok" -eq 0 ]; then
        emit_fail "$metric" "$mode" kib "all $MEM_LAUNCHES launches failed or produced no metric (codes:${codes})"
        return 1
    fi
    if [ "$bad" -gt 0 ]; then
        emit_fail "$metric" "$mode" kib "$bad of $MEM_LAUNCHES launches failed (codes:${codes}); no median over the survivors"
        return 1
    fi
    s="$(stats < "$samples")"
    emit_ok "$metric" "$mode" kib $s "$MEM_SOURCE; median of $MEM_LAUNCHES launches"
    return 0
}

group_memory() {
    local mode
    if [ "$MEM_BACKEND" = none ]; then
        for mode in jdk-only real-jdk hotspot; do
            has_mode "$mode" || continue
            emit_gap peak_memory.startup "$mode" kib "$MEM_BACKEND_WHY"
            emit_gap peak_memory.corpus  "$mode" kib "$MEM_BACKEND_WHY"
            emit_group memory "$mode" gap "$MEM_BACKEND_WHY"
        done
        return
    fi
    ensure_cratonbench || true
    for mode in jdk-only real-jdk hotspot; do
        has_mode "$mode" || continue
        vm_prefix "$mode"
        measure_mem peak_memory.startup "$mode" "${VM_ARGV[@]}" -cp "$HELLO_CP" "$HELLO_CLASS"
        if [ -n "$BENCH_CLASSES" ]; then
            vm_prefix "$mode"
            measure_mem peak_memory.corpus "$mode" "${VM_ARGV[@]}" -cp "$BENCH_CLASSES" CratonBench hashmap
        else
            emit_gap peak_memory.corpus "$mode" kib "CratonBench did not compile: $BENCH_COMPILE_WHY"
        fi
        emit_group memory "$mode" ok "backend=$MEM_BACKEND"
    done
}

# ---------------------------------------------------------------------------
# Metrics 3, 4, 5 — class origins, registry size, native dispatches per kind
# ---------------------------------------------------------------------------

CENSUS_PARSED=""
CENSUS_ORIGINS=""
CENSUS_EXIT=""
# census_run <tag> <mode> <classpath> <main-class> [args...]
census_run() {
    local tag="$1" mode="$2" cp="$3" cls="$4" reg org
    shift 4
    CENSUS_PARSED=""; CENSUS_ORIGINS=""; CENSUS_EXIT=""
    vm_prefix "$mode"
    reg="$RAW/census-$tag-$mode.registry.json"
    org="$RAW/census-$tag-$mode.origins.json"
    rm -f "$reg" "$org"
    run_timed "$RAW/census-$tag-$mode.out" "$RAW/census-$tag-$mode.err" \
        "${VM_ARGV[@]}" --dump-native-registry "$reg" --dump-class-origins "$org" \
        -cp "$cp" "$cls" "$@"
    CENSUS_EXIT="$RUN_EXIT"
    # The dumps are written from every exit path that has a live VM, including
    # the failing ones — that is the whole point of the strict-mode census, so
    # a non-zero exit here does not invalidate the files.
    if [ -s "$reg" ]; then
        CENSUS_PARSED="$RAW/census-$tag-$mode.registry.kv"
        dump_counts "$reg" > "$CENSUS_PARSED"
        [ -s "$CENSUS_PARSED" ] || CENSUS_PARSED=""
    fi
    if [ -s "$org" ]; then
        CENSUS_ORIGINS="$RAW/census-$tag-$mode.origins.kv"
        dump_counts "$org" > "$CENSUS_ORIGINS"
        [ -s "$CENSUS_ORIGINS" ] || CENSUS_ORIGINS=""
    fi
}

CLASS_ORIGIN_TAGS="boot-image application-class-path user-defined vm-array hidden-class generated-lambda generated-proxy reflection-accessor vm-internal compatibility-stub"
GENERATED_ORIGIN_TAGS="hidden-class generated-lambda generated-proxy reflection-accessor"
NATIVE_KINDS="intrinsic bridge synthetic-stub"

group_census() {
    local mode tag n total kind v
    if has_mode hotspot; then
        # Not a silent omission: HotSpot has no native registry and no
        # class-origin census, so there is nothing to compare these against.
        emit_group census hotspot gap "metrics 4 and 5 read CratonVM's own registry census; HotSpot has no equivalent dump"
    fi

    for mode in jdk-only real-jdk; do
        has_mode "$mode" || continue
        census_run hello "$mode" "$HELLO_CP" "$HELLO_CLASS"

        if [ -z "$CENSUS_ORIGINS" ]; then
            emit_fail class_origins.total "$mode" count "no --dump-class-origins output (VM exit=$CENSUS_EXIT)"
        else
            for tag in $CLASS_ORIGIN_TAGS; do
                n="$(dump_lookup "$CENSUS_ORIGINS" counts "$tag")"
                if [ -z "$n" ]; then
                    emit_gap "class_origins.$tag" "$mode" count "origin tag absent from the dump's counts block (schema drift?)"
                else
                    emit_ok "class_origins.$tag" "$mode" count "$n" - - 1 "--dump-class-origins counts block"
                fi
            done
            total=0
            for tag in $GENERATED_ORIGIN_TAGS; do
                n="$(dump_lookup "$CENSUS_ORIGINS" counts "$tag")"
                if is_int "${n:-}"; then total=$((total + n)); else total=-1; break; fi
            done
            if [ "$total" -ge 0 ]; then
                emit_ok class_origins.generated "$mode" count "$total" - - 1 "derived from $GENERATED_ORIGIN_TAGS; vm-array is excluded because an array class is not generated code"
            else
                emit_gap class_origins.generated "$mode" count "one or more generated-origin tags missing from the dump"
            fi
            n="$(dump_lookup "$CENSUS_ORIGINS" counts total)"
            if [ -n "$n" ]; then
                emit_ok class_origins.total   "$mode" count "$n" - - 1 "--dump-class-origins counts.total"
                emit_ok classes_loaded_total  "$mode" count "$n" - - 1 "class-manager census; includes VM array and generated classes"
            else
                emit_fail class_origins.total "$mode" count "counts.total absent from the dump"
            fi
        fi

        if [ -z "$CENSUS_PARSED" ]; then
            emit_fail registry.total "$mode" count "no --dump-native-registry output (VM exit=$CENSUS_EXIT)"
            emit_fail native_invocations.synthetic-stub "$mode" count "no --dump-native-registry output (VM exit=$CENSUS_EXIT)"
            emit_group census "$mode" fail "registry dump missing"
            continue
        fi
        for kind in $NATIVE_KINDS; do
            v="$(dump_lookup "$CENSUS_PARSED" counts "$kind")"
            if [ -n "$v" ]; then
                emit_ok "registry.$kind" "$mode" count "$v" - - 1 "schema-2 native census, counts block"
            else
                emit_gap "registry.$kind" "$mode" count "kind absent from the census counts block (schema drift?)"
            fi
            v="$(dump_lookup "$CENSUS_PARSED" invocations "$kind")"
            if [ -n "$v" ]; then
                emit_ok "native_invocations.$kind" "$mode" count "$v" - - 1 "schema-2 census invocations block (NativeMethodRegistry::invocations_of_kind)"
            else
                emit_gap "native_invocations.$kind" "$mode" count "kind absent from the census invocations block; the per-kind dispatch counters may not be wired"
            fi
        done
        v="$(dump_lookup "$CENSUS_PARSED" counts total)"
        if [ -n "$v" ]; then
            emit_ok registry.total "$mode" count "$v" - - 1 "schema-2 native census, counts.total"
        else
            emit_fail registry.total "$mode" count "counts.total absent from the census"
        fi
        emit_group census "$mode" ok "workload=HelloWorld vm-exit=$CENSUS_EXIT"
    done

    # Metric 3's HotSpot half. -Xlog:class+load counts the same event, but not
    # the same population (no VM array classes), so it carries its own detail
    # string and must not be diffed row-for-row against the CratonVM number.
    if has_mode hotspot; then
        run_timed "$RAW/census-hello-hotspot.out" "$RAW/census-hello-hotspot.err" \
            "$JAVA" -Xlog:class+load=info -cp "$HELLO_CP" "$HELLO_CLASS"
        if [ "$RUN_EXIT" -ne 0 ]; then
            emit_fail classes_loaded_total hotspot count "HotSpot HelloWorld run exited $RUN_EXIT"
        else
            n="$(cat "$RAW/census-hello-hotspot.out" "$RAW/census-hello-hotspot.err" 2>/dev/null | grep -ac 'class,load')"
            if [ "${n:-0}" -gt 0 ]; then
                emit_ok classes_loaded_total hotspot count "$n" - - 1 "-Xlog:class+load event count; a different population from CratonVM's class census"
            else
                emit_gap classes_loaded_total hotspot count "-Xlog:class+load produced no events on this JDK"
            fi
        fi
    fi

    # Metric 6 — bytecode instruction count. VERIFIED ABSENT, not skipped:
    # DiagnosticCounters::bytecodes_executed exists in
    # vm/src/runtime/diagnostics.rs, but nothing in the interpreter increments
    # it and nothing in the launcher prints it, so there is no number to read.
    # Emitting 0 would be the most misleading thing this harness could do —
    # metric 6 is the stated mechanism behind metrics 1 and 8.
    for mode in jdk-only real-jdk hotspot; do
        has_mode "$mode" || continue
        emit_gap bytecodes_executed "$mode" count "no interpreter bytecode counter is wired: DiagnosticCounters::bytecodes_executed is never incremented and the launcher never prints it. Closing this needs a VM-side change, outside this harness's ownership"
    done
}

# ---------------------------------------------------------------------------
# Metric 7 — JIT compilation count and time
# ---------------------------------------------------------------------------

group_jit() {
    local mode errf line compiles k v
    if has_mode hotspot; then
        emit_group jit hotspot gap "compile counts are not comparable across compilers; metric 7 exists to name newly-hot JDK methods for CratonVM's own JIT"
    fi
    ensure_cratonbench || true
    for mode in jdk-only real-jdk; do
        has_mode "$mode" || continue
        if [ -z "$BENCH_CLASSES" ]; then
            emit_group jit "$mode" gap "CratonBench did not compile: $BENCH_COMPILE_WHY"
            continue
        fi
        vm_prefix "$mode"
        errf="$RAW/jit-$mode.err"
        export CRATONVM_DBG_JIT_METHOD_STATS=1
        run_timed "$RAW/jit-$mode.out" "$errf" \
            "${VM_ARGV[@]}" -cp "$BENCH_CLASSES" CratonBench stringregex
        unset CRATONVM_DBG_JIT_METHOD_STATS

        line="$(grep -am1 'JIT method stats:' "$errf" 2>/dev/null)"
        if [ -z "$line" ]; then
            emit_fail jit.total_compile_time_ms "$mode" ms "no 'JIT method stats' line on stderr (vm exit=$RUN_EXIT): CRATONVM_DBG_JIT_METHOD_STATS may be unwired in this build, or no TieredCompilationManager was ever constructed"
            emit_group jit "$mode" fail "stats line absent"
            continue
        fi
        # `c1=` appears twice on that line (tier population, then compile
        # counts), so the compiles section is cut out before any key is read.
        compiles="$(printf '%s' "$line" | sed 's/.*| compiles: //; s/ |.*//')"
        for k in c1 c2 osr deopts c2_bailouts total_compile_time_ms; do
            v="$(printf ' %s ' "$compiles" | sed -n "s/.*[[:space:]]$k=\([0-9]\{1,\}\).*/\1/p")"
            if [ -z "$v" ]; then
                emit_gap "jit.$k" "$mode" count "key '$k' absent from the JIT stats line (format drift?)"
            elif [ "$k" = total_compile_time_ms ]; then
                emit_ok "jit.$k" "$mode" ms "$v" - - 1 "CRATONVM_DBG_JIT_METHOD_STATS; workload=CratonBench stringregex"
            else
                emit_ok "jit.$k" "$mode" count "$v" - - 1 "CRATONVM_DBG_JIT_METHOD_STATS; workload=CratonBench stringregex"
            fi
        done
        v="$(printf '%s' "$line" | sed -n 's/.*still-interpreted=\([0-9]\{1,\}\).*/\1/p')"
        if [ -n "$v" ]; then
            emit_ok jit.methods_still_interpreted "$mode" count "$v" - - 1 "methods that never left the interpreter"
        fi
        v="$(printf '%s' "$line" | sed -n 's/.*stats: [0-9]\{1,\} distinct methods tracked, \([0-9]\{1,\}\) ever invoked.*/\1/p')"
        if [ -n "$v" ]; then
            emit_ok jit.methods_ever_invoked "$mode" count "$v" - - 1 "distinct methods invoked at least once"
        fi
        emit_group jit "$mode" ok "vm-exit=$RUN_EXIT"
    done
}

# ---------------------------------------------------------------------------
# Metric 8 — core corpus throughput
# ---------------------------------------------------------------------------

HOTSPOT_SUM_FILE=""

# Expected checksum for a phase, from the CratonBench performance gate's own
# baseline. HotSpot is the primary oracle; this is the fallback that keeps the
# house rule enforceable on a host where HotSpot was not run.
baseline_checksum() { # <phase>
    [ -f "$CRATONBENCH_CHECKSUMS" ] || return 1
    awk -F'\t' -v p="$1" '$1 == p { print $3; exit }' "$CRATONBENCH_CHECKSUMS"
}

PHASE_MS=""
PHASE_SUM=""
# Parse one CratonBench phase line: "<n>. <name> (...)  : <ms> ms  [<checksum>]"
parse_phase_line() { # <stdout file>
    local line
    PHASE_MS=""; PHASE_SUM=""
    line="$(grep -aE '^[0-9]+\. ' "$1" 2>/dev/null | head -1)"
    [ -n "$line" ] || return 1
    PHASE_MS="$(printf '%s' "$line" | sed -n 's/.*: \([0-9]\{1,\}\) ms.*/\1/p')"
    PHASE_SUM="$(printf '%s' "$line" | sed -n 's/.*\[\(-\{0,1\}[0-9]\{1,\}\)\].*/\1/p')"
    [ -n "$PHASE_MS" ] && [ -n "$PHASE_SUM" ]
}

group_throughput() {
    local mode phase samples sum ok bad codes mismatched i s want oracle hs phases_ok
    if ! ensure_cratonbench; then
        for mode in jdk-only real-jdk hotspot; do
            has_mode "$mode" || continue
            emit_group throughput "$mode" gap "$BENCH_COMPILE_WHY"
        done
        return
    fi
    # HotSpot FIRST: it is the checksum oracle for the two CratonVM modes, and
    # an oracle collected after the fact would be no oracle at all.
    for mode in hotspot jdk-only real-jdk; do
        has_mode "$mode" || continue
        phases_ok=0
        for phase in ${PHASES//,/ }; do
            samples="$RAW/tp-$mode-$phase.samples"
            : > "$samples"
            sum=""; ok=0; bad=0; codes=""; mismatched=0
            for i in $(seq 1 "$REPS"); do
                vm_prefix "$mode"
                run_timed "$RAW/tp-$mode-$phase.out" "$RAW/tp-$mode-$phase.err" \
                    "${VM_ARGV[@]}" -Xmx8g -cp "$BENCH_CLASSES" CratonBench "$phase"
                if [ "$RUN_EXIT" -ne 0 ] || ! parse_phase_line "$RAW/tp-$mode-$phase.out"; then
                    bad=$((bad + 1))
                    case " $codes " in *" $RUN_EXIT "*) ;; *) codes="$codes $RUN_EXIT" ;; esac
                    continue
                fi
                if [ -n "$sum" ] && [ "$sum" != "$PHASE_SUM" ]; then mismatched=1; fi
                sum="$PHASE_SUM"
                ok=$((ok + 1))
                printf '%s\n' "$PHASE_MS" >> "$samples"
            done

            if [ "$ok" -eq 0 ]; then
                emit_fail "throughput.$phase" "$mode" ms "all $REPS runs failed or printed no phase line (exit codes:${codes})"
                continue
            fi
            if [ "$bad" -gt 0 ]; then
                emit_fail "throughput.$phase" "$mode" ms "$bad of $REPS runs failed (exit codes:${codes}); no median over the survivors"
                continue
            fi
            if [ "$mismatched" = 1 ]; then
                emit_fail "throughput.$phase" "$mode" ms "checksum was not stable across the $REPS runs; a nondeterministic result has no timing"
                continue
            fi

            # House rule (BENCHMARK.md, restated in the JDK-only benchmark doc):
            # a phase whose checksum does not match the oracle is a CORRECTNESS
            # failure and its timing is void. It is never recorded as slow.
            oracle=""
            want="$(baseline_checksum "$phase")"
            [ -n "$want" ] && oracle="CratonBench perf-gate baseline"
            if [ -n "$HOTSPOT_SUM_FILE" ] && [ -s "$HOTSPOT_SUM_FILE" ]; then
                hs="$(awk -F'\t' -v p="$phase" '$1 == p { print $2; exit }' "$HOTSPOT_SUM_FILE")"
                if [ -n "$hs" ]; then want="$hs"; oracle="HotSpot, this run"; fi
            fi
            if [ -z "$want" ]; then
                emit_gap "throughput.$phase" "$mode" ms "checksum $sum unverified: no HotSpot run this session and no baseline checksum for this phase. An unverified timing is not a result"
                continue
            fi
            if [ "$sum" != "$want" ]; then
                emit_fail "throughput.$phase" "$mode" ms "CORRECTNESS: checksum $sum != $want (oracle: $oracle); timing void"
                continue
            fi
            s="$(stats < "$samples")"
            emit_ok "throughput.$phase" "$mode" ms $s "median of $REPS isolated runs; checksum $sum verified against $oracle"
            phases_ok=$((phases_ok + 1))
            if [ "$mode" = hotspot ]; then
                printf '%s\t%s\n' "$phase" "$sum" >> "$HOTSPOT_SUM_FILE"
            fi
        done
        emit_group throughput "$mode" ok "$phases_ok phase(s) with a verified checksum"
    done
}

# ---------------------------------------------------------------------------
# Metric 8b — the RJdk* corpus, end to end
# ---------------------------------------------------------------------------
#
# These numbers INCLUDE VM startup and are therefore not steady-state
# throughput. Read them next to metric 1, never instead of metric 8.
# Correctness uses the regression suite's own oracle rule: the deterministic
# PASS/CK lines must match HotSpot's.

extract_det_lines() { sed 's/\x1b\[[0-9;]*m//g' | grep -aE '^(PASS|CK) '; }

group_vectors() {
    local dst="$OUT/classes/vectors" srcs=() v mode rel=() samples ok bad codes i s ran
    for v in ${VECTORS//,/ }; do
        if [ -f "$VECTOR_SRC_DIR/$v.java" ]; then srcs+=("$VECTOR_SRC_DIR/$v.java"); fi
    done
    if [ "${#srcs[@]}" -eq 0 ]; then
        for mode in jdk-only real-jdk hotspot; do
            has_mode "$mode" || continue
            emit_group vectors "$mode" gap "none of the requested vectors exist under regression-suite/src"
        done
        return
    fi
    mkdir -p "$dst"
    [ -n "$JAVAC_RELEASE" ] && rel=(--release "$JAVAC_RELEASE")
    if ! "$JAVAC" ${rel[@]+"${rel[@]}"} -d "$dst" "${srcs[@]}" > "$RAW/javac-vectors.out" 2>&1; then
        for mode in jdk-only real-jdk hotspot; do
            has_mode "$mode" || continue
            emit_group vectors "$mode" gap "javac failed on the vector corpus (see raw/javac-vectors.out): $(head -1 "$RAW/javac-vectors.out")"
        done
        return
    fi

    # HotSpot first: it is the oracle for the other two modes.
    for mode in hotspot jdk-only real-jdk; do
        has_mode "$mode" || continue
        ran=0
        for v in ${VECTORS//,/ }; do
            [ -f "$VECTOR_SRC_DIR/$v.java" ] || continue
            samples="$RAW/vec-$mode-$v.samples"
            : > "$samples"
            ok=0; bad=0; codes=""
            for i in $(seq 1 "$REPS"); do
                vm_prefix "$mode"
                run_timed "$RAW/vec-$mode-$v.out" "$RAW/vec-$mode-$v.err" \
                    "${VM_ARGV[@]}" -cp "$dst" "$v"
                if [ "$RUN_EXIT" -ne 0 ]; then
                    bad=$((bad + 1))
                    case " $codes " in *" $RUN_EXIT "*) ;; *) codes="$codes $RUN_EXIT" ;; esac
                    continue
                fi
                ok=$((ok + 1))
                printf '%s\n' "$RUN_MS" >> "$samples"
            done
            extract_det_lines < "$RAW/vec-$mode-$v.out" > "$RAW/vec-$mode-$v.lines" 2>/dev/null

            if [ "$bad" -gt 0 ]; then
                # Under --jdk-only in wave 1 this is an EXPECTED outcome for
                # some vectors. Recorded as a failure, which is data, rather
                # than quietly dropped.
                emit_fail "corpus.$v" "$mode" ms "$bad of $REPS runs exited non-zero (codes:${codes})"
                continue
            fi
            if [ "$mode" != hotspot ] && has_mode hotspot; then
                if [ -s "$RAW/vec-hotspot-$v.lines" ]; then
                    if ! diff -q "$RAW/vec-hotspot-$v.lines" "$RAW/vec-$mode-$v.lines" >/dev/null 2>&1; then
                        emit_fail "corpus.$v" "$mode" ms "CORRECTNESS: deterministic PASS/CK lines differ from HotSpot's; timing void"
                        continue
                    fi
                else
                    emit_gap "corpus.$v" "$mode" ms "no HotSpot oracle lines for this vector; an unverified end-to-end timing is not a result"
                    continue
                fi
            fi
            s="$(stats < "$samples")"
            emit_ok "corpus.$v" "$mode" ms $s "end-to-end wall time INCLUDING VM startup, not steady state; median of $REPS runs"
            ran=$((ran + 1))
        done
        emit_group vectors "$mode" ok "$ran vector(s) timed"
    done
}

# ---------------------------------------------------------------------------
# Metric 9 — GC pause and root counts
# ---------------------------------------------------------------------------

group_gc() {
    # The doc names RConcurrent explicitly: under strict mode real worker
    # threads execute real bytecode and add real roots, so this vector is more
    # stressed, not less. It is excluded from the regression suite's default
    # class list for that reason, so the harness runs it on purpose.
    local vec="RConcurrent" dst="$OUT/classes/gc" mode rel=() errf vm_exit gen young mi ma k val n s
    if [ ! -f "$VECTOR_SRC_DIR/$vec.java" ]; then
        for mode in jdk-only real-jdk hotspot; do
            has_mode "$mode" || continue
            emit_group gc "$mode" gap "regression-suite/src/$vec.java not found"
        done
        return
    fi
    mkdir -p "$dst"
    [ -n "$JAVAC_RELEASE" ] && rel=(--release "$JAVAC_RELEASE")
    if ! "$JAVAC" ${rel[@]+"${rel[@]}"} -d "$dst" "$VECTOR_SRC_DIR/$vec.java" > "$RAW/javac-gc.out" 2>&1; then
        for mode in jdk-only real-jdk hotspot; do
            has_mode "$mode" || continue
            emit_group gc "$mode" gap "javac failed on $vec (see raw/javac-gc.out)"
        done
        return
    fi

    for mode in jdk-only real-jdk; do
        has_mode "$mode" || continue
        vm_prefix "$mode"
        errf="$RAW/gc-$mode.err"
        run_timed "$RAW/gc-$mode.out" "$errf" \
            "${VM_ARGV[@]}" --verbose:gc -cp "$dst" "$vec"
        vm_exit="$RUN_EXIT"

        gen="$(grep -am1 '^\[GC\] generational:' "$errf" 2>/dev/null)"
        if [ -n "$gen" ]; then
            mi="$(printf '%s' "$gen" | sed -n 's/.*minor=\([0-9]\{1,\}\).*/\1/p')"
            ma="$(printf '%s' "$gen" | sed -n 's/.*major=\([0-9]\{1,\}\).*/\1/p')"
            [ -n "$mi" ] && emit_ok gc.minor_collections "$mode" count "$mi" - - 1 "generational collector; --verbose:gc; workload=$vec"
            [ -n "$ma" ] && emit_ok gc.major_collections "$mode" count "$ma" - - 1 "generational collector; --verbose:gc; workload=$vec"
        fi
        young="$(grep -am1 '^\[GC-SUMMARY\] young' "$errf" 2>/dev/null)"
        if [ -n "$young" ]; then
            for k in count total_us p50_us p99_us max_us; do
                val="$(printf '%s' "$young" | sed -n "s/.*[[:space:]]$k=\([0-9]\{1,\}\).*/\1/p")"
                if [ -n "$val" ]; then
                    if [ "$k" = count ]; then
                        emit_ok "gc.young_count" "$mode" count "$val" - - 1 "G1 pause summary; --verbose:gc; workload=$vec"
                    else
                        emit_ok "gc.young_$k" "$mode" us "$val" - - 1 "G1 pause summary; --verbose:gc; workload=$vec"
                    fi
                fi
            done
        else
            emit_gap gc.young_p50_us "$mode" us "no [GC-SUMMARY] line: pause percentiles are kept by G1 only and the default collector is generational. Re-run this group with --XX:UseGc g1 to obtain it"
        fi
        if [ -z "$gen" ] && [ -z "$young" ]; then
            emit_fail gc.minor_collections "$mode" count "--verbose:gc produced no GC lines at all (vm exit=$vm_exit): either no collection ran or the summary is unwired in this build"
            emit_group gc "$mode" fail "no GC output"
        else
            emit_group gc "$mode" ok "workload=$vec vm-exit=$vm_exit"
        fi

        # Root counts. The only readout is the per-pause `[g1][PHASES] roots=`
        # trace: G1 only, behind a debug flag, emitted once per pause, and its
        # own overhead perturbs the pause it is meant to describe. Not a number
        # this harness can honestly produce.
        emit_gap gc.roots_scanned "$mode" count "the only root-count readout is the per-pause [g1][PHASES] debug trace (G1 only, debug-flag gated, and self-perturbing). No aggregate root counter exists"
    done

    if has_mode hotspot; then
        run_timed "$RAW/gc-hotspot.out" "$RAW/gc-hotspot.err" \
            "$JAVA" -Xlog:gc -cp "$dst" "$vec"
        if [ "$RUN_EXIT" -ne 0 ]; then
            emit_fail gc.young_p50_us hotspot us "HotSpot $vec run exited $RUN_EXIT"
            emit_group gc hotspot fail "vector failed under HotSpot"
        else
            cat "$RAW/gc-hotspot.err" "$RAW/gc-hotspot.out" 2>/dev/null \
                | grep -a 'Pause ' \
                | sed -n 's/.*[[:space:]]\([0-9]\{1,\}\)\.\([0-9]\{3\}\)ms.*/\1\2/p' \
                > "$RAW/gc-hotspot.pauses"
            n="$(wc -l < "$RAW/gc-hotspot.pauses" 2>/dev/null | tr -d ' ')"
            if [ "${n:-0}" -gt 0 ]; then
                s="$(stats < "$RAW/gc-hotspot.pauses")"
                emit_ok gc.young_p50_us hotspot us $s "-Xlog:gc pause durations, all pause kinds pooled; not the same population as CratonVM's young-only summary"
                emit_ok gc.collections  hotspot count "$n" - - 1 "-Xlog:gc pause line count"
            else
                emit_gap gc.young_p50_us hotspot us "-Xlog:gc emitted no parseable pause durations"
            fi
            emit_group gc hotspot ok "workload=$vec"
        fi
        emit_gap gc.roots_scanned hotspot count "HotSpot does not report a root count"
    fi
}

# ---------------------------------------------------------------------------
# Registry / class / thread growth
# ---------------------------------------------------------------------------

group_growth() {
    local mode trivial_reg trivial_cls heavy_reg heavy_cls
    if ! ensure_cratonbench; then
        for mode in jdk-only real-jdk; do
            has_mode "$mode" || continue
            emit_group growth "$mode" gap "$BENCH_COMPILE_WHY"
        done
        return
    fi
    for mode in jdk-only real-jdk; do
        has_mode "$mode" || continue
        # Trivial vs heavy workload, same binary, same mode. Native
        # registration is an init-time activity, so registry.total should be
        # identical between the two; a delta is what "unbounded registry
        # growth" looks like from outside the VM.
        census_run growth-trivial "$mode" "$HELLO_CP" "$HELLO_CLASS"
        trivial_reg=""; trivial_cls=""
        [ -n "$CENSUS_PARSED" ]  && trivial_reg="$(dump_lookup "$CENSUS_PARSED" counts total)"
        [ -n "$CENSUS_ORIGINS" ] && trivial_cls="$(dump_lookup "$CENSUS_ORIGINS" counts total)"

        census_run growth-heavy "$mode" "$BENCH_CLASSES" CratonBench stringregex
        heavy_reg=""; heavy_cls=""
        [ -n "$CENSUS_PARSED" ]  && heavy_reg="$(dump_lookup "$CENSUS_PARSED" counts total)"
        [ -n "$CENSUS_ORIGINS" ] && heavy_cls="$(dump_lookup "$CENSUS_ORIGINS" counts total)"

        if is_int "${trivial_reg:-}" && is_int "${heavy_reg:-}"; then
            emit_ok growth.registry_total.trivial "$mode" count "$trivial_reg" - - 1 "registry.total after HelloWorld"
            emit_ok growth.registry_total.heavy   "$mode" count "$heavy_reg"   - - 1 "registry.total after CratonBench stringregex"
            emit_ok growth.registry_delta         "$mode" count "$((heavy_reg - trivial_reg))" - - 1 "heavy minus trivial; registration is an init-time activity, so 0 is the expectation"
        else
            emit_fail growth.registry_delta "$mode" count "one of the two registry censuses was not written (trivial='${trivial_reg:-none}' heavy='${heavy_reg:-none}')"
        fi
        if is_int "${trivial_cls:-}" && is_int "${heavy_cls:-}"; then
            emit_ok growth.class_total.trivial "$mode" count "$trivial_cls" - - 1 "class census after HelloWorld"
            emit_ok growth.class_total.heavy   "$mode" count "$heavy_cls"   - - 1 "class census after CratonBench stringregex"
            emit_ok growth.class_delta         "$mode" count "$((heavy_cls - trivial_cls))" - - 1 "INFORMATIONAL ONLY: a bigger workload legitimately loads more classes, and this delta cannot distinguish that from unbounded growth"
        fi
        emit_gap growth.threads_at_exit "$mode" count "no live-thread count at exit: threading::thread_registry offers a per-thread debug dump, not a counter the launcher prints"
        emit_group growth "$mode" ok "trivial vs heavy census pair"
    done
}

# ---------------------------------------------------------------------------
# Metric 10 — binary size
# ---------------------------------------------------------------------------

group_size() {
    local bytes profile=release note=""
    bytes="$(wc -c < "$CV" 2>/dev/null | tr -d ' ')"
    case "$CV" in *[/\\]debug[/\\]*) profile=debug; note=" (NOT comparable to a release baseline)" ;; esac
    if [ -n "$bytes" ]; then
        emit_ok binary_size build bytes "$bytes" - - 1 "cratonvm launcher, $profile profile$note"
    else
        emit_fail binary_size build bytes "could not stat the cratonvm binary"
    fi
    # The doc asks for the default build, the --features synthetic-jdk build
    # and any hardened build. Producing the other two means building them, and
    # this harness deliberately builds nothing.
    emit_gap binary_size.synthetic_jdk build bytes "requires a second cargo build with --features synthetic-jdk; this harness never invokes cargo. Measure it in the job that produces the artifact"
    emit_group size build ok "profile=$profile"
}

# ---------------------------------------------------------------------------
# Metadata — alongside the results, never inside them
# ---------------------------------------------------------------------------

write_metadata() {
    local f="$OUT/run-metadata.txt"
    {
        echo "# JDK-only benchmark run metadata."
        echo "# Kept OUT of results.tsv on purpose: it changes on every run and on"
        echo "# every host, and would make the result set undiffable."
        echo "#"
        echo "# A number without this block is not comparable to anything."
        echo
        echo "captured_at_utc: $(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)"
        echo "host_label: ${HOST_LABEL:-<unset: pass --label>}"
        echo "hostname: $(hostname 2>/dev/null || echo unknown)"
        echo "platform: $PLATFORM"
        echo "uname: $(uname -a 2>/dev/null || echo unknown)"
        if [ -r /proc/cpuinfo ]; then
            echo "cpu_model: $(awk -F': ' '/model name/ { print $2; exit }' /proc/cpuinfo)"
            echo "cpu_threads: $(grep -c '^processor' /proc/cpuinfo)"
        else
            echo "cpu_model: ${PROCESSOR_IDENTIFIER:-unknown}"
            echo "cpu_threads: ${NUMBER_OF_PROCESSORS:-unknown}"
        fi
        [ -r /proc/meminfo ] && echo "mem_total_kib: $(awk '/MemTotal/ { print $2; exit }' /proc/meminfo)"
        if [ -r /proc/loadavg ]; then
            echo "loadavg_1min: $(cut -d' ' -f1 /proc/loadavg)"
            echo "# A run captured on a loaded host is not a baseline."
        fi
        echo "vm_commit: $(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
        echo "vm_branch: $(git -C "$ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
        if [ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null)" ]; then
            echo "vm_worktree_dirty: yes   # results are not attributable to vm_commit alone"
        else
            echo "vm_worktree_dirty: no"
        fi
        echo "cratonvm_binary: ${CV#"$ROOT"/}"
        case "$CV" in *[/\\]debug[/\\]*) echo "cratonvm_profile: debug   # NOT a performance baseline" ;;
                      *)                 echo "cratonvm_profile: release" ;; esac
        echo "java_home: $JDK"
        echo "java_version: $("$JAVA" -version 2>&1 | head -1)"
        echo "java_feature: $("$JAVA" -version 2>&1 | awk -F'\"' '/version/ { split($2, a, "."); print a[1]; exit }')"
        # No apostrophe inside a ${var:-word} default: bash parses the word for
        # quotes even inside double quotes, and a lone ' swallows the rest of
        # the file.
        echo "javac_release_flag: ${JAVAC_RELEASE:-<none, javac default level>}"
        echo "# CI pins JDK 25 on every job today; JDK 21 has no coverage anywhere"
        echo "# in the workflow. Any 17/21/25 matrix is a proposal, not coverage."
        echo
        echo "harness: scripts/jdk-only-bench.sh"
        echo "groups: $GROUP_LIST"
        echo "modes: $MODES"
        echo "launches: $LAUNCHES (plus $WARMUPS discarded warmups)"
        echo "mem_launches: $MEM_LAUNCHES"
        echo "reps: $REPS"
        echo "phases: $PHASES"
        echo "vectors: $VECTORS"
        echo "timeout_sec: $TIMEOUT_SEC"
        echo "wall_clock_source: $TIMER"
        echo "peak_memory_backend: $MEM_BACKEND${MEM_BACKEND_WHY:+ ($MEM_BACKEND_WHY)}"
        echo "timeout_wrapper: ${TIMEOUT_BIN:-<none: launches are unbounded>}"
    } > "$f"
}

# ---------------------------------------------------------------------------
# Finalisation
# ---------------------------------------------------------------------------

finalise() {
    {
        echo "# JDK-only mode benchmark results. See bench/jdk-only/README.md."
        echo "# Diffable by construction: sorted, no timestamps, no absolute paths."
        echo "# Run metadata (host, CPU, JDK, commit) lives in run-metadata.txt."
        echo "#"
        echo "# status: ok   = measured"
        echo "#         gap  = not obtainable here; 'detail' says why. NOT zero."
        echo "#         fail = the input existed and did not work. NOT zero."
        echo "# value/low/high = median, p25, p75 (nearest-rank). '-' unless ok."
        printf '# metric%smode%sunit%sstatus%svalue%slow%shigh%sn%sdetail\n' \
            "$TAB" "$TAB" "$TAB" "$TAB" "$TAB" "$TAB" "$TAB" "$TAB"
        sort -t "$TAB" -k1,1 -k2,2 "$RESULTS_RAW"
    } > "$RESULTS"

    # Self-check 1: no absolute path survived redaction into the diffable file.
    local leak
    for leak in "$ROOT" "$ROOT_ALT" "$JDK" "$JDK_ALT"; do
        [ -n "$leak" ] || continue
        if grep -qF -- "$leak" "$RESULTS" 2>/dev/null; then
            echo "INTERNAL ERROR: absolute path '$leak' reached $RESULTS" >&2
            exit 4
        fi
    done
    # Self-check 2: no non-ok row carries a value (the fabrication guard).
    if awk -F'\t' '$1 !~ /^#/ && $4 != "ok" && $5 != "-" { bad = 1 } END { exit !bad }' "$RESULTS"; then
        echo "INTERNAL ERROR: a non-ok row in $RESULTS carries a value" >&2
        exit 4
    fi
    # Self-check 3: every ok row carries one. `unit=state` rows are exempt:
    # the per-group status rows record that a group ran, and have no number by
    # construction.
    if awk -F'\t' '$1 !~ /^#/ && $3 != "state" && $4 == "ok" && ($5 == "-" || $5 == "") { bad = 1 } END { exit !bad }' "$RESULTS"; then
        echo "INTERNAL ERROR: an ok row in $RESULTS carries no value" >&2
        exit 4
    fi
    rm -f "$RESULTS_RAW"
}

# ---------------------------------------------------------------------------
# Gate
# ---------------------------------------------------------------------------

lookup() { # <results> <metric> <mode> -> value, empty unless status=ok
    awk -F'\t' -v m="$2" -v md="$3" '$1 == m && $2 == md && $4 == "ok" { print $5; exit }' "$1"
}
lookup_status() {
    awk -F'\t' -v m="$2" -v md="$3" '$1 == m && $2 == md { print $4; exit }' "$1"
}

GATE_FAIL=0
GATE_NOT_EVAL=0
GATE_OUT=""

gate_line() { printf '  %-9s %-46s %s\n' "$1" "$2" "$3" >> "$GATE_OUT"; }

# gate_ratio <label> <class-tag> <metric> <limit-pct> <units>
gate_ratio() {
    local label="$1" tag="$2" metric="$3" limit="$4" unit="$5" strict compat pct verdict
    strict="$(lookup "$RESULTS" "$metric" jdk-only)"
    compat="$(lookup "$RESULTS" "$metric" real-jdk)"
    if [ -z "$strict" ] || [ -z "$compat" ]; then
        local ss cs
        ss="$(lookup_status "$RESULTS" "$metric" jdk-only)"
        cs="$(lookup_status "$RESULTS" "$metric" real-jdk)"
        gate_line NOT-EVAL "$label [$tag]" "need '$metric' measured in both modes; jdk-only=${ss:-absent}, real-jdk=${cs:-absent}"
        GATE_NOT_EVAL=$((GATE_NOT_EVAL + 1))
        return
    fi
    pct="$(awk -v a="$strict" -v b="$compat" 'BEGIN { if (b + 0 == 0) print "nan"; else printf "%.1f", (a - b) * 100.0 / b }')"
    if [ "$pct" = nan ]; then
        gate_line NOT-EVAL "$label [$tag]" "the real-jdk value is 0; a ratio is undefined"
        GATE_NOT_EVAL=$((GATE_NOT_EVAL + 1))
        return
    fi
    verdict="$(awk -v p="$pct" -v l="$limit" 'BEGIN { if (p > l) print "BREACH"; else print "ok" }')"
    gate_line "$verdict" "$label [$tag]" "jdk-only ${strict}${unit} vs real-jdk ${compat}${unit} = ${pct}% (limit +${limit}%)"
    [ "$verdict" = BREACH ] && GATE_FAIL=$((GATE_FAIL + 1))
    return 0
}

gate_row() { # id kind metric limit class
    local id="$1" kind="$2" metric="$3" limit="$4" class="$5" tag v st d m
    tag="$(printf '%s' "$class" | tr '[:lower:]' '[:upper:]')"
    case "$kind" in
        ratio-max-increase)
            gate_ratio "$id" "$tag" "$metric" "$limit" ""
            ;;
        ratio-max-decrease)
            # A throughput DECREASE shows up as an INCREASE in phase wall time,
            # so the arithmetic is the same and only the wording differs.
            case "$metric" in
                *'*')
                    m="${metric%\*}"
                    st=0
                    for v in $(awk -F'\t' -v p="$m" 'index($1, p) == 1 { print $1 }' "$RESULTS" | sort -u); do
                        gate_ratio "$v" "$tag" "$v" "$limit" "ms"
                        st=$((st + 1))
                    done
                    # A family that matched nothing must still produce a row.
                    # Silently emitting no line at all is how a budget stops
                    # being checked without anyone noticing.
                    if [ "$st" -eq 0 ]; then
                        gate_line NOT-EVAL "$id [$tag]" "no metric matches '$metric'; the throughput group was not run, or every phase failed its checksum"
                        GATE_NOT_EVAL=$((GATE_NOT_EVAL + 1))
                    fi
                    ;;
                *) gate_ratio "$id" "$tag" "$metric" "$limit" "ms" ;;
            esac
            ;;
        absolute-max)
            v="$(lookup "$RESULTS" "$metric" jdk-only)"
            st="$(lookup_status "$RESULTS" "$metric" jdk-only)"
            if ! is_int "${v:-}"; then
                # An invariant that could not be checked is not a satisfied one.
                gate_line BREACH "$id [$tag]" "NOT MEASURED (status=${st:-absent}); an unchecked invariant is treated as failed, never as passed"
                GATE_FAIL=$((GATE_FAIL + 1))
                return
            fi
            if [ "$v" -gt "$limit" ]; then
                gate_line BREACH "$id [$tag]" "jdk-only $metric = $v (limit $limit)"
                GATE_FAIL=$((GATE_FAIL + 1))
            else
                gate_line ok "$id [$tag]" "jdk-only $metric = $v (limit $limit)"
            fi
            ;;
        delta-max)
            d="$(lookup "$RESULTS" growth.registry_delta jdk-only)"
            if ! is_int "${d:-}"; then
                gate_line NOT-EVAL "$id [$tag]" "growth.registry_delta was not measured"
                GATE_NOT_EVAL=$((GATE_NOT_EVAL + 1))
                return
            fi
            if [ "$d" -gt "$limit" ]; then
                gate_line BREACH "$id [$tag]" "the registry grew by $d between the trivial and heavy workloads (limit $limit)"
                GATE_FAIL=$((GATE_FAIL + 1))
            else
                gate_line ok "$id [$tag]" "registry delta $d (limit $limit)"
            fi
            ;;
        not-measured)
            gate_line NOT-EVAL "$id [$tag]" "declared unmeasurable by this harness; budgets.tsv says what is missing. Its absence is not a pass"
            GATE_NOT_EVAL=$((GATE_NOT_EVAL + 1))
            ;;
        *)
            gate_line NOT-EVAL "$id" "budget kind '$kind' has no evaluator in this harness"
            GATE_NOT_EVAL=$((GATE_NOT_EVAL + 1))
            ;;
    esac
}

run_gate() {
    local id kind metric limit class note
    GATE_OUT="$OUT/gate.txt"
    {
        echo "JDK-only benchmark gate"
        echo "======================="
        echo
        echo "THE PERFORMANCE ROWS BELOW ARE *PROPOSED* ENGINEERING GATES, NOT"
        echo "ESTABLISHED THRESHOLDS. No baseline has been captured. They are to be"
        echo "recalibrated after the first clean baseline on a fixed host, per the"
        echo "recalibration procedure in docs/benchmarks/jdk-only.md. Do not quote a"
        echo "PASS here as evidence that strict mode meets a performance target."
        echo
        echo "The synthetic-stub row is different in kind: it is the feature's"
        echo "defining invariant (design section 1.3) and is not negotiable."
        echo
    } > "$GATE_OUT"

    if [ ! -f "$BUDGETS" ]; then
        # A BREACH, not a NOT-EVAL. With no budget table there is no
        # synthetic-stub invariant row, and a gate that exits 0 because it had
        # nothing to check is indistinguishable from one that passed. Note that
        # .gitignore excludes bench/, so this file can silently fail to land.
        gate_line BREACH "(all)" "budgets file not found: ${BUDGETS#"$ROOT"/} — nothing was checked, including the synthetic-stub invariant"
        GATE_FAIL=$((GATE_FAIL + 1))
    else
        # No pipeline here on purpose: a `while read` on the right of a pipe
        # runs in a subshell and every GATE_FAIL increment would be discarded.
        while IFS="$TAB" read -r id kind metric limit class note; do
            case "${id:-}" in '#'* | '') continue ;; esac
            gate_row "$id" "$kind" "$metric" "$limit" "$class"
        done < "$BUDGETS"
    fi

    {
        echo
        echo "summary: $GATE_FAIL breach(es), $GATE_NOT_EVAL not evaluated"
    } >> "$GATE_OUT"
    cat "$GATE_OUT"
}

run_drift() {
    local out="$OUT/drift.txt"
    {
        echo "Baseline drift"
        echo "=============="
        echo "baseline:  ${BASELINE#"$ROOT"/}"
        echo "tolerance: +/-${DRIFT_TOLERANCE}%"
        echo
        awk -F'\t' -v tol="$DRIFT_TOLERANCE" '
            FILENAME == base && $1 !~ /^#/ {
                if ($4 == "ok") b[$1 "\t" $2] = $5
                bs[$1 "\t" $2] = $4
                next
            }
            $1 !~ /^#/ {
                key = $1 "\t" $2
                seen[key] = 1
                if (!(key in bs))  { printf "  NEW        %s [%s] (%s)\n", $1, $2, $4; next }
                # unit=state rows carry no number; a change of STATUS is the
                # whole signal, and running them through the ratio arithmetic
                # would print a spurious "baseline is 0".
                if ($3 == "state") {
                    if ($4 != bs[key]) printf "  STATE      %s [%s] %s -> %s\n", $1, $2, bs[key], $4
                    else               printf "  ok         %s [%s] %s\n", $1, $2, $4
                    next
                }
                if ($4 != "ok" || !(key in b)) { printf "  NOT-EVAL   %s [%s] now %s, baseline %s\n", $1, $2, $4, bs[key]; next }
                if (b[key] + 0 == 0) { printf "  NOT-EVAL   %s [%s] baseline is 0; a ratio is undefined\n", $1, $2; next }
                d = ($5 - b[key]) * 100.0 / b[key]
                if (d > tol || d < -tol) flag = "DRIFT     "; else flag = "ok        "
                printf "  %s %s [%s] %s -> %s (%+.1f%%)\n", flag, $1, $2, b[key], $5, d
            }
            END {
                for (key in bs) if (!(key in seen)) {
                    split(key, p, "\t")
                    printf "  MISSING    %s [%s] present in the baseline, absent now\n", p[1], p[2]
                }
            }
        ' base="$BASELINE" "$BASELINE" "$RESULTS" | sort
    } > "$out"
    cat "$out"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

HOTSPOT_SUM_FILE="$RAW/hotspot-checksums.tsv"
: > "$HOTSPOT_SUM_FILE"

echo "== jdk-only benchmark =="
echo "   cratonvm : ${CV#"$ROOT"/}"
echo "   jdk      : $JDK"
echo "   platform : $PLATFORM (timer=$TIMER, peak-memory=$MEM_BACKEND)"
echo "   groups   : $GROUP_LIST"
echo "   modes    : $MODES"
echo "   out      : ${OUT#"$ROOT"/}"
echo

for g in ${GROUP_LIST//,/ }; do
    case "$g" in
        startup)    echo "-- startup";    group_startup ;;
        memory)     echo "-- memory";     group_memory ;;
        census)     echo "-- census";     group_census ;;
        jit)        echo "-- jit";        group_jit ;;
        throughput) echo "-- throughput"; group_throughput ;;
        vectors)    echo "-- vectors";    group_vectors ;;
        gc)         echo "-- gc";         group_gc ;;
        growth)     echo "-- growth";     group_growth ;;
        size)       echo "-- size";       group_size ;;
        *)          die "unknown group: $g" ;;
    esac
done

write_metadata
finalise

echo
echo "== results =="
echo "   ${RESULTS#"$ROOT"/}"
echo "   $ROWS_OK measured, $ROWS_GAP gap(s), $ROWS_FAIL failure(s)"
echo "   metadata: ${OUT#"$ROOT"/}/run-metadata.txt"

RC=0
if [ "$DO_GATE" = 1 ]; then
    echo
    run_gate
    [ "$GATE_FAIL" -gt 0 ] && RC=1
fi

if [ -n "$BASELINE" ]; then
    if [ ! -f "$BASELINE" ]; then
        echo "ERROR: baseline not found: $BASELINE" >&2
        RC=1
    else
        echo
        run_drift
        if [ "$GATE_DRIFT" = 1 ] && grep -q '^  DRIFT' "$OUT/drift.txt"; then
            echo "drift beyond tolerance, and --gate-drift was passed" >&2
            RC=1
        fi
    fi
fi

exit "$RC"
