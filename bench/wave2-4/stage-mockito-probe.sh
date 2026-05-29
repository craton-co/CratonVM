#!/usr/bin/env bash
# bench/wave2-4/stage-mockito-probe.sh
# WP2.4-D — stage the Mockito MockMaker acceptance probe.
#
# Mockito 5.x's inline MockMaker needs four jars on the classpath:
#   - mockito-core-X.Y.jar
#   - byte-buddy-X.Y.Z.jar
#   - byte-buddy-agent-X.Y.Z.jar  (also used as -javaagent: for runtime
#                                  attach; carries Premain-Class)
#   - objenesis-X.Y.jar
#
# This script searches the local maven repo for each, copies the located
# jars next to the staged classes, and writes a skip flag if any jar is
# missing.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
STAGED_DIR="$HERE/staged-mockito"
CLASSES_DIR="$STAGED_DIR/classes"
COMPILE_LOG="$STAGED_DIR/compile.log"
MAIN_CLASS_FILE="$STAGED_DIR/main-class.txt"
JARS_TXT="$STAGED_DIR/jars.txt"
SKIP_FLAG="$STAGED_DIR/skipped.flag"
SRC_DIR="$REPO_ROOT/apps/mockito_probe"

mkdir -p "$CLASSES_DIR"
: > "$COMPILE_LOG"
: > "$JARS_TXT"
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
    echo "stage-mockito-probe: ERROR javac not found" >&2
    exit 10
fi
echo "stage-mockito-probe: using javac at $JAVAC" | tee -a "$COMPILE_LOG"

# ---------------------------------------------------------------------------
# Helper: locate a jar by glob roots; first match wins. Reject sources +
# javadoc artifacts.
# ---------------------------------------------------------------------------
find_first_jar() {
    local label="$1"; shift
    local match
    for pattern in "$@"; do
        shopt -s nullglob
        for f in $pattern; do
            shopt -u nullglob
            local base
            base="$(basename "$f")"
            if [[ -f "$f" && "$base" != *sources* && "$base" != *javadoc* ]]; then
                echo "$f"
                return 0
            fi
        done
        shopt -u nullglob
    done
    return 1
}

# Glob root permutations: $env override, $HOME, $USERPROFILE, hardcoded.
home_m2="${HOME:-}/.m2/repository"
up_m2=""
if [[ -n "${USERPROFILE:-}" ]]; then
    UP_BASH="/$(echo "${USERPROFILE:0:1}" | tr '[:upper:]' '[:lower:]')/${USERPROFILE:3}"
    UP_BASH="${UP_BASH//\\//}"
    up_m2="$UP_BASH/.m2/repository"
fi
hard_m2="$HOME/.m2/repository"

MOCKITO_JAR=""
BB_JAR=""
BBAG_JAR=""
OBJ_JAR=""

# Mockito core.
if [[ -n "${MOCKITO_JAR:-}" && -f "$MOCKITO_JAR" ]]; then
    found="$MOCKITO_JAR"
else
    found="$(find_first_jar mockito \
        "$home_m2/org/mockito/mockito-core/*/mockito-core-*.jar" \
        "$up_m2/org/mockito/mockito-core/*/mockito-core-*.jar" \
        "$hard_m2/org/mockito/mockito-core/*/mockito-core-*.jar" \
        || true)"
fi
MOCKITO_JAR="$found"

# byte-buddy core.
if [[ -n "${BYTEBUDDY_JAR:-}" && -f "$BYTEBUDDY_JAR" ]]; then
    found="$BYTEBUDDY_JAR"
else
    found="$(find_first_jar byte-buddy \
        "$home_m2/net/bytebuddy/byte-buddy/*/byte-buddy-[0-9]*.jar" \
        "$up_m2/net/bytebuddy/byte-buddy/*/byte-buddy-[0-9]*.jar" \
        "$hard_m2/net/bytebuddy/byte-buddy/*/byte-buddy-[0-9]*.jar" \
        || true)"
fi
BB_JAR="$found"

# byte-buddy-agent.
if [[ -n "${BYTEBUDDY_AGENT_JAR:-}" && -f "$BYTEBUDDY_AGENT_JAR" ]]; then
    found="$BYTEBUDDY_AGENT_JAR"
else
    found="$(find_first_jar byte-buddy-agent \
        "$home_m2/net/bytebuddy/byte-buddy-agent/*/byte-buddy-agent-[0-9]*.jar" \
        "$up_m2/net/bytebuddy/byte-buddy-agent/*/byte-buddy-agent-[0-9]*.jar" \
        "$hard_m2/net/bytebuddy/byte-buddy-agent/*/byte-buddy-agent-[0-9]*.jar" \
        || true)"
fi
BBAG_JAR="$found"

# objenesis.
if [[ -n "${OBJENESIS_JAR:-}" && -f "$OBJENESIS_JAR" ]]; then
    found="$OBJENESIS_JAR"
else
    found="$(find_first_jar objenesis \
        "$home_m2/org/objenesis/objenesis/*/objenesis-*.jar" \
        "$up_m2/org/objenesis/objenesis/*/objenesis-*.jar" \
        "$hard_m2/org/objenesis/objenesis/*/objenesis-*.jar" \
        || true)"
