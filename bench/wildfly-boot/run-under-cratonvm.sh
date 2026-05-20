#!/usr/bin/env bash
# bench/wildfly-boot/run-under-cratonvm.sh
# WP8.10.2 — boot a real WildFly 32 distribution under cratonvm.
# Mirrors the JVM invocation at the bottom of WildFly's standalone.sh, but
# substitutes target/release/cratonvm.exe for `java`.
#
# Argv layout (translated from standalone.sh):
#   $CRATONVM \
#       -Dprogram.name=standalone.sh \
#       -Djboss.home.dir=$WILDFLY_HOME \
#       -Dorg.jboss.boot.log.file=$WILDFLY_HOME/standalone/log/server.log \
#       -Dlogging.configuration=file:$WILDFLY_HOME/standalone/configuration/logging.properties \
#       -Dorg.jboss.modules.system.pkgs=org.jboss.byteman \
#       --jar $WILDFLY_HOME/jboss-modules.jar \
#       -mp $WILDFLY_HOME/modules \
#       org.jboss.as.standalone \
#       --server-config=standalone.xml
#
# Notes on the shape:
#   * `-mp` is parsed by JBoss Modules' Main.main, NOT by cratonvm — so it
#     appears AFTER `--jar` (as a program argument). cratonvm's
#     `--module-path` would attempt JPMS resolution, which is wrong here.
#   * `-Djboss.modules.system.pkgs` is normally also delegated, but harmless
#     to forward as a system property; JBoss Modules reads it via
#     System.getProperty.
#   * No `-Xmx` set explicitly — WildFly tolerates default heap; bump if
#     OutOfMemoryError shows up in last-run.stderr.log.
#
# Env:
#   CRATONVM_BIN   override binary path
#   TIMEOUT_SEC   seconds before kill (default 60)
#   SERVER_CONFIG override standalone xml file (default standalone.xml)
#   STDERR_TAIL_LINES  trailing stderr lines to mirror to console (default 100)

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
STAGED_DIR="$HERE/staged"
WILDFLY_HOME="$STAGED_DIR/wildfly"

STDOUT_LOG="$HERE/last-run.stdout.log"
STDERR_LOG="$HERE/last-run.stderr.log"
RC_FILE="$HERE/last-run.rc"
META_FILE="$HERE/last-run.meta.json"

if [[ ! -f "$WILDFLY_HOME/jboss-modules.jar" ]]; then
    echo "run-under-cratonvm: ERROR WildFly not staged; run stage.sh first" >&2
    echo "                  expected at $WILDFLY_HOME/jboss-modules.jar" >&2
    exit 2
fi

if [[ -n "${CRATONVM_BIN:-}" && -x "$CRATONVM_BIN" ]]; then
    CRATONVM="$CRATONVM_BIN"
elif [[ -x "$REPO_ROOT/target/release/cratonvm.exe" ]]; then
    CRATONVM="$REPO_ROOT/target/release/cratonvm.exe"
elif [[ -x "$REPO_ROOT/target/release/cratonvm" ]]; then
    CRATONVM="$REPO_ROOT/target/release/cratonvm"
else
    echo "run-under-cratonvm: ERROR cratonvm binary not found; build with 'cargo build --release -p cratonvm-cli'" >&2
    exit 3
fi

# Convert /c/Foo/Bar (MSYS) -> C:/Foo/Bar (forward-slash Windows path).
to_native_path() {
    local p="$1"
    case "$(uname -s 2>/dev/null || echo Windows)" in
        MINGW*|MSYS*|CYGWIN*|Windows*)
            if command -v cygpath >/dev/null 2>&1; then
                cygpath -m "$p"
            elif [[ "$p" =~ ^/([a-zA-Z])/(.*)$ ]]; then
                echo "${BASH_REMATCH[1]}:/${BASH_REMATCH[2]}"
            else
                echo "$p"
            fi
            ;;
        *) echo "$p" ;;
    esac
}

WFH_NATIVE="$(to_native_path "$WILDFLY_HOME")"
JBOSS_MODULES_JAR="$WFH_NATIVE/jboss-modules.jar"
MODULES_DIR="$WFH_NATIVE/modules"
LOG_FILE="$WFH_NATIVE/standalone/log/server.log"
LOGGING_CFG="file:$WFH_NATIVE/standalone/configuration/logging.properties"
SERVER_CFG="${SERVER_CONFIG:-standalone.xml}"

