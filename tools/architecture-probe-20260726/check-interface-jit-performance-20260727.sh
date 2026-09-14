#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EXE=""
JAVA_HOME_ARG="${JAVA_HOME:-}"
ITERATIONS=1000000
REPS=3
CPU=13

usage() {
    echo "usage: $0 -Exe /absolute/cratonvm [--java-home DIR] [--iterations N] [--reps N] [--cpu N]" >&2
    exit 2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -Exe) EXE="$2"; shift 2 ;;
        --java-home) JAVA_HOME_ARG="$2"; shift 2 ;;
        --iterations) ITERATIONS="$2"; shift 2 ;;
        --reps) REPS="$2"; shift 2 ;;
        --cpu) CPU="$2"; shift 2 ;;
        *) usage ;;
    esac
done

[ -x "$EXE" ] || usage
if [ -z "$JAVA_HOME_ARG" ]; then
    JAVA_BIN="$(command -v java)"
    JAVA_HOME_ARG="$(cd "$(dirname "$(readlink -f "$JAVA_BIN")")/.." && pwd)"
fi
[ -x "$JAVA_HOME_ARG/bin/java" ] || usage
[ -x "$JAVA_HOME_ARG/bin/javac" ] || usage
command -v taskset >/dev/null || { echo "taskset is required" >&2; exit 2; }

CLASSES="$(mktemp -d)"
RESULTS="$(mktemp)"
DIAG="$(mktemp)"
trap 'rm -rf "$CLASSES"; rm -f "$RESULTS" "$DIAG"' EXIT
"$JAVA_HOME_ARG/bin/javac" -d "$CLASSES" "$SCRIPT_DIR/ArchitectureRuntimeProbe20260726.java"

run_case() {
    local mode="$1" vm="$2" rep="$3" line actual got_iterations elapsed checksum
    case "$vm" in
        jit)
            line="$(taskset -c "$CPU" "$EXE" --java-home "$JAVA_HOME_ARG" -cp "$CLASSES" \
                ArchitectureRuntimeProbe20260726 "$mode" "$ITERATIONS" 2>/dev/null | tail -1)"
            ;;
        nojit)
            line="$(taskset -c "$CPU" "$EXE" --nojit --java-home "$JAVA_HOME_ARG" -cp "$CLASSES" \
                ArchitectureRuntimeProbe20260726 "$mode" "$ITERATIONS" 2>/dev/null | tail -1)"
            ;;
        hotspot)
            line="$(taskset -c "$CPU" "$JAVA_HOME_ARG/bin/java" -cp "$CLASSES" \
                ArchitectureRuntimeProbe20260726 "$mode" "$ITERATIONS" 2>/dev/null | tail -1)"
            ;;
        *) return 2 ;;
    esac
    IFS=$'\t' read -r actual got_iterations elapsed checksum <<< "$line"
    if [ "$actual" != "$mode" ] || [ "$got_iterations" != "$ITERATIONS" ] ||
        ! [[ "$elapsed" =~ ^[0-9]+$ ]] || ! [[ "$checksum" =~ ^-?[0-9]+$ ]]; then
        echo "invalid output from $vm/$mode: $line" >&2
        exit 1
    fi
    printf '%s\t%s\t%s\t%s\t%s\n' "$mode" "$vm" "$rep" "$elapsed" "$checksum" |
        tee -a "$RESULTS"
}

for mode in dispatch-mono dispatch-poly4 dispatch-mega16; do
    for rep in $(seq 1 "$REPS"); do
        run_case "$mode" hotspot "$rep"
        run_case "$mode" jit "$rep"
        run_case "$mode" nojit "$rep"
    done
done

awk -F '\t' '
    { checksum[$1 SUBSEP $5] = 1 }
    END {
        for (key in checksum) {
            split(key, p, SUBSEP)
            distinct[p[1]]++
        }
        for (mode in distinct) {
            if (distinct[mode] != 1) {
                printf "checksum mismatch for %s\n", mode > "/dev/stderr"
                failed = 1
            }
        }
        exit failed
    }
' "$RESULTS"

median() {
    local mode="$1" vm="$2"
    awk -F '\t' -v mode="$mode" -v vm="$vm" '
        $1 == mode && $2 == vm { print $4 }
    ' "$RESULTS" | sort -n | awk '{ value[NR] = $1 } END { print value[int((NR + 1) / 2)] }'
}

for mode in dispatch-mono dispatch-poly4 dispatch-mega16; do
    jit_ns="$(median "$mode" jit)"
    nojit_ns="$(median "$mode" nojit)"
    hotspot_ns="$(median "$mode" hotspot)"
    printf 'median\t%s\tjit=%s\tnojit=%s\thotspot=%s\n' \
        "$mode" "$jit_ns" "$nojit_ns" "$hotspot_ns"
    if [ "$jit_ns" -ge "$nojit_ns" ]; then
        echo "$mode JIT regression: $jit_ns ns is not faster than interpreter $nojit_ns ns" >&2
        exit 1
    fi
done

CRATONVM_DBG=jit-mic taskset -c "$CPU" "$EXE" --java-home "$JAVA_HOME_ARG" -cp "$CLASSES" \
    ArchitectureRuntimeProbe20260726 dispatch-poly4 10000 >/dev/null 2>"$DIAG"
helper_calls="$(grep -c 'JIT_MIC.*ArchitectureRuntimeProbe20260726[$]Op.apply' "$DIAG" || true)"
if [ "$helper_calls" -gt 8 ]; then
    echo "poly4 cache did not stabilize: $helper_calls helper calls (expected <= 8)" >&2
    exit 1
fi
echo "validated interface checksums, JIT speedups in all dispatch shapes, and cloned-cache stabilization ($helper_calls helper calls)"
