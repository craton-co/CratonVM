#!/usr/bin/env bash
# bench/wildfly/diff-baseline.sh
# WP0.5 — compare the last rust-jvm run against bench-baseline.json.
# Exits 0 if the run matches (including expected failure mode), non-zero if
# anything drifted. Designed for CI nightly-regression detection.
#
# Checks, in order:
#   1. rc matches expected_final_rc
#   2. every expected_stderr_contains string is in last-run.stderr.log
#   3. every expected_stderr_forbidden string is ABSENT from stderr
#   4. every expected_stdout_contains string is in last-run.stdout.log
#
# Usage: bash bench/wildfly/diff-baseline.sh
# Optional: JQ=/path/to/jq  (uses jq if available for robust parsing).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASELINE="$HERE/bench-baseline.json"
STDOUT_LOG="$HERE/last-run.stdout.log"
STDERR_LOG="$HERE/last-run.stderr.log"
RC_FILE="$HERE/last-run.rc"

if [[ ! -f "$BASELINE" ]]; then
    echo "diff-baseline: ERROR $BASELINE missing" >&2
    exit 2
fi
for f in "$STDOUT_LOG" "$STDERR_LOG" "$RC_FILE"; do
    if [[ ! -f "$f" ]]; then
        echo "diff-baseline: ERROR $f missing; run run-under-rustjvm.sh first" >&2
        exit 2
    fi
done

ACTUAL_RC="$(head -n 1 "$RC_FILE" | tr -d '[:space:]')"

# --- parse baseline -------------------------------------------------------
# Prefer jq if available; fall back to a small POSIX shell/python parser.
json_extract() {
    # $1=key, operating on $BASELINE. Produces newline-separated array items
    # (for *_contains / *_forbidden) or a single scalar (for expected_final_rc).
    local key="$1"
    if command -v jq >/dev/null 2>&1; then
        if jq -e --arg k "$key" 'has($k) and (.[$k] | type == "array")' "$BASELINE" >/dev/null 2>&1; then
            jq -r --arg k "$key" '.[$k][]' "$BASELINE"
        else
            jq -r --arg k "$key" '.[$k]' "$BASELINE"
        fi
    elif command -v python3 >/dev/null 2>&1; then
        python3 - "$BASELINE" "$key" <<'PY'
import json, sys
with open(sys.argv[1], "r", encoding="utf-8") as f:
    data = json.load(f)
v = data.get(sys.argv[2])
if isinstance(v, list):
    for item in v:
        print(item)
else:
    print("" if v is None else v)
PY
    else
        # Desperate fallback: grep-based scalar extraction (won't handle arrays).
        echo "diff-baseline: WARN no jq/python3 — array fields cannot be parsed" >&2
        grep -Eo "\"$key\"[[:space:]]*:[[:space:]]*[0-9]+" "$BASELINE" | head -n1 | awk -F: '{gsub(/[^0-9]/,"",$2); print $2}'
    fi
}

EXPECTED_RC="$(json_extract expected_final_rc)"
STDERR_CONTAINS="$(json_extract expected_stderr_contains || true)"
STDERR_FORBIDDEN="$(json_extract expected_stderr_forbidden || true)"
STDOUT_CONTAINS="$(json_extract expected_stdout_contains || true)"

# --- compare --------------------------------------------------------------
FAILS=0
report_fail() { echo "diff-baseline: FAIL $*" >&2; FAILS=$((FAILS + 1)); }

if [[ "$ACTUAL_RC" != "$EXPECTED_RC" ]]; then
    report_fail "rc mismatch: got $ACTUAL_RC, baseline says $EXPECTED_RC"
fi

check_each() {
    local label="$1" log="$2" must_contain="$3" items="$4"
    [[ -z "$items" ]] && return 0
    while IFS= read -r item; do
        [[ -z "$item" ]] && continue
        if grep -Fq -- "$item" "$log"; then
            if [[ "$must_contain" == "no" ]]; then
                report_fail "$label unexpectedly contains '$item'"
            fi
        else
            if [[ "$must_contain" == "yes" ]]; then
                report_fail "$label missing required signal '$item'"
            fi
        fi
    done <<< "$items"
}

check_each "stderr" "$STDERR_LOG" yes "$STDERR_CONTAINS"
check_each "stderr" "$STDERR_LOG" no  "$STDERR_FORBIDDEN"
check_each "stdout" "$STDOUT_LOG" yes "$STDOUT_CONTAINS"

if (( FAILS == 0 )); then
    echo "diff-baseline: baseline match (rc=$ACTUAL_RC, $(wc -l < "$STDERR_LOG") stderr lines, $(wc -l < "$STDOUT_LOG") stdout lines)"
    exit 0
fi
echo "diff-baseline: baseline DRIFT ($FAILS check(s) failed) — update bench-baseline.json if this is an intentional fix, or investigate as a regression"
exit 1
