#!/usr/bin/env bash
# Generic single-class/jar runner for anything under apps/.
#
# This is the common "build a classpath, run one main class or jar, capture
# the result" operation that every apps/*-suite-runner/*.sh script duplicates
# in its own bespoke way. It does NOT replace those scripts: framework-
# specific test discovery, sharding, and categorization (e.g. H2's
# TestBase.testFromMain() convention, Tomcat's shard layout) stay there. Use
# this for a one-off "does this class run" / "cratonvm vs hotspot" check.
set -u
set -o pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/run-app.sh <app-dir> <main-class-or-jar> [options] [-- java-args...]

  <app-dir>              Directory under apps/ (e.g. apps/h2database)
  <main-class-or-jar>    Fully-qualified main class, or a .jar file

Options:
  --cp-file FILE         Classpath list file (one entry per line, or a single
                         platform-separated line). Default: <app-dir>/craton-testcp.txt
                         if present, else target/classes + target/test-classes +
                         every jar under <app-dir>/lib.
  --heap SIZE            -Xmx value passed to the VM. Default: 1g
  --timeout SECS         Kill the run after SECS seconds. Default: 300
  --vm cratonvm|hotspot  Which runtime to invoke. Default: cratonvm
  --out DIR              Result directory. Default: <app-dir>/out
  -h, --help             Show this help

Environment:
  CRATONVM_BIN   Path to the cratonvm binary. Default: target/release/cratonvm
  JAVA_HOME      JDK root used for --vm hotspot

Examples:
  scripts/run-app.sh apps/h2database org.h2.test.TestAll
  scripts/run-app.sh apps/netty io.netty.SomeTest --heap 2g --vm hotspot
  scripts/run-app.sh apps/spring-boot-suite-runner app.jar -- -Dfoo=bar
EOF
}

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"

case "${1:-}" in -h|--help) usage; exit 0 ;; esac
if [ $# -lt 2 ]; then usage; exit 1; fi

APP_DIR="$1"; shift
TARGET="$1"; shift

CP_FILE=""
HEAP="1g"
TIMEOUT="300"
VM_KIND="cratonvm"
OUT_DIR=""
EXTRA_ARGS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --cp-file) CP_FILE="$2"; shift 2 ;;
    --heap) HEAP="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --vm) VM_KIND="$2"; shift 2 ;;
    --out) OUT_DIR="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    --) shift; EXTRA_ARGS+=("$@"); break ;;
    *) EXTRA_ARGS+=("$1"); shift ;;
  esac
done

[ -d "$APP_DIR" ] || { echo "ERROR: app dir not found: $APP_DIR" >&2; exit 1; }
APP_DIR="$(cd "$APP_DIR" && pwd)"
OUT_DIR="${OUT_DIR:-$APP_DIR/out}"
mkdir -p "$OUT_DIR"

# --- classpath ---------------------------------------------------------
build_classpath() {
  if [ -n "$CP_FILE" ]; then
    [ -f "$CP_FILE" ] || { echo "ERROR: --cp-file not found: $CP_FILE" >&2; exit 1; }
    tr -d '\r\n' < "$CP_FILE"
    return
  fi
  if [ -f "$APP_DIR/craton-testcp.txt" ]; then
    tr -d '\r\n' < "$APP_DIR/craton-testcp.txt"
    return
  fi
  local sep=":"
  case "$(uname -s 2>/dev/null)" in MINGW*|MSYS*|CYGWIN*) sep=";" ;; esac
  local parts=()
  [ -d "$APP_DIR/target/classes" ] && parts+=("$APP_DIR/target/classes")
  [ -d "$APP_DIR/target/test-classes" ] && parts+=("$APP_DIR/target/test-classes")
  if [ -d "$APP_DIR/lib" ]; then
    while IFS= read -r jar; do parts+=("$jar"); done < <(find "$APP_DIR/lib" -name '*.jar')
  fi
  [ "${#parts[@]}" -gt 0 ] || {
    echo "ERROR: no classpath found; pass --cp-file or add $APP_DIR/craton-testcp.txt" >&2
    exit 1
  }
  local IFS="$sep"
  echo "${parts[*]}"
}

CLASSPATH="$(build_classpath)"

# --- runtime -------------------------------------------------------------
find_cratonvm() {
  local c
  for c in "${CRATONVM_BIN:-}" "$REPO_ROOT/target/release/cratonvm" "$REPO_ROOT/target/release/cratonvm.exe"; do
    [ -n "$c" ] && [ -x "$c" ] && { echo "$c"; return 0; }
  done
  return 1
}

case "$VM_KIND" in
  cratonvm)
    VM_BIN="$(find_cratonvm)" || {
      echo "ERROR: cratonvm binary not found (set CRATONVM_BIN or build target/release/cratonvm)" >&2
      exit 1
    }
    ;;
  hotspot)
    [ -n "${JAVA_HOME:-}" ] || { echo "ERROR: --vm hotspot needs JAVA_HOME" >&2; exit 1; }
    VM_BIN="$JAVA_HOME/bin/java"
    [ -x "$VM_BIN" ] || VM_BIN="$JAVA_HOME/bin/java.exe"
    ;;
  *)
    echo "ERROR: --vm must be cratonvm or hotspot, got: $VM_KIND" >&2
    exit 1
    ;;
esac

# --- invoke ----------------------------------------------------------------
RUN_ARGS=(-Xmx"$HEAP" -cp "$CLASSPATH")
case "$TARGET" in
  *.jar) RUN_ARGS+=(--jar "$TARGET") ;;
  *) RUN_ARGS+=("$TARGET") ;;
esac
RUN_ARGS+=("${EXTRA_ARGS[@]}")

STAMP="$(date +%Y%m%d-%H%M%S)"
SAFE_TARGET="$(printf '%s' "$TARGET" | sed 's/[^A-Za-z0-9_.-]/_/g')"
OUT_STDOUT="$OUT_DIR/$SAFE_TARGET.$VM_KIND.$STAMP.out"
OUT_STDERR="$OUT_DIR/$SAFE_TARGET.$VM_KIND.$STAMP.err"

echo "[run-app] $VM_BIN ${RUN_ARGS[*]}" >&2
START_MS=$(date +%s%3N)
if command -v timeout >/dev/null 2>&1; then
  timeout "${TIMEOUT}s" "$VM_BIN" "${RUN_ARGS[@]}" >"$OUT_STDOUT" 2>"$OUT_STDERR"
else
  "$VM_BIN" "${RUN_ARGS[@]}" >"$OUT_STDOUT" 2>"$OUT_STDERR"
fi
EXIT_CODE=$?
END_MS=$(date +%s%3N)

echo "exit=$EXIT_CODE elapsed_ms=$((END_MS - START_MS)) stdout=$OUT_STDOUT stderr=$OUT_STDERR"
exit "$EXIT_CODE"
