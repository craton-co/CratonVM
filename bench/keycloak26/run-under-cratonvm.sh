#!/usr/bin/env bash
# bench/keycloak26/run-under-cratonvm.sh
# WP8.7 — run the staged keycloak26 fixture under cratonvm and capture
# stdout/stderr/rc deterministically. Compared against bench-baseline.json
# by diff-baseline.sh.
#
# Usage: bash bench/keycloak26/run-under-cratonvm.sh
# Env:
#   CRATONVM_BIN   override path to cratonvm[.exe]; default target/release.
#   TIMEOUT_SEC   seconds before kill (default 600 — 10 minutes).
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

if [[ ! -d "$CLASSES_DIR" ]]; then
    echo "run-keycloak26: ERROR staged classes missing; run stage.sh first" >&2
    exit 2
fi
if [[ ! -f "$MAIN_CLASS_FILE" ]]; then
    echo "run-keycloak26: ERROR main-class.txt missing; staging incomplete" >&2
    exit 2
fi
MAIN_CLASS="$(head -n 1 "$MAIN_CLASS_FILE" | tr -d '[:space:]')"
if [[ -z "$MAIN_CLASS" ]]; then
    echo "run-keycloak26: ERROR main-class.txt is empty" >&2
    exit 2
fi

if [[ -n "${CRATONVM_BIN:-}" && -x "$CRATONVM_BIN" ]]; then
    CRATONVM="$CRATONVM_BIN"
elif [[ -x "$REPO_ROOT/target/release/cratonvm.exe" ]]; then
    CRATONVM="$REPO_ROOT/target/release/cratonvm.exe"
elif [[ -x "$REPO_ROOT/target/release/cratonvm" ]]; then
    CRATONVM="$REPO_ROOT/target/release/cratonvm"
else
    echo "run-keycloak26: ERROR cratonvm binary not found; build with 'cargo build --release -p cratonvm-cli'" >&2
    exit 3
fi

case "$(uname -s 2>/dev/null || echo Windows)" in
    MINGW*|MSYS*|CYGWIN*|Windows*) CPSEP=';' ;;
    *) CPSEP=':' ;;
esac
CP_PARTS=("$CLASSES_DIR")
for jar in "$STAGED_DIR"/*.jar; do
    [[ -f "$jar" ]] && CP_PARTS+=("$jar")
done
CP=""
for part in "${CP_PARTS[@]}"; do
    if [[ -z "$CP" ]]; then CP="$part"; else CP="${CP}${CPSEP}${part}"; fi
done

: > "$STDOUT_LOG"
: > "$STDERR_LOG"

TIMEOUT_SEC="${TIMEOUT_SEC:-600}"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo "run-keycloak26: binary=$CRATONVM" >&2
echo "run-keycloak26: cp=$CP" >&2
echo "run-keycloak26: main=$MAIN_CLASS" >&2
echo "run-keycloak26: timeout=${TIMEOUT_SEC}s" >&2

set +e
if command -v timeout >/dev/null 2>&1; then
    timeout --kill-after=5 "${TIMEOUT_SEC}" \
        "$CRATONVM" -c "$CP" "$MAIN_CLASS" \
        > "$STDOUT_LOG" 2> "$STDERR_LOG"
    RC=$?
else
    "$CRATONVM" -c "$CP" "$MAIN_CLASS" \
        > "$STDOUT_LOG" 2> "$STDERR_LOG"
    RC=$?
fi
set -e

echo "$RC" > "$RC_FILE"

GIT_REV="unknown"
if command -v git >/dev/null 2>&1 && git -C "$REPO_ROOT" rev-parse HEAD >/dev/null 2>&1; then
    GIT_REV="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
fi
cat > "$META_FILE" <<JSON
{
  "generated_at": "$TS",
  "cratonvm_bin":  "$CRATONVM",
  "cratonvm_rev":  "$GIT_REV",
  "main_class":   "$MAIN_CLASS",
  "classpath":    "$CP",
  "rc":           $RC,
  "timeout_sec":  $TIMEOUT_SEC
}
JSON

echo "run-keycloak26: rc=$RC" >&2
echo "run-keycloak26: stdout -> $STDOUT_LOG" >&2
echo "run-keycloak26: stderr -> $STDERR_LOG" >&2
exit 0
