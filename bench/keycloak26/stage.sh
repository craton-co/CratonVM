#!/usr/bin/env bash
# bench/keycloak26/stage.sh
# WP8.7 — stage the keycloak26 forcing-function fixture.
#
# Network requirement: NONE today (placeholder fixture is in-tree). When
# real-distribution staging is wired (per-app TODO below), this script may
# `curl` an upstream tarball — CI is assumed to have outbound network.
#
# Real-distribution slot (not yet wired):
#   * If $KEYCLOAK26_HOME points at an unpacked distribution, prefer it.
#   * Tarball URL TODO; expected to download in <5min on CI.
#
# Behaviour today: javac --release 21 of fixture/Main.java -> staged/classes/.
# Idempotent — safe to re-run.
#
# Exit codes: 0 ok, 10 javac missing, 12 compile failure.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STAGED_DIR="$HERE/staged"
CLASSES_DIR="$STAGED_DIR/classes"
COMPILE_LOG="$STAGED_DIR/compile.log"
MAIN_CLASS_FILE="$STAGED_DIR/main-class.txt"
SRC_DIR="$HERE/fixture"

mkdir -p "$CLASSES_DIR"
: > "$COMPILE_LOG"

if [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/javac" ]]; then
    JAVAC="$JAVA_HOME/bin/javac"
elif [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/javac.exe" ]]; then
    JAVAC="$JAVA_HOME/bin/javac.exe"
elif command -v javac >/dev/null 2>&1; then
    JAVAC="$(command -v javac)"
else
    echo "stage-keycloak26: ERROR javac not found (JAVA_HOME unset and javac not on PATH)" >&2
    exit 10
fi
echo "stage-keycloak26: using javac at $JAVAC" | tee -a "$COMPILE_LOG"

if [[ ! -f "$SRC_DIR/Main.java" ]]; then
    echo "stage-keycloak26: ERROR $SRC_DIR/Main.java missing" >&2
    exit 11
fi

echo "stage-keycloak26: compiling fixture/Main.java" | tee -a "$COMPILE_LOG"
if ! "$JAVAC" --release 21 -d "$CLASSES_DIR" "$SRC_DIR/Main.java" >> "$COMPILE_LOG" 2>&1; then
    echo "stage-keycloak26: ERROR javac failed; see $COMPILE_LOG" >&2
    tail -n 40 "$COMPILE_LOG" >&2 || true
    exit 12
fi

echo "Main" > "$MAIN_CLASS_FILE"
echo "stage-keycloak26: main class = Main" | tee -a "$COMPILE_LOG"
echo "stage-keycloak26: staged classes at $CLASSES_DIR" | tee -a "$COMPILE_LOG"
echo "stage-keycloak26: OK"
exit 0
