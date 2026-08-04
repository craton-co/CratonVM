#!/bin/bash
# osr-exit-differential.sh — does a forced OSR exit resume the interpreter in
# the state the program says it is in?
#
# `docs/feature-designs/jit-osr-exit-and-recompile.md` step 3, the increment the
# osr-02 lane calls the point of the lane. The defect is a WRONG ANSWER, not a
# slowdown: if the bail point is not the exact interpreter state the loop was
# in, the loop body executes more times than the program says. The recorded
# instance is 20 000 iterations requested and 20 008 executed — a discrepancy no
# termination test, and no final-sum check, can see.
#
# So this compares COUNTS OF EXECUTIONS, not results.
# `probes/OsrExitDifferentialProbe.java` publishes three observables per shape:
#
#   execs   a static counter incremented once per loop-body execution. A re-run
#           iteration increments it twice. Both the interpreter and compiled
#           code commit this store.
#   trace   an FNV-1a chain over the induction variable AND the loop-carried
#           state, mixed once per body execution — a digest of the frame at
#           every iteration boundary, so a resume one iteration early or late
#           perturbs it even when `execs` is restored by a compensating skip.
#   result  the shape's return value: the ordinary correctness check, kept
#           because a transfer that corrupts a live-across local shows up here
#           and nowhere else.
#
# Every arm's whole stdout must be byte-identical. HotSpot is the control; a
# CratonVM arm compared only against another CratonVM arm proves nothing about
# either.
#
# ARMS
#
#   hotspot            the control
#   nojit              CratonVM, interpreter only — no OSR at all
#   default            CratonVM, default flags
#   exit-test          unconditional bail at the loop header on the first reach
#                      (iteration 0, where reject and transfer coincide)
#   exit-after-N       bail on the N-th reach of the header, for several N, so
#                      the body COMMITS iterations before the exit and the
#                      transfer carries genuinely JIT-advanced state. This is
#                      the arm the lane exists for.
#
# The forced-exit levers need `CRATONVM_DEOPT_REAL`, which is DEFAULT-ON
# (`jit::deopt_real_enabled` returns true for `Err(_)`), so they are set as
# their own tokens and nothing else has to be armed.
#
# NOT ARMED, deliberately: the bytecode loop rewriter. Arming it also disables
# the native byte-copy unroller — they are exact complements — so an armed run
# compared against an unarmed one measures both changes at once
# (`docs/jit/loop-rewriter-wiring.md`). Pass --loop-xform to add that arm; it is
# then compared against the other arms, which is what makes the one-to-many
# reverse mapping inside a transformed region observable.
#
# Usage:
#   osr-exit-differential.sh -Exe /abs/path/to/cratonvm [--java /path/to/java]
#                            [--n 400000] [--after "1 2 7 64 1000"] [--loop-xform]
#                            [--keep DIR]

set -uo pipefail

EXE=""
JAVA="${JAVA_HOME:+$JAVA_HOME/bin/java}"
[[ -n "$JAVA" ]] || JAVA="$(command -v java || true)"
N=400000
AFTERS="1 2 7 64 1000"
LOOP_XFORM=0
KEEP=""
TIMEOUT=600

while [[ $# -gt 0 ]]; do
  case "$1" in
    -Exe) EXE="$2"; shift 2 ;;
    --java) JAVA="$2"; shift 2 ;;
    --n) N="$2"; shift 2 ;;
    --after) AFTERS="$2"; shift 2 ;;
    --loop-xform) LOOP_XFORM=1; shift ;;
    --keep) KEEP="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$EXE" ]]; then
  echo "osr-exit-differential.sh: -Exe is required" >&2
  exit 2
fi

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SRC="$REPO/probes/OsrExitDifferentialProbe.java"
if [[ ! -f "$SRC" ]]; then
  echo "osr-exit-differential.sh: $SRC is missing" >&2
  exit 2
fi

WORK="${KEEP:-$(mktemp -d)}"
mkdir -p "$WORK"
CLASSES="$WORK/classes"
mkdir -p "$CLASSES"

# A probe that does not COMPILE is the classic vacuous pass: the run "skips",
# the harness reports nothing red, and the arm proves nothing. Fail loudly.
if [[ -z "$JAVA" ]]; then
  echo "osr-exit-differential.sh: no java found; set --java or JAVA_HOME" >&2
  exit 2
fi
JAVAC="$(dirname "$JAVA")/javac"
if [[ ! -x "$JAVAC" ]]; then
  echo "osr-exit-differential.sh: $JAVAC is not executable" >&2
  exit 2
