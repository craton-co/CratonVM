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
# THE TRUNCATION HAS TO MATCH THE VISIBILITY THE SOURCE ACTUALLY USES. It read
# `/^mod tests \{/` from the day it landed until 2026-09-20, and `gc/src/zgc.rs`
# -- the only one of the four sources with a test module, and the one with
# ~10.8k lines of them -- spells it `pub(crate) mod tests {`. So the truncation
# never fired, the "body" was every line of every file, and the census counted
# the test module as adoption by the collector: `vaddr` read 27 where the
# collector references it 19 times, 42% over. A ratchet on the wrong quantity is
# worse than no ratchet, because it carries the authority of having been
# checked. See `docs/internal/zgc-round-20260920/gap-e-submodule-census-counts-
# test-code.md`.
#
# `--verify` prints, per source, how many lines survived the truncation. A file
# that has a `mod tests` and shows `kept == total` is the bug above, returning.
#
# THIS SCRIPT AND `zgc_submodule_adoption_matches_the_recorded_census`
# (`gc/src/zgc.rs`) MUST TRUNCATE AT THE SAME POINT. The test derives the same
# census from the same four `include_str!`s and compares it against the same
# recorded file; if the two disagree about where `mod tests` starts, one of them
# is measuring a different question and `--write` produces a file that fails the
# build. The three spellings accepted below are the exact three the test's
# `find` chain accepts.
#
# Usage: scripts/zgc-submodule-adoption.sh          # print the census
#        scripts/zgc-submodule-adoption.sh --verify # show the truncation points
#        scripts/zgc-submodule-adoption.sh --write  # update the recorded one
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# THE COLLECTOR, not one file. `zgc.rs` was split on 2026-09-02 and its own
# child modules are as much "the collector" as it is -- counting only `zgc.rs`
# would report a module as abandoned the moment its caller moved next door.
# The twelve modules being counted are NOT in this list: a module referencing
# itself is not adoption.
srcs="$root/gc/src/zgc.rs $root/gc/src/zgc/starts.rs $root/gc/src/zgc/sweep.rs $root/gc/src/zgc/arena_tlab.rs"
out="$root/types/tests/zgc-submodule-adoption.txt"

modules="adapters barrier census forwarding generation mark metrics page relocate remembered tlab vaddr"

# The three spellings, anchored at column 0. `pub(crate) mod tests {` is the one
# `gc/src/zgc.rs` uses and the one the original pattern missed. Bracket
# expressions rather than backslash escapes: awk processes `\(` in a regex
# literal as a plain `(`, which in an ERE is a GROUP OPEN -- `^pub\(crate\) mod`
# would match `pubcrate mod` and not the spelling we are here to catch. `[(]`
# has no such reading.
zgc_body_of() {
  awk '
    /^mod tests [{]/            { exit }
    /^pub mod tests [{]/        { exit }
    /^pub[(]crate[)] mod tests [{]/ { exit }
                                { print }
  ' "$1"
}

if [ "${1:-}" = "--verify" ]; then
  for f in $srcs; do
    total="$(wc -l < "$f" | tr -d ' ')"
    kept="$(zgc_body_of "$f" | wc -l | tr -d ' ')"
    has="$(grep -cE '^[[:space:]]*(pub[^[:space:]]* )?mod tests' "$f" || true)"
    printf '%s kept=%s total=%s mod_tests_decls=%s\n' \
      "${f#"$root/"}" "$kept" "$total" "$has"
  done
  exit 0
fi

body=""
for f in $srcs; do
  body="${body}$(zgc_body_of "$f")"$'
'
done
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
