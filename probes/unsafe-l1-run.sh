#!/usr/bin/env bash
# l1run.sh [sweep|null|all] -- the L1 Unsafe probes on HotSpot and on CratonVM
# in both modes, plus the diffs. Detached: writes DONE on every exit path so a
# client-side poll always terminates.
#
# stdout ONLY on every arm. `2>&1` would put four sun.misc.Unsafe deprecation
# WARNINGs into the HotSpot transcript and none into CratonVM's -- the harness
# artefact that invented four differences in this family once already.
#
# Two things a crashing arm does to a transcript, both handled here:
#   * HotSpot writes its fatal-error summary to STDOUT. Inside the null-argument
#     loop that turned 33 rows into 205 and made `diff` unreadable. Only lines
#     that look like a probe row (`^<n> `) are kept.
#   * a core dump per crash is ~2 GB on /data. `ulimit -c 0` and
#     `-XX:-CreateCoredumpOnCrash`.
set +e
ulimit -c 0
source /data/toolchain/env.sh
P=/data/l1u-probes
CV=/data/l1u-target/release/cratonvm
JH=/data/toolchain/jdk-25
XP="--add-exports java.base/jdk.internal.misc=ALL-UNNAMED"
# `-XX:-UseCompressedOops`: configure the ORACLE like the VM under test.
# CratonVM does not use compressed oops, so a default HotSpot reports
# `arrayIndexScale` = 4 on every reference array where CratonVM reports 8,
# and three rows of this lane's residual were that and nothing else.
# MEASURED 2026-08-30: with this flag the oracle reports 8 on all three,
# the sweep diff falls from 20 changed lines to 14, and no new difference
# appears anywhere in the 457 rows. A differential whose oracle is
# configured unlike the VM under test reports its own configuration as a
# defect, forever, in a column readers have to be told to ignore.
QUIET="-XX:-CreateCoredumpOnCrash -XX:ErrorFile=/dev/null -XX:-UseCompressedOops"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
WHAT=${1:-all}
cd "$P" || { echo GAVEUP; exit 1; }
rm -f DONE hs_err_pid*.log core.*

echo "=== compile"
javac -nowarn -Xmaxwarns 0 $XP -d out \
      UnsafeShadowSweep.java UnsafeNullArgProbe.java UnsafeSubwordProbe.java \
      AllocBoundary.java 2>&1 \
  | grep -E "error:" | head -20
echo "compile done"

if [ "$WHAT" = all ] || [ "$WHAT" = sweep ]; then
  echo "=== sweep: hotspot"
  timeout 300 java $QUIET $XP -cp out UnsafeShadowSweep > sweep-hs.out 2>/dev/null
  echo "hs rc=$?  lines=$(wc -l < sweep-hs.out)"

  echo "=== sweep: cratonvm compatible"
  timeout 900 "$CV" --java-home "$JH" $XP -cp out UnsafeShadowSweep > sweep-cv-compat.out 2>/dev/null
  echo "compat rc=$?  lines=$(wc -l < sweep-cv-compat.out)"

  echo "=== sweep: cratonvm --jdk-only"
  timeout 900 "$CV" --java-home "$JH" --jdk-only $XP -cp out UnsafeShadowSweep > sweep-cv-strict.out 2>/dev/null
  echo "strict rc=$?  lines=$(wc -l < sweep-cv-strict.out)"
fi

# One call per process, behind a timeout: a row that crashes or never returns
# costs its own row and nothing else. `rc=124` is `timeout`'s -- it is what
# distinguishes "hung" from "threw", and both from "answered".
one_per_process () {
  local cls=$1 stem=$2 tmo=$3
  local N
  N=$(java $XP -cp out "$cls" count 2>/dev/null)
  echo "=== $stem cases: $N"
  for arm in hs compat strict; do
    : > "$stem-$arm.out"
    for ((i = 0; i < N; i++)); do
      case "$arm" in
        hs)     timeout "$tmo" java $QUIET $XP -cp out "$cls" "$i" > .row 2>/dev/null ;;
        compat) timeout "$tmo" "$CV" --java-home "$JH" $XP -cp out "$cls" "$i" > .row 2>/dev/null ;;
        strict) timeout "$tmo" "$CV" --java-home "$JH" --jdk-only $XP -cp out "$cls" "$i" > .row 2>/dev/null ;;
      esac
      rc=$?
      # Only a probe row survives; a fatal-error summary does not. HotSpot
      # writes that summary to STDOUT, which once turned 33 rows into 205.
      row=$(grep -aE "^[0-9]+ " .row | head -1)
      if [ -n "$row" ]; then
        printf '%s rc=%s\n' "$row" "$rc" >> "$stem-$arm.out"
      elif [ "$rc" = 124 ]; then
        printf '%d NEVER RETURNED rc=%s\n' "$i" "$rc" >> "$stem-$arm.out"
      else
        printf '%d THE VM DIED rc=%s\n' "$i" "$rc" >> "$stem-$arm.out"
      fi
      rm -f hs_err_pid*.log core.*
    done
    rm -f .row
    echo "$arm $stem rows=$(wc -l < $stem-$arm.out)"
  done
}

if [ "$WHAT" = all ] || [ "$WHAT" = null ]; then
  one_per_process UnsafeNullArgProbe null 90
fi

if [ "$WHAT" = all ] || [ "$WHAT" = subword ]; then
  one_per_process UnsafeSubwordProbe subword 45
fi

echo "=== diffs"
for m in compat strict; do
  [ -f sweep-cv-$m.out ] && echo "DIFF sweep $m: $(diff sweep-hs.out sweep-cv-$m.out | grep -c '^[<>]') changed lines"
  for stem in null subword; do
    [ -f $stem-$m.out ] && echo "DIFF $stem $m: $(diff $stem-hs.out $stem-$m.out | grep -c '^[<>]') changed lines"
  done
done
[ -f sweep-cv-strict.out ] && echo "MODE-DRIFT sweep: $(diff sweep-cv-compat.out sweep-cv-strict.out | grep -c '^[<>]') changed lines"
for stem in null subword; do
  [ -f $stem-strict.out ] && echo "MODE-DRIFT $stem: $(diff $stem-compat.out $stem-strict.out | grep -c '^[<>]') changed lines"
done
echo DONE
