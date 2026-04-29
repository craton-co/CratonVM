#!/usr/bin/env bash
# bench/wave2-4/stage-jacoco-probe.sh
# WP2.4-D — stage the JaCoCo coverage agent acceptance probe.
#
# Behaviour:
#   * Locate jacocoagent.jar from the candidate list (env override +
#     known vendor locations under C:/craton/ejbca-ce/lib/coverage/).
#   * If absent: write skipped.flag + skip-with-message; harness still
#     exits 0.
#   * javac --release 21 of apps/jacoco_probe/{Target,Main}.java into
#     staged-jacoco/classes.
#   * Copy located agent jar to staged-jacoco/jacocoagent.jar.
#
# Exit codes: 0 ok or skipped, 10 javac missing, 12 compile failure.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
STAGED_DIR="$HERE/staged-jacoco"
CLASSES_DIR="$STAGED_DIR/classes"
COMPILE_LOG="$STAGED_DIR/compile.log"
MAIN_CLASS_FILE="$STAGED_DIR/main-class.txt"
JAR_PATH_FILE="$STAGED_DIR/jar-path.txt"
SKIP_FLAG="$STAGED_DIR/skipped.flag"
SRC_DIR="$REPO_ROOT/apps/jacoco_probe"

mkdir -p "$CLASSES_DIR"
: > "$COMPILE_LOG"
rm -f "$SKIP_FLAG"

