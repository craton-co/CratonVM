#!/bin/bash
# hangloop.sh <n-runs> <tag> <mode> [craton|hotspot]
#
# The LOAD-PROOF stall loop.
#
# JUnit's own @Timeout is disabled, so a hang stays a hang; the VM watchdog is
# NOT armed, because a fixed wall-clock deadline on this host classifies a slow
# run as a stall (measured: two runs tripped 280 s and 420 s deadlines with the
# census reporting `waited_ms=6`, i.e. a healthy mid-test wait, while tests
# were still finishing every 30 s). Instead `PshProbe` halts the JVM with 97
# once ONE netty operation has been outstanding for two minutes, after printing
# that operation's state and every reactor's stack.
#
# So: rc=0 is a pass at any speed, rc=97 is a hang with its diagnosis in the
# log, and the 900 s `timeout` is only a backstop for a hang the probe does not
# cover.
set -u
N="$1"; TAG="$2"; MODE="$3"; VM="${4:-craton}"; shift 3
EXE=${NRES_EXE:-/data/bin/cratonvm-nres-20260824}
JDK=/data/toolchain/jdk-25
CLS=io.netty.handler.ssl.ParameterizedSslHandlerTest
SIG='(io.netty.handler.ssl.SslProvider, io.netty.handler.ssl.SslProvider)'
case "$MODE" in
  full)  RUNNER=(PerTestProgressRunner "$CLS") ;;
  comp)  RUNNER=(MethodProgressRunner "$CLS#testCompositeBufSizeEstimationGuaranteesSynchronousWrite$SIG") ;;
  alert) RUNNER=(MethodProgressRunner "$CLS#testAlertProducedAndSend$SIG") ;;
  *) echo "bad mode $MODE"; exit 2 ;;
esac
D=/data/nres/$TAG; mkdir -p "$D"
SUM="$D/SUMMARY.txt"; : > "$SUM"
cd /data/cratonvm/apps/netty-suite-runner || { echo "GAVEUP no runner dir" >> "$SUM"; exit 2; }
for i in $(seq 1 "$N"); do
  L="$D/run-$i.log"
  t0=$(date +%s); la=$(cut -d' ' -f1 /proc/loadavg)
  if [ "$VM" = hotspot ]; then
    timeout -k 20 900 "$JDK/bin/java" @${NRES_ARGS:-/data/nres/ossl.args} \
        -Djunit.jupiter.execution.timeout.mode=disabled "${RUNNER[@]}" > "$L" 2>&1
  else
    timeout -k 20 900 "$EXE" --java-home "$JDK" --Xmx 1500m @${NRES_ARGS:-/data/nres/ossl.args} \
        -XX:+UseG1GC -Djunit.jupiter.execution.timeout.mode=disabled "${RUNNER[@]}" > "$L" 2>&1
  fi
  rc=$?
  wall=$(( $(date +%s) - t0 ))
  st=PASS
  [ "$rc" -eq 97 ] && st=HANG
  [ "$rc" -ne 0 ] && [ "$rc" -ne 97 ] && st=OTHER
  stuck=$(grep -a "STUCK " "$L" | head -1 | sed 's/.*STUCK //' | cut -c1-120)
  printf "%-3s %-5s %-5s rc=%-4s wall=%-5s load0=%-7s %s\n" \
     "$i" "$VM" "$st" "$rc" "$wall" "$la" "${stuck:--}" >> "$SUM"
done
echo "LOOPDONE $TAG" >> "$SUM"
