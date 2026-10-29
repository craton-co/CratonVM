#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Replay every committed seed and every committed crash reproducer through
# its fuzz target, once each, and exit non-zero if anything crashes or if a
# target has no seeds at all.
#
# This is the *bounded, deterministic* half of the fuzzing story and the one
# meant for CI. `run-all.sh` runs an open-ended coverage-guided campaign
# measured in hours; this finishes in seconds because libFuzzer's `-runs=0`
# executes each corpus file exactly once and exits. It finds nothing new —
# that is not its job. Its job is to make the committed corpus load-bearing:
#
#   * a regression reproducer committed under `fuzz/artifacts/<target>/`
#     after a bug is fixed stays fixed, on every commit;
#   * a seed that stops matching its target's per-input layout (because the
#     layout changed and `corpus/gen_seeds.py` was not re-run) shows up as a
#     failure rather than as a silently-ignored file;
#   * a new target cannot land with an empty corpus, which is the state that
#     makes a coverage-guided campaign near-useless.
#
# Usage:
#   ./replay-corpus.sh                       # every target
#   ./replay-corpus.sh fuzz_zip_entry ...    # only these
#
# Environment overrides:
#   FUZZ_TIMEOUT      per-input timeout in seconds   (default 25)
#   FUZZ_RSS_MB       per-process RSS limit in MiB   (default 4096)
#   CARGO_FUZZ        the cargo-fuzz invocation      (default "cargo +nightly fuzz")
#   ALLOW_EMPTY_CORPUS  space-separated targets exempt from the seed
#                       requirement. Every exemption needs a comment in
#                       `docs/known-issues/c2/fuzzing-state.md` saying why.

set -u -o pipefail

cd "$(dirname "$0")" || exit 1

FUZZ_TIMEOUT="${FUZZ_TIMEOUT:-25}"
FUZZ_RSS_MB="${FUZZ_RSS_MB:-4096}"
CARGO_FUZZ="${CARGO_FUZZ:-cargo +nightly fuzz}"
ALLOW_EMPTY_CORPUS="${ALLOW_EMPTY_CORPUS:-}"

LOG_DIR="$(pwd)/artifacts/logs"
mkdir -p "$LOG_DIR"

if [ "$#" -gt 0 ]; then
    TARGETS="$*"
else
    # shellcheck disable=SC2086
    TARGETS="$($CARGO_FUZZ list 2>/dev/null)"
fi

if [ -z "${TARGETS// /}" ]; then
    echo "replay-corpus: 'cargo fuzz list' returned no targets." >&2
    echo "replay-corpus: is the nightly toolchain installed and cargo-fuzz on PATH?" >&2
    exit 2
fi

# Replay one directory of inputs through one target. Echoes nothing on
# success; returns non-zero and leaves the log behind on failure.
replay_dir() {
    local target="$1" dir="$2" label="$3"
    local log="$LOG_DIR/${target}.${label}.log"
    # shellcheck disable=SC2086
    $CARGO_FUZZ run "$target" "$dir" -- \
        -runs=0 \
        -timeout="$FUZZ_TIMEOUT" \
        -rss_limit_mb="$FUZZ_RSS_MB" \
        >"$log" 2>&1
}

count_files() {
    [ -d "$1" ] || { echo 0; return; }
    find "$1" -type f ! -name '*.log' ! -name 'README.md' ! -name '*.py' \
        | wc -l | tr -d ' '
}

EMPTY=""
FAILED=""
TOTAL_INPUTS=0

for t in $TARGETS; do
    corpus="corpus/$t"
    artifacts="artifacts/$t"
    seeds="$(count_files "$corpus")"
    repros="$(count_files "$artifacts")"

    printf 'replay-corpus: %-26s %4s seed(s) %4s repro(s)  ' "$t" "$seeds" "$repros"

    if [ "$seeds" -eq 0 ]; then
        case " $ALLOW_EMPTY_CORPUS " in
            *" $t "*) echo "SKIPPED (exempt: no committed seeds)"; continue ;;
            *) echo "NO SEEDS"; EMPTY="$EMPTY $t"; continue ;;
        esac
    fi

    TOTAL_INPUTS=$((TOTAL_INPUTS + seeds + repros))
    status=0
    if ! replay_dir "$t" "$corpus" seeds; then
        status=1
    fi
    if [ "$repros" -gt 0 ] && ! replay_dir "$t" "$artifacts" repros; then
        status=1
    fi

    if [ "$status" -eq 0 ]; then
        echo "ok"
    else
        echo "FAILED -- see $LOG_DIR/$t.*.log"
        FAILED="$FAILED $t"
    fi
done

echo
rc=0

if [ -n "${EMPTY// /}" ]; then
    echo "replay-corpus: targets with no committed seeds:$EMPTY" >&2
    echo "replay-corpus: add them to fuzz/corpus/gen_seeds.py and re-run" >&2
    echo "replay-corpus:   python fuzz/corpus/gen_seeds.py" >&2
    echo "replay-corpus: a coverage-guided fuzzer given no seed starts from" >&2
    echo "replay-corpus: random noise and never reaches the parser." >&2
    rc=1
fi

if [ -n "${FAILED// /}" ]; then
    echo "replay-corpus: targets that crashed on a committed input:$FAILED" >&2
    echo "replay-corpus: reproduce with" >&2
    echo "replay-corpus:   cargo +nightly fuzz run <target> fuzz/corpus/<target>/<seed>" >&2
    rc=1
fi

if [ "$rc" -eq 0 ]; then
    echo "replay-corpus: $TOTAL_INPUTS committed input(s) replayed clean"
fi

exit "$rc"
