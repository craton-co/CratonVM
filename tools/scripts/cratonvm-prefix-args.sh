#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# A stand-in for the `cratonvm` binary that prepends a fixed set of VM flags.
#
# WHY THIS EXISTS. Criterion 6 of the JDK-only design ("the strict corpus is
# green") has to be measured on the SUITES — H2, Hibernate, Spring Boot, Tomcat
# — not on probes, and none of those runners has a hook for extra VM flags.
# They all resolve the binary from an environment variable, though
# (`CRATONVM_BIN` for H2, `CRATONVM_EXE` for Tomcat, `CV_BIN` for Hibernate),
# so pointing that variable at this script buys the whole flag surface without
# editing a single shared runner. That matters twice over: those runners are
# owned by other lanes, and a local edit to one of them would silently
# invalidate every baseline captured with the unedited copy.
#
# Usage:
#   CRATONVM_PREFIX_BIN=/path/to/real/cratonvm \
#   CRATONVM_PREFIX_ARGS="--jdk-only" \
#   CRATONVM_BIN=scripts/cratonvm-prefix-args.sh \
#       apps/h2database-suite-runner/run-h2-suite.sh run --category all
#
# CRATONVM_PREFIX_ARGS is word-split deliberately, so multi-flag values work.
#
# It refuses rather than guesses: an unset or non-executable
# CRATONVM_PREFIX_BIN exits 3 with a message, because the failure mode this
# replaces — silently exec'ing something else and producing a full green suite
# run that measured the wrong binary — is exactly the kind of result this lane
# exists to stop trusting.
set -u

if [ -z "${CRATONVM_PREFIX_BIN:-}" ]; then
  echo "cratonvm-prefix-args: CRATONVM_PREFIX_BIN is unset. Point it at the real" >&2
  echo "  cratonvm binary; this script only prepends flags to it." >&2
  exit 3
fi
if [ ! -x "$CRATONVM_PREFIX_BIN" ]; then
  echo "cratonvm-prefix-args: CRATONVM_PREFIX_BIN=$CRATONVM_PREFIX_BIN is not executable." >&2
  exit 3
fi

# shellcheck disable=SC2086 # word splitting is the point
exec "$CRATONVM_PREFIX_BIN" ${CRATONVM_PREFIX_ARGS:-} "$@"
