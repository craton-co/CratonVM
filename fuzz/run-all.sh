#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Run every libFuzzer target in this crate for a bounded time and report
# which ones produced crash artifacts.
#
# The target list is taken from `cargo fuzz list`, so a target added to
# `Cargo.toml` is picked up automatically — there is no second list to keep
# in sync.
#
# Usage:
#   ./run-all.sh                     # 3600s per target (the report's tier)
#   ./run-all.sh 300                 # 300s per target (smoke)
#   ./run-all.sh 300 fuzz_zip_entry fuzz_signed_jar   # only these targets
#
# Environment overrides:
#   FUZZ_TIMEOUT   per-input timeout in seconds        (default 10)
#   FUZZ_RSS_MB    per-process RSS limit in MiB        (default 4096)
#   FUZZ_JOBS      libFuzzer worker processes          (default 1)
#   CARGO_FUZZ     the cargo-fuzz invocation           (default "cargo +nightly fuzz")
#
# Crash artifacts land in `fuzz/artifacts/<target>/` (cargo-fuzz's default
# `-artifact_prefix`); this script only counts and reports them. Exits
# non-zero if any target crashed, so it can gate CI.

set -u -o pipefail

cd "$(dirname "$0")" || exit 1

DURATION="${1:-3600}"
if [ "$#" -gt 0 ]; then
    shift
fi

FUZZ_TIMEOUT="${FUZZ_TIMEOUT:-10}"
FUZZ_RSS_MB="${FUZZ_RSS_MB:-4096}"
FUZZ_JOBS="${FUZZ_JOBS:-1}"
CARGO_FUZZ="${CARGO_FUZZ:-cargo +nightly fuzz}"

ARTIFACT_DIR="$(pwd)/artifacts"
LOG_DIR="$(pwd)/artifacts/logs"
mkdir -p "$ARTIFACT_DIR" "$LOG_DIR"

if [ "$#" -gt 0 ]; then
    TARGETS="$*"
else
    # shellcheck disable=SC2086
    TARGETS="$($CARGO_FUZZ list 2>/dev/null)"
fi

if [ -z "${TARGETS// /}" ]; then
    echo "run-all: 'cargo fuzz list' returned no targets." >&2
    echo "run-all: is the nightly toolchain installed and cargo-fuzz on PATH?" >&2
    exit 2
fi

echo "run-all: ${DURATION}s per target, timeout=${FUZZ_TIMEOUT}s, rss=${FUZZ_RSS_MB}MiB"
echo "run-all: artifacts -> $ARTIFACT_DIR"
echo

FAILED=""
for t in $TARGETS; do
    before=0
    if [ -d "$ARTIFACT_DIR/$t" ]; then
        before="$(find "$ARTIFACT_DIR/$t" -type f | wc -l | tr -d ' ')"
    fi

    printf 'run-all: %-26s ' "$t"
    log="$LOG_DIR/$t.log"
    # shellcheck disable=SC2086
    $CARGO_FUZZ run "$t" --jobs "$FUZZ_JOBS" -- \
        -max_total_time="$DURATION" \
        -timeout="$FUZZ_TIMEOUT" \
        -rss_limit_mb="$FUZZ_RSS_MB" \
        >"$log" 2>&1
    status=$?

    after=0
    if [ -d "$ARTIFACT_DIR/$t" ]; then
        after="$(find "$ARTIFACT_DIR/$t" -type f | wc -l | tr -d ' ')"
    fi
    new=$((after - before))

    if [ "$status" -eq 0 ] && [ "$new" -eq 0 ]; then
        echo "clean"
    else
        echo "FAILED (exit $status, $new new artifact(s)) -- see $log"
        FAILED="$FAILED $t"
    fi
done

echo
if [ -n "${FAILED// /}" ]; then
    echo "run-all: targets with findings:$FAILED"
    echo "run-all: reproduce with"
    echo "         cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<artifact>"
    exit 1
fi

echo "run-all: all targets clean"
