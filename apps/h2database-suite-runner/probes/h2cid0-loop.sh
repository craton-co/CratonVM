#!/bin/bash
# h2cid0-loop.sh <worker-id> <binary> <iterations> [extra env assignments...]
# The doc's recipe with the two 10-thread rounds LOOPED (TestMVStoreCacheLoop).
set -u
W="$1"; BIN="$2"; ITERS="${3:-20}"
shift 3
H2=/data/data/h2database/h2
ROOT=/data/data/h2cid0-runs
WORK="$ROOT/L$W"
mkdir -p "$WORK"
cd "$WORK" || exit 1
CP="$ROOT/probe:$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"
for i in $(seq 1 "$ITERS"); do
  LOG="$WORK/iter$i.log"
  rm -rf "$WORK/data"
  start=$(date +%s)
  echo "=== L$W iter $i start $(date -Is) extra: $*" >> "$WORK/worker.log"
  env CRATONVM_NO_MOVING_YOUNG=1 TMPDIR=/data/tmp "$@" \
      "$BIN" --java-home /home/victor/jdk25 --Xmx 1g \
      -c "$CP" org.h2.test.store.TestMVStoreCacheLoop > "$LOG" 2>&1
  rc=$?
  end=$(date +%s)
  echo "=== L$W iter $i rc=$rc secs=$((end-start)) rounds=$(grep -c 'loop round' "$LOG")" >> "$WORK/worker.log"
  if grep -q "cannot be cast\|cratonvm::gc::guard" "$LOG"; then
    echo "!!! L$W iter $i REPRODUCED / GUARD FIRED" >> "$WORK/worker.log"
    cp "$LOG" "$ROOT/HIT-L$W-i$i.log"
    exit 0
  fi
  gzip -f "$LOG"
done
echo "=== L$W exhausted" >> "$WORK/worker.log"
