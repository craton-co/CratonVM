#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EXE=""
JAVA_HOME_ARG="${JAVA_HOME:-}"
ITERATIONS=500000
CPU=13

usage() {
    echo "usage: $0 -Exe /absolute/cratonvm [--java-home DIR] [--iterations N] [--cpu N]" >&2
    exit 2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -Exe) EXE="$2"; shift 2 ;;
        --java-home) JAVA_HOME_ARG="$2"; shift 2 ;;
        --iterations) ITERATIONS="$2"; shift 2 ;;
        --cpu) CPU="$2"; shift 2 ;;
        *) usage ;;
    esac
done

[ -x "$EXE" ] || usage
[ -x "$JAVA_HOME_ARG/bin/java" ] || usage
[ -x "$JAVA_HOME_ARG/bin/javac" ] || usage

CLASSES="$(mktemp -d)"
JIT_OUT="$(mktemp)"
NOJIT_OUT="$(mktemp)"
DIAG="$(mktemp)"
trap 'rm -rf "$CLASSES"; rm -f "$JIT_OUT" "$NOJIT_OUT" "$DIAG"' EXIT

"$JAVA_HOME_ARG/bin/javac" -d "$CLASSES" \
    "$SCRIPT_DIR/ArchitectureMonitorJitProbe20260727.java"

taskset -c "$CPU" "$EXE" --java-home "$JAVA_HOME_ARG" -cp "$CLASSES" \
    ArchitectureMonitorJitProbe20260727 "$ITERATIONS" >"$JIT_OUT"
taskset -c "$CPU" "$EXE" --nojit --java-home "$JAVA_HOME_ARG" -cp "$CLASSES" \
    ArchitectureMonitorJitProbe20260727 "$ITERATIONS" >"$NOJIT_OUT"

jit_line="$(tail -1 "$JIT_OUT")"
nojit_line="$(tail -1 "$NOJIT_OUT")"
IFS=$'\t' read -r jit_probe jit_iterations jit_elapsed jit_checksum <<<"$jit_line"
IFS=$'\t' read -r nojit_probe nojit_iterations nojit_elapsed nojit_checksum <<<"$nojit_line"

if [ "$jit_probe" != monitor-entry ] || [ "$nojit_probe" != monitor-entry ] ||
    [ "$jit_iterations" != "$ITERATIONS" ] || [ "$nojit_iterations" != "$ITERATIONS" ] ||
    [ "$jit_checksum" != "$nojit_checksum" ]; then
    echo "monitor probe mismatch" >&2
    echo "jit:   $jit_line" >&2
    echo "nojit: $nojit_line" >&2
    exit 1
fi

CRATONVM_DBG_RBC6=1 CRATONVM_DBG_JITC=1 taskset -c "$CPU" \
    "$EXE" --java-home "$JAVA_HOME_ARG" -cp "$CLASSES" \
    ArchitectureMonitorJitProbe20260727 200000 >/dev/null 2>"$DIAG"

grep -q 'bg-compile ArchitectureMonitorJitProbe20260727.lockedAdd' "$DIAG" || {
    echo "lockedAdd did not reach method-entry JIT compilation" >&2
    tail -80 "$DIAG" >&2
    exit 1
}
grep -q 'bg-compile ArchitectureMonitorJitProbe20260727.lockedMaybeFail' "$DIAG" || {
    echo "lockedMaybeFail did not reach method-entry JIT compilation" >&2
    tail -80 "$DIAG" >&2
    exit 1
}
if grep -Eq 'compile FAILED ArchitectureMonitorJitProbe20260727\.(lockedAdd|lockedMaybeFail)' "$DIAG"; then
    echo "a monitor probe method reached the compiler but failed lowering" >&2
    tail -80 "$DIAG" >&2
    exit 1
fi

printf 'validated monitor checksum=%s jit_ns=%s nojit_ns=%s, exception cleanup, and method-entry compilation\n' \
    "$jit_checksum" "$jit_elapsed" "$nojit_elapsed"
