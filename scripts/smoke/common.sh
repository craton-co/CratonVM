#!/usr/bin/env bash
# Shared helpers for the Phase-I smoke runners (RI.1 .. RI.17).
#
# Every runner source-includes this file as its first action, which:
#   * validates required env vars ($CRATONVM, $FIXTURE_CACHE, $JAVA_HOME*),
#   * exposes `smoke_download`, `smoke_run_cratonvm`, and
#     `smoke_require_signal` so each runner stays ≤ 30 LoC of real logic,
#   * installs `set -euo pipefail` so missing pieces fail loudly.

set -euo pipefail

SMOKE_DIR_ME="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SMOKE_REPO_ROOT="$(cd "$SMOKE_DIR_ME/../.." && pwd)"

if [[ -z "${CRATONVM:-}" ]]; then
    CRATONVM="$SMOKE_REPO_ROOT/target/release/cratonvm"
fi
if [[ ! -x "$CRATONVM" ]]; then
    echo "smoke/common: cratonvm binary not found at $CRATONVM" >&2
    exit 2
fi

if [[ -z "${FIXTURE_CACHE:-}" ]]; then
    FIXTURE_CACHE="$SMOKE_REPO_ROOT/.smoke-cache/default"
fi
mkdir -p "$FIXTURE_CACHE"

# JAVA_HOME_FOR_SMOKE overrides JAVA_HOME; defaults to whatever is set by
# actions/setup-java or the caller's shell.
JAVA_HOME_FOR_SMOKE="${JAVA_HOME_FOR_SMOKE:-${JAVA_HOME:-}}"
if [[ -z "$JAVA_HOME_FOR_SMOKE" ]]; then
    echo "smoke/common: JAVA_HOME is not set; cannot run cratonvm" >&2
    exit 2
fi

# Curl-with-retry: guarantees deterministic downloads even on flaky CI.
smoke_download() {
    local url="$1"
    local dest="$2"
    if [[ -s "$dest" ]]; then
        echo "smoke/common: cached $dest"
        return 0
    fi
    mkdir -p "$(dirname "$dest")"
    local tries=0
    while (( tries < 5 )); do
        if curl --fail --location --silent --show-error \
                --retry 3 --retry-delay 2 \
                --output "$dest" "$url"; then
            return 0
        fi
        tries=$((tries + 1))
        sleep $((tries * 2))
    done
    echo "smoke/common: failed to download $url after 5 tries" >&2
    return 1
}

# Run cratonvm with the workload and tee stdout+stderr into $SMOKE_LOG.
# Extra args: passed through as JVM+program arguments.
smoke_run_cratonvm() {
    SMOKE_LOG="$(mktemp)"
    if [[ -z "${SMOKE_TIMEOUT:-}" ]]; then
        SMOKE_TIMEOUT=600
    fi
    if command -v timeout >/dev/null 2>&1; then
        timeout "${SMOKE_TIMEOUT}" "$CRATONVM" \
            --java-home "$JAVA_HOME_FOR_SMOKE" \
            "$@" 2>&1 | tee "$SMOKE_LOG" || true
    else
        "$CRATONVM" \
            --java-home "$JAVA_HOME_FOR_SMOKE" \
            "$@" 2>&1 | tee "$SMOKE_LOG" || true
    fi
    export SMOKE_LOG
}

# Assert that a specific signal appears in the last cratonvm run's output.
# Exits 0 on match, 1 otherwise, printing the last 30 lines on failure.
smoke_require_signal() {
    local pattern="$1"
    if grep -q -E -- "$pattern" "$SMOKE_LOG"; then
        echo "smoke/common: PASS — saw '$pattern'"
        return 0
    fi
    echo "smoke/common: FAIL — expected '$pattern' not found in output" >&2
    echo "---- last 30 lines of output ----" >&2
    tail -n 30 "$SMOKE_LOG" >&2
    return 1
}

# Assert that a specific string does NOT appear — used for "no crash" tests.
smoke_forbid_signal() {
    local pattern="$1"
    if grep -q -E -- "$pattern" "$SMOKE_LOG"; then
        echo "smoke/common: FAIL — saw forbidden '$pattern'" >&2
        grep -E -- "$pattern" "$SMOKE_LOG" | head -5 >&2
        return 1
    fi
    echo "smoke/common: PASS — forbidden '$pattern' absent"
    return 0
}