fi
OBJ_JAR="$found"

# Maven Central fallback — download any missing jar into staged-mockito/cache/.
# Honours $MAVEN_CENTRAL_BASE override; respects $NO_NET=1 to skip.
CACHE_DIR="$STAGED_DIR/cache"
MC_BASE="${MAVEN_CENTRAL_BASE:-https://repo1.maven.org/maven2}"
fetch_mc() {
    # $1 = label, $2 = group/path, $3 = artifact, $4 = version, $5 = jar-base-name
    # Echoes the cached path on stdout (captured by caller); log lines go to
    # $COMPILE_LOG only (no tee — that would pollute stdout).
    local label="$1" group="$2" art="$3" ver="$4" base="$5"
    local cached="$CACHE_DIR/${base}.jar"
    if [[ -f "$cached" ]]; then
        echo "stage-mockito-probe: using cached $label at $cached" >> "$COMPILE_LOG"
        echo "$cached"; return 0
    fi
    if [[ "${NO_NET:-0}" == "1" ]]; then return 1; fi
    if ! command -v curl >/dev/null 2>&1; then return 1; fi
    mkdir -p "$CACHE_DIR"
    local url="$MC_BASE/$group/$art/$ver/${base}.jar"
    echo "stage-mockito-probe: fetching $url" >> "$COMPILE_LOG"
    if curl -fsSL --retry 2 --connect-timeout 30 -o "$cached" "$url" >> "$COMPILE_LOG" 2>&1; then
        echo "$cached"; return 0
    else
        rm -f "$cached"; return 1
    fi
}

if [[ -z "$MOCKITO_JAR" ]]; then
    MOCKITO_VER="${MOCKITO_VERSION:-5.13.0}"
    if got="$(fetch_mc mockito-core org/mockito mockito-core "$MOCKITO_VER" "mockito-core-${MOCKITO_VER}")"; then
        MOCKITO_JAR="$got"
    fi
fi
if [[ -z "$BB_JAR" ]]; then
    BB_VER="${BYTEBUDDY_VERSION:-1.14.19}"
    if got="$(fetch_mc byte-buddy net/bytebuddy byte-buddy "$BB_VER" "byte-buddy-${BB_VER}")"; then
        BB_JAR="$got"
    fi
fi
if [[ -z "$BBAG_JAR" ]]; then
    BB_VER="${BYTEBUDDY_VERSION:-1.14.19}"
    if got="$(fetch_mc byte-buddy-agent net/bytebuddy byte-buddy-agent "$BB_VER" "byte-buddy-agent-${BB_VER}")"; then
        BBAG_JAR="$got"
    fi
fi
if [[ -z "$OBJ_JAR" ]]; then
    OBJ_VER="${OBJENESIS_VERSION:-3.4}"
    if got="$(fetch_mc objenesis org/objenesis objenesis "$OBJ_VER" "objenesis-${OBJ_VER}")"; then
        OBJ_JAR="$got"
    fi
fi

# Tally and skip-if-missing.
missing=()
[[ -z "$MOCKITO_JAR" ]] && missing+=("mockito-core")
[[ -z "$BB_JAR"      ]] && missing+=("byte-buddy")
[[ -z "$BBAG_JAR"    ]] && missing+=("byte-buddy-agent")
[[ -z "$OBJ_JAR"     ]] && missing+=("objenesis")

