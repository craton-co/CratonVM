#!/bin/bash
# Hunt the load-dependent SIGSEGV in RtEqualsProbe.
#   usage: stress_rtq.sh <binary> <rounds> <parallel> [extra env assignments...]
# Runs `parallel` probe processes per round under CPU load, and reports every
# non-zero exit with its captured stderr tail.
BIN=$1; ROUNDS=${2:-10}; PAR=${3:-8}; shift 3
EXTRA=("$@")
CP=/data/tmp/rtq/probe:/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite/classes:$(ls /data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite/lib/*.jar | tr '\n' ':')
OUT=/data/tmp/rtq/stress
rm -rf "$OUT"; mkdir -p "$OUT"; cd "$OUT" || exit 1

# CPU load: enough to make the scheduler preempt VM threads mid-compile.
HOGS=6
for i in $(seq 1 $HOGS); do
  ( while :; do :; done ) &
  echo $! >> "$OUT/hogs.pid"
done
trap 'while read -r p; do kill "$p" 2>/dev/null; done < "$OUT/hogs.pid"' EXIT

CRASHES=0
TOTAL=0
for r in $(seq 1 "$ROUNDS"); do
  pids=()
  for k in $(seq 1 "$PAR"); do
    ( env "${EXTRA[@]}" CRATONVM_JIT_THRESHOLD=1 timeout 900 \
        "$BIN" --java-home /data/data/jdk25-real -cp "$CP" RtEqualsProbe 3000 \
        > "$OUT/r$r-k$k.log" 2>&1; echo $? > "$OUT/r$r-k$k.rc" ) &
    pids+=($!)
  done
  for p in "${pids[@]}"; do wait "$p"; done
  for k in $(seq 1 "$PAR"); do
    TOTAL=$((TOTAL+1))
    rc=$(cat "$OUT/r$r-k$k.rc")
    if [ "$rc" != "0" ]; then
      CRASHES=$((CRASHES+1))
      echo "=== CRASH round=$r slot=$k rc=$rc ==="
      grep -a 'SIGSEGV\|fatal error\|r10=\|slot\[r10\]\|jit pc\|jit addr' "$OUT/r$r-k$k.log" | tail -8
    fi
  done
  echo "round $r done: crashes=$CRASHES / runs=$TOTAL"
done
echo "STRESS_TOTAL=$TOTAL STRESS_CRASHES=$CRASHES"
