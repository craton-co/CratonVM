#!/usr/bin/env bash
# Count what actually reaches the three L1 residual paths, on a real workload.
#
# The suite's runner discards stderr and these instruments are `tracing` warns,
# so the vectors are run DIRECTLY here, one process each, stderr kept. Both
# modes, every vector the corpus schedules.
#
#   R5  UNCLASSIFIED-NULL-BASE      a null-base Unsafe access whose offset is
#                                   not an arena handle, a synthetic offset or
#                                   a registered static field
#   R3  NON-ARRAY-SCALE             arrayIndexScale answering the catch-all 1
#   R1  "minting synthetic offset"  objectFieldOffset1 on a name it cannot find
#
# REUSE THE SUITE'S OWN BUILD. A bare `javac src/*.java` gave 58 errors -- the
# corpus depends on a named module the suite assembles first from `modules/` --
# so no vector ran and every counter read zero. A zero from a run that did not
# happen is not a zero.
#
# POSITIVE CONTROLS FIRST, and the run is abandoned if one is silent. The R3
# instrument was mute on its first placement (an early return stood in front of
# the arm it was in) and only the control said so.
set +e
ulimit -c 0
source /data/toolchain/env.sh
W=/data/cvm-l1u-20260828
CV=/data/l1u-target/release/cratonvm
JDK=/data/toolchain/jdk-25
XP="--add-exports java.base/jdk.internal.misc=ALL-UNNAMED"
OUT=/data/l1u-counts
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
rm -rf "$OUT"; mkdir -p "$OUT"
cd "$W" || { echo "COUNT-DONE GAVEUP"; exit 1; }

echo "=== POSITIVE CONTROLS (each must be non-zero, or the count below is void)"
c1=$(timeout 90 "$CV" --java-home "$JDK" $XP -cp /data/l1u-probes/out UnsafeNullArgProbe 24 2>&1 \
     | grep -ac "UNCLASSIFIED-NULL-BASE")
echo "  R5 control (compareAndSwapInt(null, a field offset)) : $c1"
c2=$(timeout 300 "$CV" --java-home "$JDK" $XP -cp /data/l1u-probes/out UnsafeShadowSweep 2>&1 \
     | grep -ac "NON-ARRAY-SCALE")
echo "  R3 control (arrayIndexScale(String.class))           : $c2"
if [ "$c1" = 0 ] || [ "$c2" = 0 ]; then
  echo "A CONTROL IS SILENT -- the instrument, not the workload. Counts below would be meaningless."
  echo COUNT-DONE
  exit 0
fi

CP="regression-suite/build:regression-suite/build-modules/cratonvm.jdkonly.svc:regression-suite/resources"
CLASSES=$(cd regression-suite/build && ls *.class 2>/dev/null | grep -v '\$' | sed 's/\.class$//')
echo "=== corpus vectors: $(echo "$CLASSES" | wc -w) (from the suite's own build)"

for mode in compat strict; do
  FLAG=""
  [ "$mode" = strict ] && FLAG="--jdk-only"
  : > "$OUT/warns-$mode.txt"
  : > "$OUT/detail-$mode.txt"
  ran=0
  for c in $CLASSES; do
    ran=$((ran+1))
    timeout 120 "$CV" --java-home "$JDK" $FLAG -cp "$CP" "$c" > /dev/null 2>"$OUT/.err"
    grep -aoE "UNCLASSIFIED-NULL-BASE|NON-ARRAY-SCALE|minting synthetic offset|NULL-BASE-FALLBACK-REACHED|ARRAY-SCALE-ASKED" "$OUT/.err" \
      | sed "s/^/$c /" >> "$OUT/warns-$mode.txt"
    grep -aE "UNCLASSIFIED-NULL-BASE|NON-ARRAY-SCALE|minting synthetic offset|NULL-BASE-FALLBACK-REACHED|ARRAY-SCALE-ASKED" "$OUT/.err" \
      | sed "s/^/$c | /" >> "$OUT/detail-$mode.txt"
  done
  rm -f "$OUT/.err"
  echo "--- $mode: $ran vectors run"
  for k in NULL-BASE-FALLBACK-REACHED UNCLASSIFIED-NULL-BASE ARRAY-SCALE-ASKED NON-ARRAY-SCALE "minting synthetic offset"; do
    printf "    %-24s : %s hits in %s vectors\n" "$k" \
      "$(grep -c "$k" "$OUT/warns-$mode.txt")" \
      "$(grep "$k" "$OUT/warns-$mode.txt" | cut -d' ' -f1 | sort -u | wc -l)"
  done
done

for k in UNCLASSIFIED-NULL-BASE NON-ARRAY-SCALE; do
  echo "=== vectors hitting $k (strict)"
  grep "$k" "$OUT/warns-strict.txt" | cut -d' ' -f1 | sort | uniq -c | sort -rn | head -15
done
echo "=== a sample of the detail (strict)"
head -6 "$OUT/detail-strict.txt" | cut -c1-220
echo COUNT-DONE
