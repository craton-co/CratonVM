#!/usr/bin/env bash
# Self-test for pair-ab.sh's SUMMARY, and specifically for the SPLIT-HALF check.
#
# Why this exists: on 2026-09-07 a hibernate A/B reported "SMALL BUT CONSISTENT,
# z=+2.47" over classes 0-39 and "z=-2.49" over classes 40-119 -- same binary,
# same lever, disjoint classes. Pooled it was a coin. A significant paired count
# over ONE set of classes is not a result, and the summary now says so by
# scoring its own two halves.
#
# The discriminating pair below is `agree` vs `hidden`: IDENTICAL pooled counts
# (29 of 40, z=+2.85) where only one is trustworthy. A check that cannot tell
# those apart is not doing anything.
#
# Runs the real awk out of pair-ab.sh -- no second copy to drift.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="$HERE/pair-ab.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Extract the summary program from the script itself -- sed/awk only, because
# this must run anywhere the harness does (no python3 on the Windows box).
sed -n "/^awk -F/,/^  }'/p" "$SRC"   | sed "1s/^awk -F'[^']*' '//"   | sed "$ s/'.*$//" > "$TMP/summary.awk"
[ -s "$TMP/summary.awk" ] || { echo "FAILED: could not extract the summary program from $SRC"; exit 1; }

# Fixtures are written as an explicit per-class win pattern, IN RUN ORDER,
# because the two splits differ only in how run order maps to halves.
mk() { # name  pattern (string of 0/1, one char per class, in run order)
  local f="$TMP/$1.tsv" pat="$2" i c am bm d
  printf 'cls	a1	a2	b1	b2	am	bm	delta	noise
' > "$f"
  for (( i=0; i<${#pat}; i++ )); do
    c="${pat:$i:1}"
    if [ "$c" = "1" ]; then am=1000; bm=1005; d=5; else am=1005; bm=1000; d=-5; fi
    printf 'C%s	%s	%s	%s	%s	%s	%s	%s	50
' "$i" "$am" "$am" "$bm" "$bm" "$am" "$bm" "$d" >> "$f"
  done
}

rep() { local n=$1 c=$2 o=""; while [ ${#o} -lt "$n" ]; do o="$o$c"; done; printf '%s' "$o"; }

run()  { awk -F'	' -f "$TMP/summary.awk" "$TMP/$1.tsv"; }
fail=0
want() { if run "$1" | grep -qF "$2"; then echo "  ok   $1: $2"
         else echo "  FAIL $1: expected '$2'"; run "$1" | sed 's/^/       | /'; fail=1; fi }
deny() { if run "$1" | grep -qF "$2"; then echo "  FAIL $1: must NOT say '$2'"; fail=1
         else echo "  ok   $1: does not say '$2'"; fi }

echo "pair-ab selftest"

# 29 of 40 pooled (z=+2.85), both splits agree -> a direction worth reporting
mk agree  "$(rep 15 1)$(rep 5 0)$(rep 14 1)$(rep 6 0)"
# 29 of 40 pooled TOO, but the first half carries all of it
mk hidden "$(rep 20 1)$(rep 9 1)$(rep 11 0)"
# first half all A, second half all B: pure TIME effect, classes interleave evenly
mk drift  "$(rep 20 1)$(rep 20 0)"
# odd classes favour A, even favour B: pure CLASS effect, balanced in time
mk classdep "$(for i in $(seq 0 39); do if [ $((i % 2)) -eq 1 ]; then printf 1; else printf 0; fi; done)"

# 1. a stable direction is reported
want agree  "SMALL BUT CONSISTENT"
deny agree  "DISAGREE"

# 2. THE discriminating pair: identical pooled counts, different treatment
want hidden "one half carries this and the other is flat"

# 3. a first/last flip that ODD/EVEN clears is DRIFT, not a class effect
want drift  "DRIFT DURING THE"
deny drift  "does not generalise"

# 4. an odd/even flip is class-dependence, and odd/even is time-balanced so
#    drift cannot explain it. This is the 2026-09-07 hibernate shape.
want classdep "does not generalise"
want classdep "ODD/EVEN DISAGREE"

# 5. near-zero halves must not count as a disagreement: +0.0 vs -0.4 is
#    "no signal", not a contradiction. An earlier draft called drift a class
#    effect for exactly this reason.
deny drift  "ODD/EVEN DISAGREE"

# The whole point: same pooled count, different verdict.
a=$(run agree  | grep -o 'A faster than B in [0-9]* of [0-9]*')
b=$(run hidden | grep -o 'A faster than B in [0-9]* of [0-9]*')
# Non-empty FIRST: two empty strings compare equal, and an earlier draft of this
# very test reported "ok" when the awk had not run at all.
if [ -z "$a" ] || [ -z "$b" ]; then
  echo "  FAIL could not read a pooled count -- the summary did not run"; fail=1
elif [ "$a" = "$b" ]; then
  echo "  ok   agree/hidden share a pooled count ($a) and are still treated differently"
else
  echo "  FAIL fixtures drifted apart: '$a' vs '$b'"; fail=1
fi

[ "$fail" -eq 0 ] && echo "PASS" || { echo "FAILED"; exit 1; }
