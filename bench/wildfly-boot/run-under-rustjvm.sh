#!/usr/bin/env bash
# bench/wildfly-boot/run-under-rustjvm.sh
# WP8.10.2 — boot a real WildFly 32 distribution under rust-jvm.
# Mirrors the JVM invocation at the bottom of WildFly's standalone.sh, but
# substitutes target/release/rustjvm.exe for `java`.
#
# Argv layout (translated from standalone.sh):
#   $RUSTJVM \
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
#   * `-mp` is parsed by JBoss Modules' Main.main, NOT by rust-jvm — so it
#     appears AFTER `--jar` (as a program argument). rust-jvm's
#     `--module-path` would attempt JPMS resolution, which is wrong here.
#   * `-Djboss.modules.system.pkgs` is normally also delegated, but harmless
#     to forward as a system property; JBoss Modules reads it via
#     System.getProperty.
#   * No `-Xmx` set explicitly — WildFly tolerates default heap; bump if
#     OutOfMemoryError shows up in last-run.stderr.log.
#
# Env:
#   RUSTJVM_BIN   override binary path
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
    echo "run-under-rustjvm: ERROR WildFly not staged; run stage.sh first" >&2
    echo "                  expected at $WILDFLY_HOME/jboss-modules.jar" >&2
    exit 2
fi

if [[ -n "${RUSTJVM_BIN:-}" && -x "$RUSTJVM_BIN" ]]; then
    RUSTJVM="$RUSTJVM_BIN"
elif [[ -x "$REPO_ROOT/target/release/rustjvm.exe" ]]; then
    RUSTJVM="$REPO_ROOT/target/release/rustjvm.exe"
elif [[ -x "$REPO_ROOT/target/release/rustjvm" ]]; then
    RUSTJVM="$REPO_ROOT/target/release/rustjvm"
else
    echo "run-under-rustjvm: ERROR rustjvm binary not found; build with 'cargo build --release -p rustjvm-cli'" >&2
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

echo "run-under-rustjvm: binary=$RUSTJVM"        >&2
echo "run-under-rustjvm: WILDFLY_HOME=$WFH_NATIVE" >&2
echo "run-under-rustjvm: jar=$JBOSS_MODULES_JAR"   >&2
echo "run-under-rustjvm: timeout=${TIMEOUT_SEC}s"  >&2
echo "run-under-rustjvm: server-config=$SERVER_CFG" >&2

: > "$STDOUT_LOG"
: > "$STDERR_LOG"

set +e
if command -v timeout >/dev/null 2>&1; then
    timeout --kill-after=5 "${TIMEOUT_SEC}" \
        "$RUSTJVM" "${SYSPROPS[@]}" --jar "$JBOSS_MODULES_JAR" "${JBOSS_MODULES_ARGS[@]}" \
        > "$STDOUT_LOG" 2> "$STDERR_LOG"
    RC=$?
else
    "$RUSTJVM" "${SYSPROPS[@]}" --jar "$JBOSS_MODULES_JAR" "${JBOSS_MODULES_ARGS[@]}" \
        > "$STDOUT_LOG" 2> "$STDERR_LOG"
    RC=$?
fi
set -e

echo "$RC" > "$RC_FILE"

TAIL_N="${STDERR_TAIL_LINES:-100}"
if [[ -s "$STDERR_LOG" ]]; then
    echo "run-under-rustjvm: --- last $TAIL_N stderr lines ---" >&2
    tail -n "$TAIL_N" "$STDERR_LOG" >&2 || true
    echo "run-under-rustjvm: --- end stderr tail ---" >&2
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
  "rustjvm_bin":    "$RUSTJVM",
  "rustjvm_rev":    "$GIT_REV",
  "wildfly_home":   "$WFH_NATIVE",
  "jboss_modules":  "$JBOSS_MODULES_JAR",
  "server_config":  "$SERVER_CFG",
  "argv":           $ARGV_JSON,
  "rc":             $RC,
  "timeout_sec":    $TIMEOUT_SEC
}
JSON

echo "run-under-rustjvm: rc=$RC"                       >&2
echo "run-under-rustjvm: stdout -> $STDOUT_LOG"        >&2
echo "run-under-rustjvm: stderr -> $STDERR_LOG"        >&2
echo "run-under-rustjvm: meta   -> $META_FILE"         >&2
exit 0
