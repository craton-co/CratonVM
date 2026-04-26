#!/usr/bin/env bash
# bench/wave2-3/stage-cglib-probe.sh
# WP2.3-D — stage the CGLIB acceptance probes (real Enhancer.create() +
# the legacy synthetic placeholder) for end-to-end execution under
# rust-jvm. Mirrors the shape of bench/wildfly/stage-ejbca-min.sh.
#
# Behaviour:
#   * Locate cglib-nodep-X.Y.jar under one of the well-known local
#     maven / project locations. If none is found, write a clear
#     skip-with-instructions message and exit 0 (so CI doesn't fail
#     just because the workstation doesn't have a local CGLIB jar).
#   * javac --release 21 the probes into bench/wave2-3/staged-cglib/classes.
#   * Copy the located jar next to the staged classes for run scripts.
#   * Pin the probe entry point in main-class.txt.
#
# Exit codes:
#   0   staging succeeded OR cglib jar missing (printed 'skip')
#   10  javac missing
#   12  compile failure (report captured in staged-cglib/compile.log)
#
# Usage: bash bench/wave2-3/stage-cglib-probe.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
STAGED_DIR="$HERE/staged-cglib"
CLASSES_DIR="$STAGED_DIR/classes"
COMPILE_LOG="$STAGED_DIR/compile.log"
MAIN_CLASS_FILE="$STAGED_DIR/main-class.txt"
JAR_PATH_FILE="$STAGED_DIR/jar-path.txt"
SKIP_FLAG="$STAGED_DIR/skipped.flag"
SRC_DIR="$REPO_ROOT/apps/cglib_probe"

mkdir -p "$CLASSES_DIR"
: > "$COMPILE_LOG"
rm -f "$SKIP_FLAG"

# ---------------------------------------------------------------------------
# Locate javac.
# ---------------------------------------------------------------------------
if [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/javac" ]]; then
    JAVAC="$JAVA_HOME/bin/javac"
elif [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/javac.exe" ]]; then
    JAVAC="$JAVA_HOME/bin/javac.exe"
elif command -v javac >/dev/null 2>&1; then
    JAVAC="$(command -v javac)"
else
    echo "stage-cglib-probe: ERROR javac not found (JAVA_HOME unset and javac not on PATH)" >&2
    exit 10
fi
echo "stage-cglib-probe: using javac at $JAVAC" | tee -a "$COMPILE_LOG"

# ---------------------------------------------------------------------------
# Locate the cglib-nodep jar.
#
# Search order (first hit wins):
#   1. $CGLIB_JAR (env override)
#   2. local maven repo: ~/.m2/repository/cglib/cglib-nodep/*/cglib-nodep-*.jar
#                        $USERPROFILE/.m2/...                        (Windows)
#   3. C:/Users/Victor/.m2/repository/cglib/cglib-nodep/*/cglib-nodep-*.jar
#   4. C:/craton/ejbca-ce/lib/cglib*.jar  (best-effort vendor location)
#   5. C:/craton/keycloak-*/**/cglib*.jar
# If none, we still stage the legacy synthetic probe (no jar needed) and
# leave a SKIP flag for the run script.
# ---------------------------------------------------------------------------
CGLIB_JAR_PATH=""
candidates=()
[[ -n "${CGLIB_JAR:-}" ]] && candidates+=("$CGLIB_JAR")

# Build candidate list; let glob expansion drop into nul if no match.
shopt -s nullglob
if [[ -n "${HOME:-}" ]]; then
    candidates+=( "$HOME"/.m2/repository/cglib/cglib-nodep/*/cglib-nodep-*.jar )
    candidates+=( "$HOME"/.m2/repository/cglib/cglib/*/cglib-*.jar )
fi
if [[ -n "${USERPROFILE:-}" ]]; then
    # Translate Windows path to bash form.
    UP_BASH="/$(echo "${USERPROFILE:0:1}" | tr '[:upper:]' '[:lower:]')/${USERPROFILE:3}"
    UP_BASH="${UP_BASH//\\//}"
    candidates+=( "$UP_BASH"/.m2/repository/cglib/cglib-nodep/*/cglib-nodep-*.jar )
    candidates+=( "$UP_BASH"/.m2/repository/cglib/cglib/*/cglib-*.jar )
fi
candidates+=( /c/Users/Victor/.m2/repository/cglib/cglib-nodep/*/cglib-nodep-*.jar )
candidates+=( /c/craton/ejbca-ce/lib/cglib*.jar )
candidates+=( /c/craton/keycloak-*/modules/system/layers/base/cglib/*/main/cglib*.jar )
candidates+=( /c/craton/keycloak-*/modules/system/layers/base/net/sf/cglib/main/cglib*.jar )
shopt -u nullglob

for cand in "${candidates[@]}"; do
    if [[ -f "$cand" ]]; then
        CGLIB_JAR_PATH="$cand"
        break
    fi
done

if [[ -z "$CGLIB_JAR_PATH" ]]; then
    echo "stage-cglib-probe: SKIP cglib-nodep jar not found" | tee -a "$COMPILE_LOG"
    echo "  searched: \$CGLIB_JAR, ~/.m2/repository/cglib/, C:/Users/Victor/.m2, C:/craton/ejbca-ce/lib, C:/craton/keycloak-*" | tee -a "$COMPILE_LOG"
    echo "  set CGLIB_JAR=/path/to/cglib-nodep-X.Y.jar to enable the real-DSL probe" | tee -a "$COMPILE_LOG"
    touch "$SKIP_FLAG"
fi

# ---------------------------------------------------------------------------
# Compile the probes.
#
# Always compile the legacy synthetic probe (no jar needed). If the jar is
# present, also compile CglibProbe2 with the jar on the classpath.
# ---------------------------------------------------------------------------
SOURCES_BASE=(
    "$SRC_DIR/Target.java"
    "$SRC_DIR/EnhancedTarget.java"
    "$SRC_DIR/Step1.java"
    "$SRC_DIR/Step2.java"
    "$SRC_DIR/CglibProbe.java"
)

case "$(uname -s 2>/dev/null || echo Windows)" in
    MINGW*|MSYS*|CYGWIN*|Windows*) CPSEP=';' ;;
    *) CPSEP=':' ;;
