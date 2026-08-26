#!/bin/bash
# fixab.sh <pairs> <tag> — INTERLEAVED unfixed-vs-fixed on the pin fix.
#
# The defect is `NoSuchMethodError java/lang/Object.<m>` from a receiver the
# collector moved out from under `native_properties_equals`. It caught once in
# 33 whole-class runs at a 600m heap (and once in ~230 at 1500m), so the rate
# tracks evacuation count and 600m is the arm to measure in.
#
# Two binaries, alternating run by run so both see the same host load — this
# box runs other agents and its load average has swung 8..148 today, which is
# enough on its own to move this rate. A sequential before/after here would be
# comparing two machines.
#
#   PRE  = /data/bin/cratonvm-nres3-20260826 … the binary that caught
#   POST = /data/bin/cratonvm-nres4-20260826 … the same tree + the pin fix
#
# Neither arm stops on a catch: the point is a RATE in both, not a single
# event. `nsme` is the count of `NoSuchMethodError` lines, which is the defect
# itself — a run can carry one and still pass, and those count.
set -u
PAIRS="$1"; TAG="$2"
PRE=${FIXAB_PRE:-/data/bin/cratonvm-nres3-20260824}
POST=${FIXAB_POST:-/data/bin/cratonvm-nres4-20260826}
JDK=/data/toolchain/jdk-25
XMX=${FIXAB_XMX:-600m}
CLS=io.netty.handler.ssl.ParameterizedSslHandlerTest
D=/data/nres/$TAG; mkdir -p "$D"
SUM="$D/SUMMARY.txt"; : > "$SUM"
cd /data/cratonvm/apps/netty-suite-runner || { echo "GAVEUP no runner dir" >> "$SUM"; exit 2; }
export CRATONVM_DBG_CCE_BT=1
export CRATONVM_DBG_VACATED_FRAMES=1
export CRATONVM_DBG_GCPART=1
one() { # $1=arm $2=idx
  local exe="$PRE"; [ "$1" = POST ] && exe="$POST"
  local L="$D/$1-$2.log" t0 rc wall la
  t0=$(date +%s); la=$(cut -d' ' -f1 /proc/loadavg)
  timeout -k 20 900 "$exe" --java-home "$JDK" --Xmx "$XMX" @/data/nres/ossl.args \
      -XX:+UseG1GC -Djunit.jupiter.execution.timeout.mode=disabled \
      PerTestProgressRunner "$CLS" > "$L" 2>&1
  rc=$?; wall=$(( $(date +%s) - t0 ))
  local nsme vac st
  nsme=$(grep -ac "NoSuchMethodError" "$L")
  vac=$(grep -ac "was_vacated" "$L")
  st=PASS; [ "$rc" -eq 97 ] && st=HANG; { [ "$rc" -ne 0 ] && [ "$rc" -ne 97 ]; } && st=OTHER
  printf "%-4s %-3s %-5s rc=%-4s wall=%-5s load0=%-7s nsme=%-3s vacated=%s\n" \
     "$1" "$2" "$st" "$rc" "$wall" "$la" "$nsme" "$vac" >> "$SUM"
}
for i in $(seq 1 "$PAIRS"); do
  if [ $(( i % 2 )) -eq 1 ]; then one PRE "$i"; one POST "$i"; else one POST "$i"; one PRE "$i"; fi
done
echo "LOOPDONE $TAG" >> "$SUM"
