#!/usr/bin/env bash
# bench/wildfly/run-under-rustjvm.sh
# WP0.5 — run the staged EJBCA minimum fixture under rust-jvm and capture
# stdout/stderr/rc deterministically. Compared against bench-baseline.json
# by diff-baseline.sh.
#
# Usage:
#   bash bench/wildfly/run-under-rustjvm.sh
# Env:
#   RUSTJVM_BIN  override path to rustjvm[.exe]; default target/release.
#   TIMEOUT_SEC  seconds before the run is killed (default 60).
#
# Artifacts:
#   bench/wildfly/last-run.stdout.log   full captured stdout
#   bench/wildfly/last-run.stderr.log   full captured stderr
#   bench/wildfly/last-run.rc           integer exit code
#   bench/wildfly/last-run.meta.json    timestamp + rustjvm path + main class
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
STAGED_DIR="$HERE/staged"
CLASSES_DIR="$STAGED_DIR/classes"
MAIN_CLASS_FILE="$STAGED_DIR/main-class.txt"

STDOUT_LOG="$HERE/last-run.stdout.log"
STDERR_LOG="$HERE/last-run.stderr.log"
RC_FILE="$HERE/last-run.rc"
META_FILE="$HERE/last-run.meta.json"

# ---------------------------------------------------------------------------
# Preconditions.
# ---------------------------------------------------------------------------
if [[ ! -d "$CLASSES_DIR" ]]; then
    echo "run-under-rustjvm: ERROR staged classes missing; run stage-ejbca-min.sh first" >&2
    exit 2
fi
if [[ ! -f "$MAIN_CLASS_FILE" ]]; then
    echo "run-under-rustjvm: ERROR main-class.txt missing; staging incomplete" >&2
    exit 2
fi
MAIN_CLASS="$(head -n 1 "$MAIN_CLASS_FILE" | tr -d '[:space:]')"
if [[ -z "$MAIN_CLASS" ]]; then
    echo "run-under-rustjvm: ERROR main-class.txt is empty" >&2
    exit 2
fi

# ---------------------------------------------------------------------------
# Locate the rust-jvm binary.
# ---------------------------------------------------------------------------
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

# ---------------------------------------------------------------------------
# Build classpath: classes dir + any jars (hamcrest/junit) dropped alongside.
# ---------------------------------------------------------------------------
case "$(uname -s 2>/dev/null || echo Windows)" in
    MINGW*|MSYS*|CYGWIN*|Windows*) CPSEP=';' ;;
    *) CPSEP=':' ;;
esac
CP_PARTS=("$CLASSES_DIR")
for jar in "$STAGED_DIR"/*.jar; do
    [[ -f "$jar" ]] && CP_PARTS+=("$jar")
done
# Join with classpath separator.
CP=""
for part in "${CP_PARTS[@]}"; do
    if [[ -z "$CP" ]]; then CP="$part"; else CP="${CP}${CPSEP}${part}"; fi
done

: > "$STDOUT_LOG"
: > "$STDERR_LOG"

TIMEOUT_SEC="${TIMEOUT_SEC:-60}"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo "run-under-rustjvm: binary=$RUSTJVM" >&2
echo "run-under-rustjvm: cp=$CP" >&2
echo "run-under-rustjvm: main=$MAIN_CLASS" >&2
echo "run-under-rustjvm: timeout=${TIMEOUT_SEC}s" >&2

set +e
if command -v timeout >/dev/null 2>&1; then
    timeout --kill-after=5 "${TIMEOUT_SEC}" \
        "$RUSTJVM" -c "$CP" "$MAIN_CLASS" \
        > "$STDOUT_LOG" 2> "$STDERR_LOG"
    RC=$?
else
    "$RUSTJVM" -c "$CP" "$MAIN_CLASS" \
        > "$STDOUT_LOG" 2> "$STDERR_LOG"
    RC=$?
fi
set -e

echo "$RC" > "$RC_FILE"

# Write a small metadata JSON for downstream tools / CI.
GIT_REV="unknown"
if command -v git >/dev/null 2>&1 && git -C "$REPO_ROOT" rev-parse HEAD >/dev/null 2>&1; then
    GIT_REV="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
fi
cat > "$META_FILE" <<JSON
{
  "generated_at": "$TS",
  "rustjvm_bin":  "$RUSTJVM",
  "rustjvm_rev":  "$GIT_REV",
  "main_class":   "$MAIN_CLASS",
  "classpath":    "$CP",
  "rc":           $RC,
  "timeout_sec":  $TIMEOUT_SEC
}
JSON

echo "run-under-rustjvm: rc=$RC" >&2
echo "run-under-rustjvm: stdout -> $STDOUT_LOG" >&2
echo "run-under-rustjvm: stderr -> $STDERR_LOG" >&2
exit 0
