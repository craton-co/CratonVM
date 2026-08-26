#!/bin/bash
# huntloop.sh <n-runs> <tag> — hunt the ClassId(0) receiver behind the stall.
#
# The stall's proximate cause is now known: the server's TLS handshake dies on
#   NoSuchMethodError 'void java.lang.Object.checkClientTrusted(X509Certificate[], String)'
# so no alert is produced, the client never sees an SSLException, and the
# test's promise is never completed. `java/lang/Object` as the receiver class
# is the tree's own `ClassId(0)` stale-receiver signature at INVOKE.
#
# `CRATONVM_DBG_CCE_BT=1` makes that dispatch miss dump the frame stack and the
# receiver's identity, which is what names the producing frame.
# `CRATONVM_DBG_VACATED_FRAMES=1` arms the complementary question — was this
# address one the LAST collection moved an object away from, i.e. a frame slot
# the remap did not reach. Both are terminal-path only, so a healthy run pays
# nothing.
#
# Runs until it catches one: the loop STOPS on the first hang so the evidence
# is the last thing in the directory.
set -u
N="$1"; TAG="$2"
EXE=${NRES_EXE:-/data/bin/cratonvm-nres2-20260824}
JDK=/data/toolchain/jdk-25
CLS=io.netty.handler.ssl.ParameterizedSslHandlerTest
D=/data/nres/$TAG; mkdir -p "$D"
SUM="$D/SUMMARY.txt"; : > "$SUM"
cd /data/cratonvm/apps/netty-suite-runner || { echo "GAVEUP no runner dir" >> "$SUM"; exit 2; }
export CRATONVM_DBG_CCE_BT=1
export CRATONVM_DBG_VACATED_FRAMES=1
# Names the mutator an STW takeover is waiting for. Printed only once a
# takeover has already been stuck 64 rounds, so a healthy run is silent.
export CRATONVM_DBG_STW_CENSUS=1
# Keeps the last 8 relocation pointer maps so a stale-ref capture can say
# whether the forward was RECORDED and the slot missed the remap, or never
# recorded at all. Debug-gated; the ring is empty without it.
export CRATONVM_DBG_GCPART=1
for i in $(seq 1 "$N"); do
  L="$D/run-$i.log"
  t0=$(date +%s); la=$(cut -d' ' -f1 /proc/loadavg)
  timeout -k 20 900 "$EXE" --java-home "$JDK" --Xmx 1500m @/data/nres/ossl.args \
      -XX:+UseG1GC -Djunit.jupiter.execution.timeout.mode=disabled \
      PerTestProgressRunner "$CLS" > "$L" 2>&1
  rc=$?
  wall=$(( $(date +%s) - t0 ))
  nsme=$(grep -ac "NoSuchMethodError" "$L")
  stw=$(grep -ac "still waiting for cooperative mutators" "$L")
  st=PASS; [ "$rc" -eq 97 ] && st=HANG; { [ "$rc" -ne 0 ] && [ "$rc" -ne 97 ]; } && st=OTHER
  printf "%-3s %-5s rc=%-4s wall=%-5s load0=%-7s nsme=%-3s stw=%s\n" \
     "$i" "$st" "$rc" "$wall" "$la" "$nsme" "$stw" >> "$SUM"
  if [ "$st" = HANG ] || [ "$nsme" -gt 0 ] || [ "$stw" -gt 0 ]; then
    echo "CAUGHT at run $i (rc=$rc nsme=$nsme stw=$stw) — stopping so the evidence stays put" >> "$SUM"
    break
  fi
done
echo "LOOPDONE $TAG" >> "$SUM"
