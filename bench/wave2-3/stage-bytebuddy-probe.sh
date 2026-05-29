#!/usr/bin/env bash
# bench/wave2-3/stage-bytebuddy-probe.sh
# WP2.3-D — stage the ByteBuddy acceptance probes (real DSL +
# legacy synthetic placeholder) for end-to-end execution under
# cratonvm. Mirrors the shape of bench/wildfly/stage-ejbca-min.sh.
#
# Behaviour mirrors stage-cglib-probe.sh:
#   * Locate byte-buddy-X.Y.Z.jar from local maven repo, vendor lib,
#     or $BYTEBUDDY_JAR override. If absent, stage only the synthetic
#     probe and write a SKIP flag.
#   * javac --release 21 the probes into bench/wave2-3/staged-bytebuddy/classes.
#
# Exit codes: 0 ok or skipped, 10 javac missing, 12 compile failure.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
STAGED_DIR="$HERE/staged-bytebuddy"
CLASSES_DIR="$STAGED_DIR/classes"
COMPILE_LOG="$STAGED_DIR/compile.log"
MAIN_CLASS_FILE="$STAGED_DIR/main-class.txt"
JAR_PATH_FILE="$STAGED_DIR/jar-path.txt"
SKIP_FLAG="$STAGED_DIR/skipped.flag"
SRC_DIR="$REPO_ROOT/apps/bytebuddy_probe"

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
    echo "stage-bytebuddy-probe: ERROR javac not found" >&2
    exit 10
fi
echo "stage-bytebuddy-probe: using javac at $JAVAC" | tee -a "$COMPILE_LOG"

# ---------------------------------------------------------------------------
# Locate the byte-buddy jar.
# ---------------------------------------------------------------------------
BB_JAR_PATH=""
candidates=()
[[ -n "${BYTEBUDDY_JAR:-}" ]] && candidates+=("$BYTEBUDDY_JAR")

shopt -s nullglob
if [[ -n "${HOME:-}" ]]; then
    candidates+=( "$HOME"/.m2/repository/net/bytebuddy/byte-buddy/*/byte-buddy-*.jar )
fi
if [[ -n "${USERPROFILE:-}" ]]; then
    UP_BASH="/$(echo "${USERPROFILE:0:1}" | tr '[:upper:]' '[:lower:]')/${USERPROFILE:3}"
    UP_BASH="${UP_BASH//\\//}"
    candidates+=( "$UP_BASH"/.m2/repository/net/bytebuddy/byte-buddy/*/byte-buddy-*.jar )
fi
candidates+=( "$HOME"/.m2/repository/net/bytebuddy/byte-buddy/*/byte-buddy-*.jar )
candidates+=( /c/craton/ejbca-ce/lib/hibernate/byte-buddy-*.jar )
candidates+=( /c/craton/keycloak-*/modules/system/layers/base/net/bytebuddy/main/byte-buddy-*.jar )
shopt -u nullglob

# Strict filter — exclude byte-buddy-agent and byte-buddy-android jars; we
# want the core "byte-buddy-X.Y.Z.jar" artifact.
for cand in "${candidates[@]}"; do
    base="$(basename "$cand")"
    if [[ -f "$cand" && "$base" =~ ^byte-buddy-[0-9].*\.jar$ && "$base" != *"agent"* && "$base" != *"android"* ]]; then
        BB_JAR_PATH="$cand"
        break
    fi
done

if [[ -z "$BB_JAR_PATH" ]]; then
    # Maven Central fallback — download byte-buddy core into staged-bytebuddy/cache/.
    CACHE_DIR="$STAGED_DIR/cache"
    BB_VER="${BYTEBUDDY_VERSION:-1.14.19}"
    BB_CACHED="$CACHE_DIR/byte-buddy-${BB_VER}.jar"
    if [[ -f "$BB_CACHED" ]]; then
        echo "stage-bytebuddy-probe: using cached byte-buddy at $BB_CACHED" | tee -a "$COMPILE_LOG"
        BB_JAR_PATH="$BB_CACHED"
    elif [[ "${NO_NET:-0}" == "1" ]]; then
        echo "stage-bytebuddy-probe: NO_NET=1; skipping Maven Central fallback" | tee -a "$COMPILE_LOG"
    elif command -v curl >/dev/null 2>&1; then
        mkdir -p "$CACHE_DIR"
        MC_BASE="${MAVEN_CENTRAL_BASE:-https://repo1.maven.org/maven2}"
        URL="$MC_BASE/net/bytebuddy/byte-buddy/${BB_VER}/byte-buddy-${BB_VER}.jar"
        echo "stage-bytebuddy-probe: fetching $URL" | tee -a "$COMPILE_LOG"
        if curl -fsSL --retry 2 --connect-timeout 30 -o "$BB_CACHED" "$URL" >> "$COMPILE_LOG" 2>&1; then
            BB_JAR_PATH="$BB_CACHED"
            echo "stage-bytebuddy-probe: downloaded byte-buddy to $BB_CACHED" | tee -a "$COMPILE_LOG"
        else
            echo "stage-bytebuddy-probe: Maven Central download failed (continuing to skip)" | tee -a "$COMPILE_LOG"
            rm -f "$BB_CACHED"
        fi
    fi
