#!/usr/bin/env bash
# Reproducible architecture probes. Every result is a fresh process and the
# two VMs alternate within each repetition to reduce host-load bias.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EXE=""
JAVA="${JAVA_HOME:+$JAVA_HOME/bin/java}"
JAVA_HOME_ARG="${JAVA_HOME:-}"
CPU=13
REPS=3
OUT=""
TIMEOUT_SECONDS=240

usage() {
    echo "usage: $0 -Exe /absolute/cratonvm [--java /path/java] [--java-home DIR] [--cpu N] [--reps N] [--out FILE]" >&2
    exit 2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -Exe) EXE="$2"; shift 2 ;;
        --java) JAVA="$2"; shift 2 ;;
        --java-home) JAVA_HOME_ARG="$2"; shift 2 ;;
        --cpu) CPU="$2"; shift 2 ;;
        --reps) REPS="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --timeout) TIMEOUT_SECONDS="$2"; shift 2 ;;
        *) usage ;;
    esac
done

[ -x "$EXE" ] || usage
if [ -z "$JAVA" ]; then
    JAVA="$(command -v java)"
fi
[ -x "$JAVA" ] || usage
if [ -z "$JAVA_HOME_ARG" ]; then
    JAVA_HOME_ARG="$(cd "$(dirname "$JAVA")/.." && pwd)"
fi
command -v javac >/dev/null || { echo "javac is required" >&2; exit 2; }
command -v taskset >/dev/null || { echo "taskset is required" >&2; exit 2; }

CLASSES="$(mktemp -d)"
trap 'rm -rf "$CLASSES"' EXIT
find "$SCRIPT_DIR" -name '*.java' -print0 | xargs -0 javac -d "$CLASSES"

if [ -z "$OUT" ]; then
    OUT="$SCRIPT_DIR/results-$(date +%Y%m%d-%H%M%S).tsv"
fi

printf 'probe\tvm\trep\titerations\telapsed_ns\tchecksum\tload1\n' > "$OUT"

run_case() {
    local vm="$1" probe="$2" class="$3" iterations="$4" mode="$5" rep="$6"
    local line load1
    load1="$(cut -d' ' -f1 /proc/loadavg)"
    if [ "$vm" = cratonvm ]; then
        if [ "$mode" = interpreted ]; then
            line="$(timeout "$TIMEOUT_SECONDS" taskset -c "$CPU" "$EXE" --nojit --java-home "$JAVA_HOME_ARG" -Xmx512m -cp "$CLASSES" "$class" "$iterations" 2>/dev/null | tail -1)"
        else
            line="$(timeout "$TIMEOUT_SECONDS" taskset -c "$CPU" "$EXE" --java-home "$JAVA_HOME_ARG" -Xmx512m -cp "$CLASSES" "$class" "$probe" "$iterations" 2>/dev/null | tail -1)"
        fi
    else
        if [ "$mode" = interpreted ]; then
            line="$(timeout "$TIMEOUT_SECONDS" taskset -c "$CPU" "$JAVA" -Xint -Xmx512m -cp "$CLASSES" "$class" "$iterations" 2>/dev/null | tail -1)"
        else
            line="$(timeout "$TIMEOUT_SECONDS" taskset -c "$CPU" "$JAVA" -Xmx512m -cp "$CLASSES" "$class" "$probe" "$iterations" 2>/dev/null | tail -1)"
        fi
    fi
    IFS=$'\t' read -r actual got_iterations elapsed checksum <<< "$line"
    if [ -z "${elapsed:-}" ] || [ "$got_iterations" != "$iterations" ]; then
        echo "invalid output from $vm/$probe: $line" >&2
        exit 1
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$probe" "$vm" "$rep" "$iterations" "$elapsed" "$checksum" "$load1" | tee -a "$OUT"
}

run_pair() {
    local probe="$1" class="$2" iterations="$3" mode="$4"
    local rep
    for rep in $(seq 1 "$REPS"); do
        if [ $((rep % 2)) -eq 1 ]; then
            run_case hotspot "$probe" "$class" "$iterations" "$mode" "$rep"
            run_case cratonvm "$probe" "$class" "$iterations" "$mode" "$rep"
        else
            run_case cratonvm "$probe" "$class" "$iterations" "$mode" "$rep"
            run_case hotspot "$probe" "$class" "$iterations" "$mode" "$rep"
        fi
    done
}

INTERP_ITERS="${INTERP_ITERS:-2000000}"
DISPATCH_ITERS="${DISPATCH_ITERS:-10000000}"
ALLOC_ITERS="${ALLOC_ITERS:-500000}"
EXCEPTION_ITERS="${EXCEPTION_ITERS:-250000}"
MONITOR_ITERS="${MONITOR_ITERS:-2000000}"
NATIVE_ITERS="${NATIVE_ITERS:-500000}"

run_pair interp-fast ArchitectureInterpFast20260726 "$INTERP_ITERS" interpreted
run_pair interp-slow org.springframework.archprobe.ArchitectureInterpSlow20260726 "$INTERP_ITERS" interpreted
run_pair dispatch-mono ArchitectureRuntimeProbe20260726 "$DISPATCH_ITERS" compiled
run_pair dispatch-poly4 ArchitectureRuntimeProbe20260726 "$DISPATCH_ITERS" compiled
run_pair dispatch-mega16 ArchitectureRuntimeProbe20260726 "$DISPATCH_ITERS" compiled
run_pair allocation ArchitectureRuntimeProbe20260726 "$ALLOC_ITERS" compiled
run_pair exception ArchitectureRuntimeProbe20260726 "$EXCEPTION_ITERS" compiled
run_pair monitor ArchitectureRuntimeProbe20260726 "$MONITOR_ITERS" compiled
run_pair native ArchitectureRuntimeProbe20260726 "$NATIVE_ITERS" compiled

expected_rows=$((REPS * 2))
awk -F '\t' -v expected_rows="$expected_rows" '
    NR == 1 { next }
    {
        rows[$1]++
        if ($1 != "native") {
            checksum[$1 SUBSEP $6] = 1
        }
    }
    END {
        failed = 0
        for (probe in rows) {
            if (rows[probe] != expected_rows) {
                printf "invalid row count for %s: got %d, expected %d\n",
                    probe, rows[probe], expected_rows > "/dev/stderr"
                failed = 1
            }
        }
        for (key in checksum) {
            split(key, parts, SUBSEP)
            distinct[parts[1]]++
        }
        for (probe in distinct) {
            if (distinct[probe] != 1) {
                printf "checksum mismatch for %s: %d distinct values\n",
                    probe, distinct[probe] > "/dev/stderr"
                failed = 1
            }
        }
        exit failed
    }
' "$OUT"

echo "validated row counts and deterministic checksums"
echo "wrote $OUT"