# Locate javac.
JAVAC=""
if [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/javac" ]];     then JAVAC="$JAVA_HOME/bin/javac"; fi
if [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/javac.exe" ]]; then JAVAC="$JAVA_HOME/bin/javac.exe"; fi
if [[ -z "$JAVAC" ]] && command -v javac >/dev/null 2>&1;      then JAVAC="$(command -v javac)"; fi
if [[ -z "$JAVAC" ]]; then
    for root in "/c/Program Files/Java/jdk-25" "/c/Program Files/Java/jdk-21"; do
        if [[ -x "$root/bin/javac.exe" ]]; then JAVAC="$root/bin/javac.exe"; break; fi
    done
fi
if [[ -z "$JAVAC" ]]; then
    echo "stage-jacoco-probe: ERROR javac not found" >&2
    exit 10
fi
echo "stage-jacoco-probe: using javac at $JAVAC" | tee -a "$COMPILE_LOG"

# Locate jacocoagent.jar.
JACOCO_JAR=""
candidates=()
[[ -n "${JACOCO_AGENT_JAR:-}" ]] && candidates+=("$JACOCO_AGENT_JAR")

shopt -s nullglob
candidates+=( /c/craton/ejbca-ce/lib/coverage/jacocoagent.jar )
candidates+=( /c/craton/ejbca-ce/lib/coverage/org.jacoco.agent-*-runtime.jar )
if [[ -n "${HOME:-}" ]]; then
    candidates+=( "$HOME"/.m2/repository/org/jacoco/org.jacoco.agent/*/org.jacoco.agent-*-runtime.jar )
fi
if [[ -n "${USERPROFILE:-}" ]]; then
    UP_BASH="/$(echo "${USERPROFILE:0:1}" | tr '[:upper:]' '[:lower:]')/${USERPROFILE:3}"
    UP_BASH="${UP_BASH//\\//}"
    candidates+=( "$UP_BASH"/.m2/repository/org/jacoco/org.jacoco.agent/*/org.jacoco.agent-*-runtime.jar )
fi
candidates+=( /c/Users/Victor/.m2/repository/org/jacoco/org.jacoco.agent/*/org.jacoco.agent-*-runtime.jar )
shopt -u nullglob

for cand in "${candidates[@]}"; do
    if [[ -f "$cand" ]]; then
        JACOCO_JAR="$cand"
        break
    fi
done

if [[ -z "$JACOCO_JAR" ]]; then
    # Maven Central fallback — download jacocoagent runtime into staged-jacoco/cache/.
    CACHE_DIR="$STAGED_DIR/cache"
    JACOCO_VER="${JACOCO_VERSION:-0.8.12}"
    JACOCO_CACHED="$CACHE_DIR/org.jacoco.agent-${JACOCO_VER}-runtime.jar"
    if [[ -f "$JACOCO_CACHED" ]]; then
        echo "stage-jacoco-probe: using cached jacoco at $JACOCO_CACHED" | tee -a "$COMPILE_LOG"
        JACOCO_JAR="$JACOCO_CACHED"
    elif [[ "${NO_NET:-0}" == "1" ]]; then
        echo "stage-jacoco-probe: NO_NET=1; skipping Maven Central fallback" | tee -a "$COMPILE_LOG"
    elif command -v curl >/dev/null 2>&1; then
        mkdir -p "$CACHE_DIR"
        MC_BASE="${MAVEN_CENTRAL_BASE:-https://repo1.maven.org/maven2}"
        URL="$MC_BASE/org/jacoco/org.jacoco.agent/${JACOCO_VER}/org.jacoco.agent-${JACOCO_VER}-runtime.jar"
        echo "stage-jacoco-probe: fetching $URL" | tee -a "$COMPILE_LOG"
        if curl -fsSL --retry 2 --connect-timeout 30 -o "$JACOCO_CACHED" "$URL" >> "$COMPILE_LOG" 2>&1; then
            JACOCO_JAR="$JACOCO_CACHED"
            echo "stage-jacoco-probe: downloaded jacoco to $JACOCO_CACHED" | tee -a "$COMPILE_LOG"
        else
            echo "stage-jacoco-probe: Maven Central download failed (continuing to skip)" | tee -a "$COMPILE_LOG"
            rm -f "$JACOCO_CACHED"
        fi
    fi
fi

if [[ -z "$JACOCO_JAR" ]]; then
    echo "stage-jacoco-probe: SKIP jacocoagent.jar not found" | tee -a "$COMPILE_LOG"
    echo "  searched: \$JACOCO_AGENT_JAR, /c/craton/ejbca-ce/lib/coverage, ~/.m2/repository/org/jacoco" | tee -a "$COMPILE_LOG"
    echo "  attempted: Maven Central (set NO_NET=1 to skip; \$MAVEN_CENTRAL_BASE to override mirror)" | tee -a "$COMPILE_LOG"
    echo "  set JACOCO_AGENT_JAR=/path/to/jacocoagent.jar to enable this probe" | tee -a "$COMPILE_LOG"
    touch "$SKIP_FLAG"
fi

# Compile probes (no jar dependency at compile time — JaCoCo instruments
# at load time via -javaagent:).
SOURCES=(
    "$SRC_DIR/Target.java"
    "$SRC_DIR/Main.java"
)
echo "stage-jacoco-probe: compiling ${#SOURCES[@]} sources" | tee -a "$COMPILE_LOG"
if ! "$JAVAC" --release 21 -d "$CLASSES_DIR" "${SOURCES[@]}" >> "$COMPILE_LOG" 2>&1; then
    echo "stage-jacoco-probe: ERROR javac failed; see $COMPILE_LOG" >&2
    tail -n 40 "$COMPILE_LOG" >&2 || true
    exit 12
fi

if [[ -n "$JACOCO_JAR" ]]; then
    echo "stage-jacoco-probe: located jacoco agent at $JACOCO_JAR" | tee -a "$COMPILE_LOG"
    echo "$JACOCO_JAR" > "$JAR_PATH_FILE"
    cp -f "$JACOCO_JAR" "$STAGED_DIR/jacocoagent.jar"
fi

echo "Main" > "$MAIN_CLASS_FILE"
echo "stage-jacoco-probe: main class = $(cat "$MAIN_CLASS_FILE")" | tee -a "$COMPILE_LOG"
echo "stage-jacoco-probe: staged classes at $CLASSES_DIR"        | tee -a "$COMPILE_LOG"
echo "stage-jacoco-probe: OK"
exit 0
