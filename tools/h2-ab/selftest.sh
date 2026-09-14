#!/usr/bin/env bash
# Self-test for h2-ab.sh's ANALYSIS. Feeds recorded samples through --analyze so
# the statistics are checked without running a VM (a timing harness whose maths
# is only exercised by real runs is a harness nobody can check).
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
H2="$HERE/h2-ab.sh"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
fail=0

mk() { # name  then rows of "round arm ms"
  local f="$TMP/$1.tsv"; shift
  printf 'round\tarm\tms\n' > "$f"
  while [ $# -gt 0 ]; do printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$f"; shift 3; done
}
run()  { bash "$H2" --analyze "$TMP/$1.tsv"; }
want() { if run "$1" | grep -qF "$2"; then echo "  ok   $1: $2"
         else echo "  FAIL $1: expected '$2'"; run "$1" | sed 's/^/       | /'; fail=1; fi }

echo "h2-ab selftest"

# 1. A big effect over a TIGHT control must be reported.
#    A~1000, B~1100 (10% slower), control pair 1000/1005 (0.5% floor).
mk big  1 A 1000  1 B 1100  1 C1 1000  1 C2 1005 \
        2 A 1002  2 B 1098  2 C1 1001  2 C2 1004
want big "VERDICT: effect"
want big "ONE UNIT ONLY"

# 2. The SAME effect under a LOOSE control must not be. Control 1000/1200 = 20%.
mk loose 1 A 1000  1 B 1100  1 C1 1000  1 C2 1200 \
         2 A 1002  2 B 1098  2 C1 1000  2 C2 1150
want loose "VERDICT: UNMEASURABLE"

# 3. No control at all -> no floor -> refuse, rather than silently comparing
#    against nothing. This is the failure mode the whole design guards.
mk nocontrol 1 A 1000  1 B 1100  2 A 1002  2 B 1098
want nocontrol "VERDICT: NO CONTROL"

# 4. Sign convention: B slower than A must read as A faster (positive effect).
want big "positive = A faster"
if run big | grep -qE 'effect *: \+'; then echo "  ok   big: effect is positive when B is slower"
else echo "  FAIL big: sign convention wrong"; run big | sed 's/^/       | /'; fail=1; fi

# 5. The floor is the WORST same-config disagreement. Assert the REPORTED
#    VALUE, not just the verdict: an earlier draft checked only the verdict,
#    and mutating "worst" to "last" still passed -- awk visits an associative
#    array in unspecified order, so both readings gave the same UNMEASURABLE.
#    round must not be averaged away by a good one.
# worst round FIRST on purpose: if the code took the last-visited round rather
# than the maximum, this fixture would read a 0.2% floor instead of 30%.
mk worst 1 A 1000  1 B 1100  1 C1 1000  1 C2 1300 \
         2 A 1000  2 B 1100  2 C1 1000  2 C2 1002 \
         3 A 1000  3 B 1100  3 C1 1000  3 C2 1003
want worst "VERDICT: UNMEASURABLE"
floor=$(run worst | grep -o "noise floor   : [0-9.]*" | grep -o "[0-9.]*$")
if [ "$floor" = "30.0" ]; then echo "  ok   worst: floor is the WORST disagreement (30.0%), not another round"
else echo "  FAIL worst: floor reads '$floor', expected 30.0"; fail=1; fi

[ "$fail" -eq 0 ] && echo "PASS" || { echo "FAILED"; exit 1; }