esac

echo "stage-cglib-probe: compiling synthetic probe family (${#SOURCES_BASE[@]} sources)" | tee -a "$COMPILE_LOG"
if ! "$JAVAC" --release 21 -d "$CLASSES_DIR" "${SOURCES_BASE[@]}" >> "$COMPILE_LOG" 2>&1; then
    echo "stage-cglib-probe: ERROR javac (synthetic probes) failed; see $COMPILE_LOG" >&2
    tail -n 40 "$COMPILE_LOG" >&2 || true
    exit 12
fi

# Pre-build the EnhancedTarget payload bytes the synthetic probe loads
# at runtime (mirrors what apps/cglib_probe/payload/EnhancedTarget.class
# already provides — we copy it next to the staged classes for sandbox
# isolation).
mkdir -p "$STAGED_DIR/payload"
if [[ -f "$SRC_DIR/payload/EnhancedTarget.class" ]]; then
    cp -f "$SRC_DIR/payload/EnhancedTarget.class" "$STAGED_DIR/payload/EnhancedTarget.class"
fi

if [[ -n "$CGLIB_JAR_PATH" ]]; then
    echo "stage-cglib-probe: located cglib jar at $CGLIB_JAR_PATH" | tee -a "$COMPILE_LOG"
    echo "$CGLIB_JAR_PATH" > "$JAR_PATH_FILE"
    cp -f "$CGLIB_JAR_PATH" "$STAGED_DIR/cglib.jar"
    echo "stage-cglib-probe: compiling CglibProbe2 with jar on cp" | tee -a "$COMPILE_LOG"
    if ! "$JAVAC" --release 21 -cp "$CGLIB_JAR_PATH" -d "$CLASSES_DIR" "$SRC_DIR/CglibProbe2.java" >> "$COMPILE_LOG" 2>&1; then
        echo "stage-cglib-probe: ERROR javac (CglibProbe2) failed; see $COMPILE_LOG" >&2
        tail -n 40 "$COMPILE_LOG" >&2 || true
        exit 12
    fi
    echo "CglibProbe2" > "$MAIN_CLASS_FILE"
else
    echo "stage-cglib-probe: jar absent — main probe = synthetic CglibProbe" | tee -a "$COMPILE_LOG"
    echo "CglibProbe" > "$MAIN_CLASS_FILE"
fi

echo "stage-cglib-probe: main class = $(cat "$MAIN_CLASS_FILE")" | tee -a "$COMPILE_LOG"
echo "stage-cglib-probe: staged classes at $CLASSES_DIR" | tee -a "$COMPILE_LOG"
echo "stage-cglib-probe: OK"
exit 0
