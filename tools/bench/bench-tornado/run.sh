#!/usr/bin/env bash
# Runner for VectorAddTornado on the PTX backend.
#
# Usage:
#   ./run.sh                # n=1024 iters=10
#   ./run.sh 1048576 50     # custom
#   ./run.sh --thread-info  # forwards --threadInfo to tornado
#
# Re-compiles the .class on demand if missing/stale.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Bring TornadoVM + JDK into scope.
# shellcheck disable=SC1091
source /c/craton/tornadovm/setvars.sh >/dev/null

API_JAR="C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/share/java/tornado/tornado-api-4.0.1-jdk25.jar"
ANNO_JAR="C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/share/java/tornado/tornado-annotation-4.0.1-jdk25.jar"

EXTRA_FLAGS=""
ARGS=()
for a in "$@"; do
    case "$a" in
        --thread-info|--threadInfo) EXTRA_FLAGS="$EXTRA_FLAGS --threadInfo" ;;
        --debug)                    EXTRA_FLAGS="$EXTRA_FLAGS --debug"      ;;
        *) ARGS+=("$a") ;;
    esac
done

# CRITICAL: compile with -g so Graal sees the LocalVariableTable
# (otherwise TornadoVM bails to the sequential CPU fallback).
if [ ! -f VectorAddTornado.class ] || [ VectorAddTornado.java -nt VectorAddTornado.class ]; then
    echo "[run.sh] compiling VectorAddTornado.java (with -g)..." >&2
    javac -g --enable-preview --release 25 -cp "$API_JAR;$ANNO_JAR" VectorAddTornado.java
fi

N="${ARGS[0]:-1024}"
ITERS="${ARGS[1]:-10}"

exec tornado $EXTRA_FLAGS --classpath . VectorAddTornado "$N" "$ITERS"