fi
# No LC_ALL=C here: it breaks javac's source encoding
# (`docs/known-issues/...lc-all-c...`), and this file is UTF-8.
if ! "$JAVAC" -d "$CLASSES" "$SRC" > "$WORK/javac.log" 2>&1; then
  echo "osr-exit-differential.sh: probe failed to compile" >&2
  cat "$WORK/javac.log" >&2
  exit 1
fi

declare -a ARM_NAMES=()
declare -a ARM_FILES=()

run_arm() {
  local name="$1"; shift
  local out="$WORK/$name.out"
  echo "  running $name" >&2
  timeout "$TIMEOUT" env "$@" > "$out" 2> "$WORK/$name.err"
  local rc=$?
  if [[ $rc -ne 0 ]]; then
    echo "osr-exit-differential.sh: arm '$name' exited $rc" >&2
    tail -20 "$WORK/$name.err" >&2
    ARM_NAMES+=("$name(FAILED rc=$rc)")
    ARM_FILES+=("$out")
    return
  fi
  # An arm that produced no accumulator line ran something other than the
  # probe. Do not let it into the comparison as a vacuous match.
  if ! grep -q '^OsrExitDifferentialProbe acc=' "$out"; then
    echo "osr-exit-differential.sh: arm '$name' produced no accumulator line" >&2
    tail -20 "$WORK/$name.err" >&2
    ARM_NAMES+=("$name(NO-OUTPUT)")
    ARM_FILES+=("$out")
    return
  fi
  ARM_NAMES+=("$name")
  ARM_FILES+=("$out")
}

echo "osr-exit differential — n=$N, work dir $WORK" >&2

run_arm hotspot "$JAVA" -cp "$CLASSES" OsrExitDifferentialProbe "$N"
run_arm nojit "$EXE" --nojit -cp "$CLASSES" OsrExitDifferentialProbe "$N"
run_arm default "$EXE" -cp "$CLASSES" OsrExitDifferentialProbe "$N"
run_arm exit-test CRATONVM_OSR_EXIT_TEST=1 "$EXE" -cp "$CLASSES" OsrExitDifferentialProbe "$N"
for a in $AFTERS; do
  run_arm "exit-after-$a" "CRATONVM_OSR_EXIT_AFTER=$a" "$EXE" -cp "$CLASSES" \
      OsrExitDifferentialProbe "$N"
done
if [[ "$LOOP_XFORM" == "1" ]]; then
  run_arm loop-xform CRATONVM_JIT_BYTECODE_LOOP_XFORM=1 "$EXE" -cp "$CLASSES" \
      OsrExitDifferentialProbe "$N"
  for a in $AFTERS; do
    run_arm "loop-xform-after-$a" CRATONVM_JIT_BYTECODE_LOOP_XFORM=1 \
        "CRATONVM_OSR_EXIT_AFTER=$a" "$EXE" -cp "$CLASSES" \
        OsrExitDifferentialProbe "$N"
  done
fi

# ── The comparison ───────────────────────────────────────────────────────
# HotSpot is arm 0 and is the reference. Diff the WHOLE stdout, not just the
# accumulator: a per-shape line tells you which shape diverged, which an
# accumulator alone cannot.
REF="${ARM_FILES[0]}"
fail=0
echo
printf '%-24s %-8s %s\n' arm verdict "acc"
for i in "${!ARM_NAMES[@]}"; do
  name="${ARM_NAMES[$i]}"
  file="${ARM_FILES[$i]}"
  acc="$(grep -m1 '^OsrExitDifferentialProbe acc=' "$file" 2>/dev/null | sed 's/.*acc=//')"
  if [[ "$name" == *FAILED* || "$name" == *NO-OUTPUT* ]]; then
    printf '%-24s %-8s %s\n' "$name" "ERROR" "-"
    fail=1
    continue
  fi
  if diff -q "$REF" "$file" > /dev/null 2>&1; then
    printf '%-24s %-8s %s\n' "$name" "ok" "$acc"
  else
    printf '%-24s %-8s %s\n' "$name" "DIVERGED" "$acc"
    fail=1
    echo "    first differing lines:"
    diff "$REF" "$file" | head -12 | sed 's/^/      /'
  fi
done

echo
if [[ $fail -eq 0 ]]; then
  echo "OSR exit differential: every arm byte-identical to HotSpot."
else
  echo "OSR exit differential: DIVERGENCE — see the arm table above." >&2
fi
[[ -n "$KEEP" ]] || echo "(transcripts in $WORK)" >&2
exit $fail