mkdir -p "$WFH_NATIVE/standalone/log" 2>/dev/null || true

TIMEOUT_SEC="${TIMEOUT_SEC:-60}"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# UNVERIFIED: WildFly 32 may also need additional system properties.
# These match what `standalone.sh` sets in the upstream tree.
SYSPROPS=(
    "-Dprogram.name=standalone.sh"
    "-Djboss.home.dir=$WFH_NATIVE"
    "-Dorg.jboss.boot.log.file=$LOG_FILE"
    "-Dlogging.configuration=$LOGGING_CFG"
    "-Djava.util.logging.manager=org.jboss.logmanager.LogManager"
    "-Djboss.modules.system.pkgs=org.jboss.byteman"
    "-Dfile.encoding=UTF-8"
)

JBOSS_MODULES_ARGS=(
    "-mp" "$MODULES_DIR"
    "org.jboss.as.standalone"
    "--server-config=$SERVER_CFG"
)

echo "run-under-cratonvm: binary=$CRATONVM"        >&2
echo "run-under-cratonvm: WILDFLY_HOME=$WFH_NATIVE" >&2
echo "run-under-cratonvm: jar=$JBOSS_MODULES_JAR"   >&2
echo "run-under-cratonvm: timeout=${TIMEOUT_SEC}s"  >&2
echo "run-under-cratonvm: server-config=$SERVER_CFG" >&2

: > "$STDOUT_LOG"
: > "$STDERR_LOG"

set +e
if command -v timeout >/dev/null 2>&1; then
    timeout --kill-after=5 "${TIMEOUT_SEC}" \
        "$CRATONVM" "${SYSPROPS[@]}" --jar "$JBOSS_MODULES_JAR" "${JBOSS_MODULES_ARGS[@]}" \
        > "$STDOUT_LOG" 2> "$STDERR_LOG"
    RC=$?
else
    "$CRATONVM" "${SYSPROPS[@]}" --jar "$JBOSS_MODULES_JAR" "${JBOSS_MODULES_ARGS[@]}" \
        > "$STDOUT_LOG" 2> "$STDERR_LOG"
    RC=$?
fi
set -e

echo "$RC" > "$RC_FILE"

TAIL_N="${STDERR_TAIL_LINES:-100}"
if [[ -s "$STDERR_LOG" ]]; then
    echo "run-under-cratonvm: --- last $TAIL_N stderr lines ---" >&2
    tail -n "$TAIL_N" "$STDERR_LOG" >&2 || true
    echo "run-under-cratonvm: --- end stderr tail ---" >&2
fi

GIT_REV="unknown"
if command -v git >/dev/null 2>&1 && git -C "$REPO_ROOT" rev-parse HEAD >/dev/null 2>&1; then
    GIT_REV="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
fi

ARGV_JSON="["
ARGV_JSON+=$(printf '"%s",' "${SYSPROPS[@]}")
ARGV_JSON+="\"--jar\",\"$JBOSS_MODULES_JAR\","
ARGV_JSON+=$(printf '"%s",' "${JBOSS_MODULES_ARGS[@]}")
ARGV_JSON="${ARGV_JSON%,}]"

cat > "$META_FILE" <<JSON
{
  "generated_at":   "$TS",
  "cratonvm_bin":    "$CRATONVM",
  "cratonvm_rev":    "$GIT_REV",
  "wildfly_home":   "$WFH_NATIVE",
  "jboss_modules":  "$JBOSS_MODULES_JAR",
  "server_config":  "$SERVER_CFG",
  "argv":           $ARGV_JSON,
  "rc":             $RC,
  "timeout_sec":    $TIMEOUT_SEC
}
JSON

echo "run-under-cratonvm: rc=$RC"                       >&2
echo "run-under-cratonvm: stdout -> $STDOUT_LOG"        >&2
echo "run-under-cratonvm: stderr -> $STDERR_LOG"        >&2
echo "run-under-cratonvm: meta   -> $META_FILE"         >&2
exit 0
