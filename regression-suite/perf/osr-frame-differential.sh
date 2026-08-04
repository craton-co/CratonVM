#!/bin/bash
# osr-frame-differential.sh — the FRAME-level half of the osr-02 exit
# differential.
#
# `osr-exit-differential.sh` compares what the program COMPUTES across arms.
# This compares the interpreter's frames themselves — locals and operand stack,
# slot for slot, tag included — against the frames an un-compiled run holds at
# the same iteration count. It is the brief's item 2 taken literally, and it
# closes the gap that one names:
#
#   > the differential is a Java-level oracle, not a frame comparator … a
#   > divergence in a local the rest of the loop never reads would not be seen
#
# Two runs of one program:
#
#   truth   `--nojit`. Every back edge is interpreted, so every arrival is
#           recorded and the sequence IS the un-compiled trajectory.
#   test    OSR armed, forced exit, so the loop is entered, advanced in
#           compiled code, and bailed back — repeatedly.
#
# `osr-frame-comparator.py` then maps every record of the test run to its index
# in the truth sequence by exact frame equality and requires that index to
# strictly increase. A frame matching nothing is a state the program cannot be
# in; an index that repeats is an iteration running twice.
#
# WHY THE TRIP COUNT IS SMALL AND THE OSR THRESHOLD IS LOW. The truth arm emits
# one line per back edge, so a 300k-trip loop is a 25 MB transcript per shape.
# Lowering `CRATONVM_TIER_OSR_BACKEDGE` makes OSR fire early enough that a few
# thousand trips still produce many entries and exits — which is what makes the
# small run non-vacuous, and the comparator FAILS a test arm with no exits
# rather than reporting a green table.
#
# Usage:
#   osr-frame-differential.sh -Exe /abs/path/to/cratonvm [--n 4000]
#                             [--after 7] [--backedge 200] [--keep DIR]

set -uo pipefail

EXE=""
N=4000
AFTER=7
BACKEDGE=200
KEEP=""
TIMEOUT=900
CLASS="OsrExitDifferentialProbe"

while [[ $# -gt 0 ]]; do
  case "$1" in
    -Exe) EXE="$2"; shift 2 ;;
    --n) N="$2"; shift 2 ;;
    --after) AFTER="$2"; shift 2 ;;
    --backedge) BACKEDGE="$2"; shift 2 ;;
    --keep) KEEP="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
[[ -n "$EXE" ]] || { echo "osr-frame-differential.sh: -Exe is required" >&2; exit 2; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SRC="$REPO/probes/OsrExitDifferentialProbe.java"
[[ -f "$SRC" ]] || { echo "osr-frame-differential.sh: $SRC is missing" >&2; exit 2; }

JAVAC="$(command -v javac || true)"
[[ -x "$JAVAC" ]] || { echo "osr-frame-differential.sh: javac not found" >&2; exit 2; }

WORK="${KEEP:-$(mktemp -d)}"
mkdir -p "$WORK/classes"
# A probe that does not COMPILE skips as "unavailable" and the run passes
# vacuously. Fail loudly instead.
if ! "$JAVAC" -d "$WORK/classes" "$SRC" > "$WORK/javac.log" 2>&1; then
  echo "osr-frame-differential.sh: probe failed to compile" >&2
  cat "$WORK/javac.log" >&2
  exit 1
fi

echo "osr-frame differential — n=$N, exit-after=$AFTER, osr-backedge=$BACKEDGE" >&2
echo "  running truth (--nojit)" >&2
timeout "$TIMEOUT" env \
  "CRATONVM_DBG_OSR_FRAME_TRACE=$CLASS" \
  "$EXE" --nojit -cp "$WORK/classes" OsrExitDifferentialProbe "$N" \
  > "$WORK/truth.out" 2> "$WORK/truth.err"
truth_rc=$?

echo "  running test (OSR + forced exit)" >&2
timeout "$TIMEOUT" env \
  "CRATONVM_DBG_OSR_FRAME_TRACE=$CLASS" \
  "CRATONVM_OSR_EXIT_AFTER=$AFTER" \
  "CRATONVM_TIER_OSR_BACKEDGE=$BACKEDGE" \
  "$EXE" -cp "$WORK/classes" OsrExitDifferentialProbe "$N" \
  > "$WORK/test.out" 2> "$WORK/test.err"
test_rc=$?

fail=0
if [[ $truth_rc -ne 0 || $test_rc -ne 0 ]]; then
  echo "osr-frame-differential.sh: a run exited non-zero (truth=$truth_rc test=$test_rc)" >&2
  tail -20 "$WORK/truth.err" "$WORK/test.err" >&2
  fail=1
fi

# The two runs must also agree on the ANSWER. A frame comparison that passed
# while the program computed something different would be reporting on the
# wrong thing entirely.
if ! diff -q "$WORK/truth.out" "$WORK/test.out" > /dev/null 2>&1; then
  echo "osr-frame-differential.sh: the two arms disagree on the program's OUTPUT:" >&2
  diff "$WORK/truth.out" "$WORK/test.out" | head -12 >&2
  fail=1
fi

# The checker is a guard, and a guard that has never been shown to fire is an
# assumption. Its self-test builds transcripts with known verdicts — including
# the replay shape that walked past an earlier version of it — and runs first,
# so a comparator that has stopped catching anything cannot report a green run.
echo
if ! python3 "$HERE/osr-frame-comparator.py" --selftest; then
  echo "osr-frame-differential.sh: the comparator's own self-test failed; its" >&2
  echo "  verdict on the real transcripts below cannot be trusted." >&2
  fail=1
fi

echo
# `--min-advance 1`: the run under test uses CRATONVM_OSR_EXIT_AFTER=$AFTER, so
# the compiled body is meant to advance the frame before bailing. Zero would be
# correct only for the unconditional-at-header trigger.
python3 "$HERE/osr-frame-comparator.py" --min-advance 1 \
    "$WORK/truth.err" "$WORK/test.err" || fail=1

echo
if [[ $fail -eq 0 ]]; then
  echo "OSR frame differential: PASS"
else
  echo "OSR frame differential: FAILED" >&2
fi
[[ -n "$KEEP" ]] || echo "(transcripts in $WORK)" >&2
exit $fail