if [[ ${#missing[@]} -gt 0 ]]; then
    echo "stage-mockito-probe: SKIP missing jars: ${missing[*]}" | tee -a "$COMPILE_LOG"
    echo "  searched: ~/.m2/repository, $hard_m2" | tee -a "$COMPILE_LOG"
    echo "  attempted: Maven Central (set NO_NET=1 to skip; \$MAVEN_CENTRAL_BASE to override mirror)" | tee -a "$COMPILE_LOG"
    echo "  set MOCKITO_JAR / BYTEBUDDY_JAR / BYTEBUDDY_AGENT_JAR / OBJENESIS_JAR to override" | tee -a "$COMPILE_LOG"
    echo "  install via: mvn dependency:get -Dartifact=org.mockito:mockito-core:5.13.0" | tee -a "$COMPILE_LOG"
    touch "$SKIP_FLAG"
    # Still continue: compile the SomeInterface (no Mockito on cp needed),
    # so a future agent can rerun staging without recompiling sources.
    if ! "$JAVAC" --release 21 -d "$CLASSES_DIR" "$SRC_DIR/SomeInterface.java" >> "$COMPILE_LOG" 2>&1; then
        echo "stage-mockito-probe: ERROR javac (SomeInterface) failed; see $COMPILE_LOG" >&2
        tail -n 40 "$COMPILE_LOG" >&2 || true
        exit 12
    fi
    echo "Main" > "$MAIN_CLASS_FILE"
    exit 0
fi

# Found all jars — copy + compile with Mockito on cp.
case "$(uname -s 2>/dev/null || echo Windows)" in
    MINGW*|MSYS*|CYGWIN*|Windows*) CPSEP=';'; IS_WINDOWS=1 ;;
    *) CPSEP=':'; IS_WINDOWS=0 ;;
esac

# Convert /c/foo to C:\foo for Windows javac, since MSYS auto-conversion
# breaks once we have semicolons inside the argument value.
to_native_path() {
    local p="$1"
    if [[ "$IS_WINDOWS" == "1" ]]; then
        if command -v cygpath >/dev/null 2>&1; then
            cygpath -w "$p"
        elif [[ "$p" =~ ^/([a-zA-Z])/(.*)$ ]]; then
            local drive="${BASH_REMATCH[1]}"
            local rest="${BASH_REMATCH[2]}"
            echo "${drive^^}:\\${rest//\//\\}"
        else
            echo "$p"
        fi
    else
        echo "$p"
    fi
}

cp -f "$MOCKITO_JAR" "$STAGED_DIR/mockito-core.jar"
cp -f "$BB_JAR"      "$STAGED_DIR/byte-buddy.jar"
cp -f "$BBAG_JAR"    "$STAGED_DIR/byte-buddy-agent.jar"
cp -f "$OBJ_JAR"     "$STAGED_DIR/objenesis.jar"

# Record absolute paths for the run script.
{
    echo "mockito-core=$MOCKITO_JAR"
    echo "byte-buddy=$BB_JAR"
    echo "byte-buddy-agent=$BBAG_JAR"
    echo "objenesis=$OBJ_JAR"
} > "$JARS_TXT"

echo "stage-mockito-probe: located jars" | tee -a "$COMPILE_LOG"
cat "$JARS_TXT" | sed 's/^/  /' | tee -a "$COMPILE_LOG"

CP="$(to_native_path "$STAGED_DIR/mockito-core.jar")${CPSEP}$(to_native_path "$STAGED_DIR/byte-buddy.jar")${CPSEP}$(to_native_path "$STAGED_DIR/byte-buddy-agent.jar")${CPSEP}$(to_native_path "$STAGED_DIR/objenesis.jar")"

SOURCES=(
    "$SRC_DIR/SomeInterface.java"
    "$SRC_DIR/Main.java"
)
echo "stage-mockito-probe: compiling ${#SOURCES[@]} sources with mockito on cp" | tee -a "$COMPILE_LOG"
echo "stage-mockito-probe: cp=$CP" >> "$COMPILE_LOG"

# Path-mangling fix: under MSYS / Git Bash, arguments containing
# semicolons (= the Windows classpath separator) are converted in unhelpful
# ways. Route compilation through an `@argfile` whose path we keep in
# Windows form so MSYS can't mangle it. Inside the argfile we use
# forward-slash paths (Windows javac accepts both `/` and `\\`), which
# avoids the @-file backslash-escape parser eating our path separators.
to_fwdslash() { echo "$1" | sed 's#\\#/#g'; }
ARGFILE_WIN="$(to_native_path "$STAGED_DIR/javac.args")"
ARGFILE_UNIX="$STAGED_DIR/javac.args"
CP_FS="$(to_fwdslash "$CP")"
CLASSES_FS="$(to_fwdslash "$(to_native_path "$CLASSES_DIR")")"
{
    echo "--release"
    echo "21"
    echo "-d"
    echo "\"$CLASSES_FS\""
    echo "-cp"
    echo "\"$CP_FS\""
    for s in "${SOURCES[@]}"; do
        printf '"%s"\n' "$(to_fwdslash "$(to_native_path "$s")")"
    done
} > "$ARGFILE_UNIX"

if ! MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' "$JAVAC" "@$ARGFILE_WIN" >> "$COMPILE_LOG" 2>&1; then
    echo "stage-mockito-probe: ERROR javac failed; see $COMPILE_LOG" >&2
    tail -n 40 "$COMPILE_LOG" >&2 || true
    exit 12
fi

echo "Main" > "$MAIN_CLASS_FILE"
echo "stage-mockito-probe: main class = $(cat "$MAIN_CLASS_FILE")" | tee -a "$COMPILE_LOG"
echo "stage-mockito-probe: staged classes at $CLASSES_DIR"        | tee -a "$COMPILE_LOG"
echo "stage-mockito-probe: OK"
exit 0
