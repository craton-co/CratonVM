#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
# Paired USER-CPU A/B of one JIT flag, both arms from one binary.
#
# The same shape as `flag-ab.sh`, measuring USER CPU instead of wall clock.
#
# It exists because wall clock stops working on a shared host. Measuring
# `CRATONVM_JIT_IR_CARRY_RCX_FOLDED` on a 1.009x effect, `flag-ab.sh` reported
# A-vs-C noise floors of 6.1% and then 14.1% at load 40-90 and returned
# UNMEASURABLE with the sign flipping between runs -- the two runs were
# describing the machine. The same effect on the same binary reproduced at a
# 0.1% floor here, twice.
#
# User CPU works because a DESCHEDULED process stops accumulating it: other
# tenants cost the run wall-clock time and not measured time. It is what
# `docs/JIT_OPTIMIZATION.md` falls back to for exactly this reason. What it
# cannot see is anything that shows up as stall rather than as instructions
# retired -- a change that trades cache misses for work is invisible to it, so
# a wall-clock number on a quiet host is still the better instrument when one
# is available.
#
# Give it enough work per sample. `%U` has 10 ms resolution, and a sample that
# is mostly VM start-up measures VM start-up: the first cut here ran at a rep
# count that left 0.26 s on the clock and reported UNMEASURABLE for that reason
# alone. Aim for seconds.
#
#   cpu-ab.sh <exe> <cp> <class> <FLAG> <rounds> [extra args...]
#
# Arms interleaved ABBA / BAAB by round, with a control arm identical to A
# measured every round; its spread is the floor.
set -u
EXE=$1; CP=$2; CLASS=$3; FLAG=$4; ROUNDS=${5:-10}; shift 5
# Captured BEFORE run_one is defined: inside a function `"$@"` is the
# FUNCTION's arguments, which silently dropped these on the first cut and
# measured the default rep count -- and therefore mostly VM start-up.
EXTRA=("$@")
SAMPLES=$(mktemp); trap 'rm -f "$SAMPLES"' EXIT

run_one() {  # label, flag value
  local u
  u=$( { /usr/bin/time -f '%U' env CRATONVM_JIT_FORCE_C2=1 "$FLAG=$2" \
           "$EXE" -Xmx2g -cp "$CP" "${EXTRA[@]}" "$CLASS" >/dev/null; } 2>&1 | tail -1 )
  case "$u" in
    [0-9]*) printf '%s\t%s\n' "$1" "$u" >>"$SAMPLES" ;;
    *) echo "RUNFAIL $1: $u" >&2 ;;
  esac
}

echo "# $FLAG user-CPU A/B: A=0 (control C=0) B=1  class=$CLASS rounds=$ROUNDS"
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 0 ]; then order="A B C B"; else order="B A B C"; fi
  for arm in $order; do
    case "$arm" in
      A|C) run_one "$arm" 0 ;;
      B)   run_one "$arm" 1 ;;
    esac
  done
done

echo "--- samples ---"; cat "$SAMPLES"
echo "--- verdict ---"
awk -F'\t' '
  { v[$1] = v[$1] " " $2; n[$1]++ }
  function median(l,   a, c, i, j, t) {
    c = split(l, a, " ")
    for (i = 1; i < c; i++) for (j = i + 1; j <= c; j++)
      if (a[i] + 0 > a[j] + 0) { t = a[i]; a[i] = a[j]; a[j] = t }
    return (c % 2) ? a[(c + 1) / 2] + 0 : (a[c / 2] + a[c / 2 + 1]) / 2
  }
  END {
    ma = median(v["A"]); mc = median(v["C"]); mb = median(v["B"])
    printf "A (flag off) median %.3f s  n=%d\n", ma, n["A"]
    printf "C (control)  median %.3f s  n=%d\n", mc, n["C"]
    printf "B (flag on)  median %.3f s  n=%d\n", mb, n["B"]
    base = (ma + mc) / 2
    floor = (ma > mc ? ma - mc : mc - ma) / base * 100
    eff = (mb - base) / base * 100
    printf "noise floor (A vs C)    : %.1f%%\n", floor
    printf "effect (B vs mean(A,C)) : %+.1f%%   ratio %.3fx\n", eff, mb / base
    ae = (eff < 0 ? -eff : eff)
    printf "VERDICT: %s\n", (ae <= floor ? "UNMEASURABLE (effect inside the floor)" \
                                         : (eff < 0 ? "FASTER" : "SLOWER"))
  }' "$SAMPLES"
