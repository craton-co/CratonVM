#!/usr/bin/env bash
# RI.1 — SPECjvm2008 compiler.compiler benchmark runs end-to-end.
#
# SPECjvm2008 is a licensed benchmark suite, so CI only runs this item
# when the environment variable `SPECJVM_MIRROR_URL` points at a
# pre-staged mirror hosting `SPECjvm2008.jar`. Without it, the runner
# exits 78 (EX_CONFIG) — a "skipped, not failed" signal the summary
# job interprets as inconclusive.

source "$(dirname "$0")/common.sh"

JAR="$FIXTURE_CACHE/SPECjvm2008.jar"

if [[ ! -s "$JAR" ]]; then
    if [[ -n "${SPECJVM_MIRROR_URL:-}" ]]; then
        smoke_download "$SPECJVM_MIRROR_URL/SPECjvm2008.jar" "$JAR"
    else
        echo "RI.1: SPECJVM_MIRROR_URL unset; skipping (licensing)" >&2
        exit 78
    fi
fi

SMOKE_TIMEOUT=900 smoke_run_cratonvm --Xmx 2g --jar "$JAR" -- -bt 1 -i 1 compiler.compiler

smoke_require_signal "Composite result:|Valid run, Score"
