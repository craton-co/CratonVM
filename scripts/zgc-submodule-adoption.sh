#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# How much of each `gc/src/zgc/` submodule is actually used by the collector.
#
# WHY THIS IS A SCRIPT AND NOT A COMMENT. A hand-maintained census sat in
# `gc/src/zgc.rs` for a month and was wrong twice: it said "NONE of them is
# wired in" through a window in which half of them became wired in, and
# `gc/Cargo.toml` plus a known-issues page quoted it -- so a stale number
# became an argument about whether a suite result was an artefact.
#
# Counts references OUTSIDE `mod tests`, which is the question that matters:
# a module a test drives and the collector does not is not adopted.
#
# Usage: scripts/zgc-submodule-adoption.sh          # print the census
#        scripts/zgc-submodule-adoption.sh --write  # update the recorded one
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="$root/gc/src/zgc.rs"
out="$root/types/tests/zgc-submodule-adoption.txt"

modules="adapters barrier census forwarding generation mark metrics page relocate remembered tlab vaddr"

body="$(awk '/^mod tests \{/{exit} {print}' "$src")"
census=""
for m in $modules; do
  # OCCURRENCES, not lines -- `grep -c` counts matching LINES, and a line with
  # two references is two references. The word-boundary class is the one
  # `zgc_submodule_adoption_matches_the_recorded_census` applies, so `mark::`
  # does not also count `young_mark::`; the two must agree exactly or the test
  # is comparing a different question from the one this answers.
  n="$(printf '%s\n' "$body" | grep -oE "(^|[^A-Za-z0-9_])${m}::" | wc -l | tr -d ' ')"
  census="${census}${m} ${n}"$'\n'
done

if [ "${1:-}" = "--write" ]; then
  printf '%s' "$census" > "$out"
  echo "wrote $out"
else
  printf '%s' "$census"
fi
