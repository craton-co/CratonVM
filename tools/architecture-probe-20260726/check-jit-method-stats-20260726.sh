#!/usr/bin/env bash
set -euo pipefail

EXE=""
JAVA_HOME_ARG=""

usage() {
    echo "usage: $0 -Exe /absolute/cratonvm --java-home /absolute/jdk" >&2
    exit 2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -Exe) EXE="$2"; shift 2 ;;
        --java-home) JAVA_HOME_ARG="$2"; shift 2 ;;
        *) usage ;;
    esac
done

[ -x "$EXE" ] || usage
[ -d "$JAVA_HOME_ARG" ] || usage

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLASSES="$(mktemp -d)"
trap 'rm -rf "$CLASSES"' EXIT
javac -d "$CLASSES" "$SCRIPT_DIR/ArchitectureJitStatsExitProbe20260726.java"

for mode in return exit; do
    stderr="$CLASSES/$mode.stderr"
    stdout="$CLASSES/$mode.stdout"
    CRATONVM_DBG=jit-method-stats \
        timeout 120 "$EXE" --java-home "$JAVA_HOME_ARG" \
        -cp "$CLASSES" ArchitectureJitStatsExitProbe20260726 "$mode" \
        >"$stdout" 2>"$stderr"

    count="$(grep -c '^\[cratonvm\] JIT method stats:' "$stderr" || true)"
    if [ "$count" -ne 1 ]; then
        echo "$mode: expected exactly one JIT method-statistics record, got $count" >&2
        cat "$stderr" >&2
        exit 1
    fi
    if grep -q 'unknown configuration token' "$stderr"; then
        echo "$mode: grouped flag was rejected" >&2
        cat "$stderr" >&2
        exit 1
    fi
    echo "$mode: exactly one JIT method-statistics record"
done
