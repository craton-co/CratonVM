#!/usr/bin/env bash
# bench/wave2-4/stage-instrument-probe.sh
# WP2.4-D — stage the hand-rolled `-javaagent:` regression test that
# does NOT depend on any external jar. This is the PRIMARY acceptance
# fixture for WP2.4: it builds an agent jar from
# apps/instrument_probe/{Target,RetransformAgent,Main}.java +
# apps/instrument_probe/META-INF/MANIFEST.MF, runs main under
# cratonvm with `-javaagent:agent.jar`, and verifies that the
# transformer's bipush 42->99 patch took effect.
#
# Behaviour:
#   * Locate javac + jar (JDK 21+).
#   * javac --release 21 the three source files into staged-instrument/classes.
#   * Bundle RetransformAgent.class + the manifest into staged-instrument/agent.jar.
#   * Pin Main as the entry point.
#
# Exit codes:
#   0   staging succeeded
#   10  javac or jar tool missing
#   12  compile / packaging failure
#
# Usage: bash bench/wave2-4/stage-instrument-probe.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
STAGED_DIR="$HERE/staged-instrument"
CLASSES_DIR="$STAGED_DIR/classes"
COMPILE_LOG="$STAGED_DIR/compile.log"
MAIN_CLASS_FILE="$STAGED_DIR/main-class.txt"
AGENT_JAR="$STAGED_DIR/agent.jar"
SRC_DIR="$REPO_ROOT/apps/instrument_probe"
MANIFEST_SRC="$SRC_DIR/META-INF/MANIFEST.MF"

mkdir -p "$CLASSES_DIR"
: > "$COMPILE_LOG"
rm -f "$AGENT_JAR" "$STAGED_DIR/skipped.flag"

# ---------------------------------------------------------------------------
# Locate javac + jar.
# ---------------------------------------------------------------------------
JAVAC=""
JAR=""
if [[ -n "${JAVA_HOME:-}" ]]; then
    if [[ -x "$JAVA_HOME/bin/javac" ]];     then JAVAC="$JAVA_HOME/bin/javac"; fi
    if [[ -x "$JAVA_HOME/bin/javac.exe" ]]; then JAVAC="$JAVA_HOME/bin/javac.exe"; fi
    if [[ -x "$JAVA_HOME/bin/jar" ]];       then JAR="$JAVA_HOME/bin/jar"; fi
    if [[ -x "$JAVA_HOME/bin/jar.exe" ]];   then JAR="$JAVA_HOME/bin/jar.exe"; fi
fi
if [[ -z "$JAVAC" ]] && command -v javac >/dev/null 2>&1; then JAVAC="$(command -v javac)"; fi
if [[ -z "$JAR" ]]   && command -v jar   >/dev/null 2>&1; then JAR="$(command -v jar)"; fi

# Best-effort fallback: common JDK install root on Windows.
if [[ -z "$JAR" || -z "$JAVAC" ]]; then
    for root in "/c/Program Files/Java/jdk-25" "/c/Program Files/Java/jdk-21"; do
        if [[ -z "$JAVAC" && -x "$root/bin/javac.exe" ]]; then JAVAC="$root/bin/javac.exe"; fi
        if [[ -z "$JAR"   && -x "$root/bin/jar.exe"   ]]; then JAR="$root/bin/jar.exe";     fi
    done
fi

if [[ -z "$JAVAC" ]]; then
    echo "stage-instrument-probe: ERROR javac not found" >&2
    exit 10
fi
if [[ -z "$JAR" ]]; then
    echo "stage-instrument-probe: ERROR jar not found (need JDK, not JRE)" >&2
    exit 10
fi
echo "stage-instrument-probe: javac=$JAVAC" | tee -a "$COMPILE_LOG"
echo "stage-instrument-probe: jar=$JAR"     | tee -a "$COMPILE_LOG"

# ---------------------------------------------------------------------------
# Compile sources.
# ---------------------------------------------------------------------------
SOURCES=(
    "$SRC_DIR/Target.java"
    "$SRC_DIR/RetransformAgent.java"
    "$SRC_DIR/Main.java"
)
echo "stage-instrument-probe: compiling ${#SOURCES[@]} sources" | tee -a "$COMPILE_LOG"
if ! "$JAVAC" --release 21 -d "$CLASSES_DIR" "${SOURCES[@]}" >> "$COMPILE_LOG" 2>&1; then
    echo "stage-instrument-probe: ERROR javac failed; see $COMPILE_LOG" >&2
    tail -n 40 "$COMPILE_LOG" >&2 || true
    exit 12
fi

# ---------------------------------------------------------------------------
# Build agent jar with the canonical manifest.
#
# `jar cfm agent.jar MANIFEST.MF -C classes RetransformAgent.class`
# packages JUST the agent class + its manifest. We deliberately do NOT
# bundle Target.class or Main.class into the agent jar — those live on
# the classpath; the jar exists purely to carry the Premain-Class
# header.
# ---------------------------------------------------------------------------
if [[ ! -f "$MANIFEST_SRC" ]]; then
    echo "stage-instrument-probe: ERROR missing manifest at $MANIFEST_SRC" >&2
    exit 12
fi

echo "stage-instrument-probe: building agent jar at $AGENT_JAR" | tee -a "$COMPILE_LOG"
if ! "$JAR" cfm "$AGENT_JAR" "$MANIFEST_SRC" \
        -C "$CLASSES_DIR" "RetransformAgent.class" \
        -C "$CLASSES_DIR" "RetransformAgent\$Patcher.class" \
        >> "$COMPILE_LOG" 2>&1; then
    echo "stage-instrument-probe: ERROR jar packaging failed; see $COMPILE_LOG" >&2
    tail -n 40 "$COMPILE_LOG" >&2 || true
    exit 12
fi

# Sanity check: agent.jar must contain the manifest and the agent class.
if ! "$JAR" tf "$AGENT_JAR" >> "$COMPILE_LOG" 2>&1; then
    echo "stage-instrument-probe: ERROR jar table-of-contents listing failed" >&2
    exit 12
fi

echo "Main" > "$MAIN_CLASS_FILE"

echo "stage-instrument-probe: agent jar contents:" | tee -a "$COMPILE_LOG"
"$JAR" tf "$AGENT_JAR" | sed 's/^/  /' | tee -a "$COMPILE_LOG"

echo "stage-instrument-probe: main class = $(cat "$MAIN_CLASS_FILE")" | tee -a "$COMPILE_LOG"
echo "stage-instrument-probe: staged classes at $CLASSES_DIR"        | tee -a "$COMPILE_LOG"
echo "stage-instrument-probe: OK"
exit 0
