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

mk() { # name first_half_A_wins second_half_A_wins
  local f="$TMP/$1.tsv"; local fw="$2" sw="$3" i
  printf 'cls\ta1\ta2\tb1\tb2\tam\tbm\tdelta\tnoise\n' > "$f"
  for i in $(seq 0 19); do
    if [ "$i" -lt "$fw" ]; then printf 'C%s\t1000\t1000\t1005\t1005\t1000\t1005\t5\t50\n' "$i" >> "$f"
    else printf 'C%s\t1005\t1005\t1000\t1000\t1005\t1000\t-5\t50\n' "$i" >> "$f"; fi
  done
  for i in $(seq 0 19); do
    if [ "$i" -lt "$sw" ]; then printf 'D%s\t1000\t1000\t1005\t1005\t1000\t1005\t5\t50\n' "$i" >> "$f"
    else printf 'D%s\t1005\t1005\t1000\t1000\t1005\t1000\t-5\t50\n' "$i" >> "$f"; fi
  done
}

run()  { awk -F'\t' -f "$TMP/summary.awk" "$TMP/$1.tsv"; }
fail=0
want() { # fixture  must-contain
  if run "$1" | grep -qF "$2"; then echo "  ok   $1: $2"
  else echo "  FAIL $1: expected '$2'"; run "$1" | sed 's/^/       | /'; fail=1; fi
}
deny() {
  if run "$1" | grep -qF "$2"; then echo "  FAIL $1: must NOT say '$2'"; fail=1
  else echo "  ok   $1: does not say '$2'"; fi
}

echo "pair-ab selftest"
mk agree  15 14   # 29/40 pooled, halves +1.79/+1.79
mk hidden 20  9   # 29/40 pooled TOO, halves +4.02/-0.45
mk flip   17  3   # pooled a coin, halves +2.68/-3.13

want agree  "SMALL BUT CONSISTENT"
deny agree  "SPLIT-HALF DISAGREEMENT"
want hidden "SPLIT-HALF DISAGREEMENT"
deny hidden "VERDICT: SMALL BUT CONSISTENT"
want flip   "THE HALVES DISAGREE IN SIGN"

# The whole point: same pooled count, different verdict.
a=$(run agree  | grep -o 'A faster than B in [0-9]* of [0-9]*')
b=$(run hidden | grep -o 'A faster than B in [0-9]* of [0-9]*')
# Non-empty FIRST: two empty strings compare equal, and an earlier draft of this
# very test reported "ok" when the awk had not run at all. A check that passes
# when nothing happened is the failure mode this whole file is about.
if [ -z "$a" ] || [ -z "$b" ]; then
  echo "  FAIL could not read a pooled count -- the summary did not run"; fail=1
elif [ "$a" = "$b" ]; then
  echo "  ok   agree/hidden share a pooled count ($a) and still get different verdicts"
else
  echo "  FAIL fixtures drifted apart: '$a' vs '$b'"; fail=1
fi

[ "$fail" -eq 0 ] && echo "PASS" || { echo "FAILED"; exit 1; }
