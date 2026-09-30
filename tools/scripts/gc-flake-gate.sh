#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Repeat-run gate for the collector test suites.
#
# WHY THIS EXISTS
#
# A `cargo test` binary runs its tests on parallel threads in ONE process, so a
# test that publishes PROCESS-GLOBAL state reinterprets the heaps of every test
# running beside it. That is not a hypothetical failure mode here: five
# `gen_heap` unit tests used to call `narrow_oop::enable(0x2000_0000, 3)` around
# their own assertions, which flips `element_byte_size(Reference)` from 8 to 4
# for the whole process. Reference arrays written at an 8-byte element stride
# were then read back at 4, element `2k+1` decoding the high half of pointer `k`
# as a whole reference. It surfaced as
#
#     ObjectRef pointer not 8-byte aligned: 0x1d1
#
# in `g1::tests::parallel_matches_serial_no_loss_or_dup` and as
# `compressed_oops::assert_region_encodable` firing in `gen_heap` — two flakes
# that were read as a race in G1's parallel evacuator for months and were
# neither G1's nor a race.
#
# A single `cargo test` run does not catch that class of bug: the measured rate
# was 14 failures in 300 full-suite runs (~4.7%), so an ordinary CI job passes
# ~95% of the time while the defect is fully live. Repetition is the only gate
# that sees it.
#
# WHAT IT DOES
#
# Runs each suite's test binary $FLAKE_RUNS times and fails if ANY run fails.
# It does not fail fast: the observed failure COUNT is the number worth having
# in the log (a 1-in-50 flake and a 45-in-50 breakage need different responses),
# and the first few failing logs are printed so the offender is named.
#
# Usage:
#   scripts/gc-flake-gate.sh                  # default runs, default suites
#   FLAKE_RUNS=200 scripts/gc-flake-gate.sh   # deeper local hunt
#   scripts/gc-flake-gate.sh -p cratonvm-vm   # a different crate's lib suite
#
# Exit 0 = every run passed. Exit 1 = at least one run failed.

set -uo pipefail

FLAKE_RUNS="${FLAKE_RUNS:-50}"
# Per-run wall-clock ceiling. A cross-test-pollution bug can also manifest as a
# wedged run rather than a failing one (an evacuation worker spinning on a
# counter that never drains), and an unbounded hang in CI reads as an infra
# problem rather than a test result.
FLAKE_TIMEOUT="${FLAKE_TIMEOUT:-300}"
MAX_LOGS_PRINTED="${MAX_LOGS_PRINTED:-3}"

if [ "$#" -gt 0 ]; then
  SUITES=("$@")
else
  # The collector crates. `cratonvm-gc` is where the defect reproduced;
  # `cratonvm-types` owns the globals it reproduced through, and carries the
  # same shape (`element_byte_size_reference` asserts the wide size in the very
  # binary the narrow-oop tests used to flip).
  SUITES=("-p" "cratonvm-gc" "-p" "cratonvm-types")
fi

# Expand "-p crate -p crate" into one entry per crate.
CRATES=()
for tok in "${SUITES[@]}"; do
  [ "$tok" = "-p" ] && continue
  CRATES+=("$tok")
done

log_dir="$(mktemp -d)"
trap 'rm -rf "$log_dir"' EXIT

overall_rc=0

for crate in "${CRATES[@]}"; do
  echo "=== flake gate: $crate --lib x $FLAKE_RUNS"

  # Build once and drive the binary directly. Going through `cargo test` every
  # iteration would pay a workspace freshness check per run, which on this
  # workspace costs more than the suite itself.
  build_out="$(cargo test -p "$crate" --lib --no-run 2>&1)" || {
    echo "$build_out"
    echo "FAIL: could not build $crate test binary"
    exit 1
  }
  bin="$(printf '%s\n' "$build_out" \
    | sed -n 's/.*Executable unittests[^(]*(\(.*\))$/\1/p' | tail -1)"
  if [ -z "$bin" ] || [ ! -x "$bin" ]; then
    echo "$build_out"
    echo "FAIL: could not locate the $crate test binary in the cargo output"
    exit 1
  fi

  fails=0
  printed=0
  for i in $(seq 1 "$FLAKE_RUNS"); do
    timeout "$FLAKE_TIMEOUT" "$bin" >"$log_dir/run.log" 2>&1
    rc=$?
    if [ "$rc" -ne 0 ]; then
      fails=$((fails + 1))
      if [ "$printed" -lt "$MAX_LOGS_PRINTED" ]; then
        printed=$((printed + 1))
        echo "--- $crate run $i FAILED (rc=$rc)"
        # `rc=124` is the timeout, which prints nothing useful of its own.
        grep -E "^test .* FAILED|panicked at|^test result" "$log_dir/run.log" \
          | head -20
      fi
    fi
  done

  if [ "$fails" -gt 0 ]; then
    echo "FLAKE GATE: $crate failed $fails/$FLAKE_RUNS runs"
    overall_rc=1
  else
    echo "flake gate: $crate clean over $FLAKE_RUNS runs"
  fi
done

if [ "$overall_rc" -ne 0 ]; then
  echo
  echo "A suite that passes once and fails on repetition is usually one test"
  echo "mutating process-global state that another test reads. Do not paper"
  echo "over it with --test-threads=1: that hides the defect rather than"
  echo "removing it, and the global is just as shared in production."
fi

exit "$overall_rc"
