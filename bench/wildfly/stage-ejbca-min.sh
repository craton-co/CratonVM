#!/usr/bin/env bash
# bench/wildfly/stage-ejbca-min.sh
# WP0.5 — stage the minimum EJBCA cesecore-common DirectRunner subset that
# was exercised on 2026-04-24 (session 93). See docs/wildfly-ejbca-roadmap.md
# WP0.5 and memory/finding_println_regression.md.
#
# Behaviour:
#   * If C:/craton/ejbca-test-run/ is present on this machine, we stage from
#     there — compiling AccessMatchType.java + DirectRunner.java with
#     hamcrest.jar + junit.jar on the classpath, then copying the resulting
#     .class files into bench/wildfly/staged/classes/.
#   * If that dir is absent (e.g. on CI or a clean dev machine), we fall back
#     to the placeholder fixture at apps/ejbca_min_fixture/src/Main.java so
#     the harness still produces a reproducible failure shape.
#
# Exit codes:
#   0   staging succeeded (real or placeholder)
#   10  javac missing
#   11  neither EJBCA subset nor placeholder found
#   12  compile failure (report captured in staged/compile.log)
#
# Usage: bash bench/wildfly/stage-ejbca-min.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
STAGED_DIR="$HERE/staged"
CLASSES_DIR="$STAGED_DIR/classes"
COMPILE_LOG="$STAGED_DIR/compile.log"
MAIN_CLASS_FILE="$STAGED_DIR/main-class.txt"

mkdir -p "$CLASSES_DIR"
: > "$COMPILE_LOG"

# ---------------------------------------------------------------------------
# Locate javac. Respect JAVA_HOME first, then PATH.
# ---------------------------------------------------------------------------
if [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/javac" ]]; then
    JAVAC="$JAVA_HOME/bin/javac"
elif [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/javac.exe" ]]; then
    JAVAC="$JAVA_HOME/bin/javac.exe"
elif command -v javac >/dev/null 2>&1; then
    JAVAC="$(command -v javac)"
else
    echo "stage-ejbca-min: ERROR javac not found (JAVA_HOME unset and javac not on PATH)" >&2
    exit 10
fi
echo "stage-ejbca-min: using javac at $JAVAC" | tee -a "$COMPILE_LOG"

# ---------------------------------------------------------------------------
# Decide source set.
# ---------------------------------------------------------------------------
EJBCA_DIR="${EJBCA_TEST_RUN_DIR:-C:/craton/ejbca-test-run}"
# Convert Windows path to something bash likes; /c/... works on Git Bash/MSYS.
if [[ "$EJBCA_DIR" =~ ^[A-Za-z]: ]]; then
    EJBCA_DIR_BASH="/$(echo "${EJBCA_DIR:0:1}" | tr '[:upper:]' '[:lower:]')/${EJBCA_DIR:3}"
else
    EJBCA_DIR_BASH="$EJBCA_DIR"
fi

MODE=""
if [[ -d "$EJBCA_DIR_BASH/src/org/cesecore/authorization/user" ]]; then
    MODE="real"
elif [[ -d "$EJBCA_DIR/src/org/cesecore/authorization/user" ]]; then
    MODE="real"
    EJBCA_DIR_BASH="$EJBCA_DIR"
elif [[ -f "$REPO_ROOT/apps/ejbca_min_fixture/src/Main.java" ]]; then
    MODE="placeholder"
else
    echo "stage-ejbca-min: ERROR no EJBCA source tree and no placeholder fixture found" >&2
    echo "  looked in: $EJBCA_DIR (Windows) / $EJBCA_DIR_BASH (bash)" >&2
    echo "  placeholder: $REPO_ROOT/apps/ejbca_min_fixture/src/Main.java (absent)" >&2
    exit 11
fi

echo "stage-ejbca-min: mode=$MODE" | tee -a "$COMPILE_LOG"

# ---------------------------------------------------------------------------
# Stage.
# ---------------------------------------------------------------------------
if [[ "$MODE" == "real" ]]; then
    SRC_DIR="$EJBCA_DIR_BASH/src"
    LIB_DIR="$EJBCA_DIR_BASH"   # ejbca-test-run puts junit.jar / hamcrest.jar at root
    # Classpath separator: ; on Windows shells, : on Unix.
    case "$(uname -s 2>/dev/null || echo Windows)" in
        MINGW*|MSYS*|CYGWIN*|Windows*) CPSEP=';' ;;
        *) CPSEP=':' ;;
    esac
    CP="$LIB_DIR/junit.jar${CPSEP}$LIB_DIR/hamcrest.jar"

    # Compile the AccessMatchType family + any Runner classes found.
    SOURCES=(
        "$SRC_DIR/org/cesecore/authorization/user/AccessMatchType.java"
    )
    # DirectRunner.java is the canonical entrypoint exercised today.
    if [[ -f "$SRC_DIR/DirectRunner.java" ]]; then
        SOURCES+=("$SRC_DIR/DirectRunner.java")
        MAIN_CLASS="DirectRunner"
    elif [[ -f "$SRC_DIR/Runner.java" ]]; then
        SOURCES+=("$SRC_DIR/Runner.java")
        MAIN_CLASS="Runner"
    else
        MAIN_CLASS="org.cesecore.authorization.user.AccessMatchType"
    fi

    echo "stage-ejbca-min: compiling ${#SOURCES[@]} sources with cp=$CP" | tee -a "$COMPILE_LOG"
    if ! "$JAVAC" --release 21 -cp "$CP" -d "$CLASSES_DIR" "${SOURCES[@]}" >> "$COMPILE_LOG" 2>&1; then
        echo "stage-ejbca-min: ERROR javac failed; see $COMPILE_LOG" >&2
        tail -n 40 "$COMPILE_LOG" >&2 || true
        exit 12
    fi
    # Also copy the pre-compiled hamcrest/junit jars next to the classes so
    # rustjvm can pick them up via -c <classes>;<jars>.
    cp -f "$LIB_DIR/junit.jar"    "$STAGED_DIR/junit.jar"    2>/dev/null || true
    cp -f "$LIB_DIR/hamcrest.jar" "$STAGED_DIR/hamcrest.jar" 2>/dev/null || true

elif [[ "$MODE" == "placeholder" ]]; then
    SRC_DIR="$REPO_ROOT/apps/ejbca_min_fixture/src"
    echo "stage-ejbca-min: compiling placeholder Main.java from $SRC_DIR" | tee -a "$COMPILE_LOG"
    if ! "$JAVAC" --release 21 -d "$CLASSES_DIR" "$SRC_DIR/Main.java" >> "$COMPILE_LOG" 2>&1; then
        echo "stage-ejbca-min: ERROR javac failed; see $COMPILE_LOG" >&2
        tail -n 40 "$COMPILE_LOG" >&2 || true
        exit 12
    fi
    MAIN_CLASS="Main"
fi

echo "$MAIN_CLASS" > "$MAIN_CLASS_FILE"
echo "stage-ejbca-min: main class = $MAIN_CLASS" | tee -a "$COMPILE_LOG"
echo "stage-ejbca-min: staged classes at $CLASSES_DIR" | tee -a "$COMPILE_LOG"
echo "stage-ejbca-min: OK"
exit 0
