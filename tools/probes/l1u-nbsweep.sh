#!/usr/bin/env bash
# Does ANY regression vector still reach the null-base fallback?
#
# The three arms cannot answer this. `run.sh` captures each vector's output into
# a shell variable and passes it through `extract` before comparing, so VM
# chatter -- including this instrument's warns -- is discarded. The arm logs
# carry ZERO cratonvm WARN lines of any kind, which is the proof: reading "0
# UNCLASSIFIED warns" out of them would have been a mute instrument reported as
# a clean result.
#
# So drive the vectors directly against the suite's own already-compiled
# classes, with stderr kept. Vectors are run WITHOUT their per-vector extra
# flags and classpath entries, so some will fail; that is fine for this
# question but it IS a coverage caveat, so the denominator below reports how
# many actually ran to completion.
#
# NullBaseControl runs in the same loop as the POSITIVE CONTROL. If it does not
# warn, the capture is broken and every zero here is void.
set +e
ulimit -c 0
source /data/toolchain/env.sh
CV=/data/l1u-target/release/cratonvm
JDK=/data/toolchain/jdk-25
SUITE=/data/cvm-l1u-20260828/regression-suite
BUILD="$SUITE/build"
OUT=/data/l1u-nbsweep
rm -rf "$OUT"; mkdir -p "$OUT"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

ran=0; failed=0; hits=0; hitlist=""
for f in "$SUITE"/src/R*.java; do
  c=$(basename "$f" .java)
  timeout 90 "$CV" --jdk-only --java-home "$JDK" -cp "$BUILD" "$c" \
      > "$OUT/$c.out" 2> "$OUT/$c.err"
  rc=$?
  if [ "$rc" = 0 ]; then ran=$((ran+1)); else failed=$((failed+1)); fi
  n=$(sed 's/\x1b\[[0-9;]*m//g' "$OUT/$c.err" | grep -c UNCLASSIFIED-NULL-BASE)
  if [ "$n" != 0 ]; then hits=$((hits+1)); hitlist="$hitlist $c($n,rc=$rc)"; fi
done

# POSITIVE CONTROL, same loop, same capture.
timeout 90 "$CV" --jdk-only --java-home "$JDK" -cp /data/l1u-probes/out NullBaseControl \
    > "$OUT/CONTROL.out" 2> "$OUT/CONTROL.err"
ctl=$(sed 's/\x1b\[[0-9;]*m//g' "$OUT/CONTROL.err" | grep -c UNCLASSIFIED-NULL-BASE)

echo "vectors completed rc=0 : $ran"
echo "vectors non-zero rc    : $failed   (no per-vector flags/classpath here)"
echo "vectors hitting the null-base fallback: $hits"
echo "hits:$hitlist"
echo "POSITIVE CONTROL warns : $ctl   (must be > 0, else every zero above is void)"
echo NBSWEEP-DONE