fi

if [[ -z "$BB_JAR_PATH" ]]; then
    echo "stage-bytebuddy-probe: SKIP byte-buddy jar not found" | tee -a "$COMPILE_LOG"
    echo "  attempted: Maven Central (set NO_NET=1 to skip; \$MAVEN_CENTRAL_BASE to override mirror)" | tee -a "$COMPILE_LOG"
    echo "  set BYTEBUDDY_JAR=/path/to/byte-buddy-X.Y.Z.jar to enable real-DSL probe" | tee -a "$COMPILE_LOG"
    touch "$SKIP_FLAG"
fi

# ---------------------------------------------------------------------------
# Compile.
# ---------------------------------------------------------------------------
# Always compile the synthetic probe (no jar needed).
SOURCES_BASE=(
    "$SRC_DIR/Greeter.java"
    "$SRC_DIR/HiGreeter.java"
    "$SRC_DIR/ByteBuddyProbeSynth.java"
)

echo "stage-bytebuddy-probe: compiling synthetic probe (${#SOURCES_BASE[@]} sources)" | tee -a "$COMPILE_LOG"
if ! "$JAVAC" --release 21 -d "$CLASSES_DIR" "${SOURCES_BASE[@]}" >> "$COMPILE_LOG" 2>&1; then
    echo "stage-bytebuddy-probe: ERROR javac (synthetic probe) failed; see $COMPILE_LOG" >&2
    tail -n 40 "$COMPILE_LOG" >&2 || true
    exit 12
fi

mkdir -p "$STAGED_DIR/payload"
if [[ -f "$SRC_DIR/payload/HiGreeter.class" ]]; then
    cp -f "$SRC_DIR/payload/HiGreeter.class" "$STAGED_DIR/payload/HiGreeter.class"
fi

if [[ -n "$BB_JAR_PATH" ]]; then
    echo "stage-bytebuddy-probe: located byte-buddy jar at $BB_JAR_PATH" | tee -a "$COMPILE_LOG"
    echo "$BB_JAR_PATH" > "$JAR_PATH_FILE"
    cp -f "$BB_JAR_PATH" "$STAGED_DIR/byte-buddy.jar"
    echo "stage-bytebuddy-probe: compiling ByteBuddyProbe with jar on cp" | tee -a "$COMPILE_LOG"
    if ! "$JAVAC" --release 21 -cp "$BB_JAR_PATH" -d "$CLASSES_DIR" "$SRC_DIR/ByteBuddyProbe.java" >> "$COMPILE_LOG" 2>&1; then
        echo "stage-bytebuddy-probe: ERROR javac (ByteBuddyProbe) failed; see $COMPILE_LOG" >&2
        tail -n 40 "$COMPILE_LOG" >&2 || true
        exit 12
    fi
    echo "ByteBuddyProbe" > "$MAIN_CLASS_FILE"
else
    echo "stage-bytebuddy-probe: jar absent — main probe = synthetic ByteBuddyProbeSynth" | tee -a "$COMPILE_LOG"
    echo "ByteBuddyProbeSynth" > "$MAIN_CLASS_FILE"
fi

echo "stage-bytebuddy-probe: main class = $(cat "$MAIN_CLASS_FILE")" | tee -a "$COMPILE_LOG"
echo "stage-bytebuddy-probe: staged classes at $CLASSES_DIR" | tee -a "$COMPILE_LOG"
echo "stage-bytebuddy-probe: OK"
exit 0
