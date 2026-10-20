#!/bin/bash
# Confirm the three collector-dependent rows are deterministic, not flakes.
#
# A single differing run is a sample, not a measurement: rerun the SAME binary
# N times per (class, collector) cell before attributing anything to the
# collector. Runs every differing class under every collector so the table has
# its own controls.
set -u

BIN=/data/vm-gcsweep-20260819.bin
JDK=/data/toolchain/jdk-25
REPS="${REPS:-3}"
source /data/toolchain/env.sh
cd /data/cratonvm/apps/bc-java
CP="$(cat /data/bcjca-classpath.txt)"

CLASSES="
org.bouncycastle.pqc.math.ntru.test.AllTests
org.bouncycastle.cert.ocsp.test.AllTests
org.bouncycastle.crypto.test.AllTests
"

printf "%-46s %-14s %s\n" CLASS COLLECTOR "RESULTS (${REPS} reps)"
for cls in $CLASSES; do
  for gc in zgc g1 generational; do
    case "$gc" in
      zgc)          FLAG="-XX:+UseZGC" ;;
      g1)           FLAG="-XX:+UseG1GC" ;;
      generational) FLAG="-XX:+UseGenerationalGC" ;;
    esac
    row=""
    for r in $(seq 1 "$REPS"); do
      timeout --kill-after=5 600 "$BIN" --java-home "$JDK" $FLAG --Xmx 1g \
        -Dbc.test.data.home=/data/cratonvm/apps/bc-test-data \
        -Dtest.java.version.prefix=25 \
        -c "$CP" junit.textui.TestRunner "$cls" > /tmp/confirm.log 2>&1
      rc=$?
      if [ "$rc" -eq 0 ]; then row="$row PASS"
      elif [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then row="$row HANG"
      else row="$row FAIL"; fi
    done
    printf "%-46s %-14s %s\n" "${cls#org.bouncycastle.}" "$gc" "$row"
  done
done
echo "confirm done $(date -Is)"
