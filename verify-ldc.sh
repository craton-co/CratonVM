#!/usr/bin/env bash
# One-binary A/B + correctness for the ldc constant cache.
# Arm order is REVERSED halfway: a single order cannot separate a cache win
# from warm-up.
set -u
BIN="$1"
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
P=probes
OFF=CRATONVM_JIT_NO_LDC_CONST_CACHE=1

echo "===== ENGAGEMENT (must be non-zero ON, zero OFF) ====="
echo "--- default (cache ON) ---"
CRATONVM_DBG_FIELD_SITE=1 "$BIN" --java-home "$JDK" --nojit -cp $P LdcConstCostProbe 40000 2>&1 \
  | grep -o "ldc: hit=[0-9]* miss=[0-9]* fill=[0-9]*" | tail -1
echo "--- kill switch (cache OFF) ---"
env $OFF CRATONVM_DBG_FIELD_SITE=1 "$BIN" --java-home "$JDK" --nojit -cp $P LdcConstCostProbe 40000 2>&1 \
  | grep -o "ldc: hit=[0-9]* miss=[0-9]* fill=[0-9]*" | tail -1

echo
echo "===== IDENTITY (every line must match HotSpot = all true) ====="
echo "--- cache ON ---"
"$BIN" --java-home "$JDK" --nojit -cp $P LdcStringIdentityProbe 2>&1
echo "--- cache OFF ---"
env $OFF "$BIN" --java-home "$JDK" --nojit -cp $P LdcStringIdentityProbe 2>&1
echo "--- JIT on ---"
"$BIN" --java-home "$JDK" -cp $P LdcStringIdentityProbe 2>&1

echo
echo "===== COST (6 rounds, arm order reversed at round 4) ====="
for r in 1 2 3 4 5 6; do
  if [ $r -le 3 ]; then
    echo "-- round $r: OFF first --"
    echo -n "OFF "; env $OFF "$BIN" --java-home "$JDK" --nojit -cp $P LdcConstCostProbe 2>&1 | grep -E "ldc-|iadd" | tr '\n' ' '; echo
    echo -n "ON  "; "$BIN" --java-home "$JDK" --nojit -cp $P LdcConstCostProbe 2>&1 | grep -E "ldc-|iadd" | tr '\n' ' '; echo
  else
    echo "-- round $r: ON first --"
    echo -n "ON  "; "$BIN" --java-home "$JDK" --nojit -cp $P LdcConstCostProbe 2>&1 | grep -E "ldc-|iadd" | tr '\n' ' '; echo
    echo -n "OFF "; env $OFF "$BIN" --java-home "$JDK" --nojit -cp $P LdcConstCostProbe 2>&1 | grep -E "ldc-|iadd" | tr '\n' ' '; echo
  fi
done
