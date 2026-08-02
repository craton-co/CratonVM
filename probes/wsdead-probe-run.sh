#!/usr/bin/env bash
# usage: probe-run.sh <exe|jdk> <outfile> [dump_after_s] [hard_s]
# env: PROBE_COMBOS (e.g. "0"), SS_SAMPLE=1 to sample loopback socket queues
set -u
TC=/data/data/apps/tomcat
P=/data/data/wsdead-probes
CP="$P/classes:$(cat $TC/.suite/cp-linux-fixed.txt):/home/victor/tomcat-build-libs/hamcrest-3.0/hamcrest-3.0.jar"
CLS=org.apache.tomcat.websocket.server.TestWsDeadlockProbe
# Grouped spelling. The old per-flag CRATONVM_REAL_NET_SOCKETS=1 / CRATONVM_REAL_AQS=1 /
# CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 / CRATONVM_ROOTSNAP_CACHE=1 are no longer honored --
# a current binary prints "4 per-flag variable(s) set directly" and runs with NONE of them.
export CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog CRATONVM_JIT=rootsnap-cache
unset CRATONVM_REAL_NET_SOCKETS CRATONVM_REAL_AQS CRATONVM_DISABLE_DEFAULT_WATCHDOG CRATONVM_ROOTSNAP_CACHE
EXE="$1"; OUT="$2"; DUMP="${3:-0}"; HARD="${4:-300}"
COMBOS="${PROBE_COMBOS:-}"
PROPS="-Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true \
  -Dtomcat.test.basedir=$TC/output/build -Dtomcat.test.temp=$TC/output/test-tmp \
  -Dtomcat.test.tomcatbuild=$TC/output/build -Dtomcat.test.relaxTiming=true \
  -Dprobe.combos=$COMBOS -Dprobe.maxThreads=${PROBE_MAXTHREADS:-}"
OPENS="--add-opens java.base/java.lang=ALL-UNNAMED --add-opens java.base/java.io=ALL-UNNAMED \
  --add-opens java.base/java.util=ALL-UNNAMED --add-opens java.base/java.util.concurrent=ALL-UNNAMED"

SSPID=""
if [ "${SS_SAMPLE:-0}" = "1" ]; then
  ( while true; do
      echo "--- $(date +%s.%N)"
      ss -tni state established '( src 127.0.0.1 or dst 127.0.0.1 )' 2>/dev/null \
        | grep -A1 '127.0.0.1' | grep -E '127\.0\.0\.1|rcv_space' | head -60
      sleep 0.5
    done > "$OUT.ss" ) &
  SSPID=$!
fi

start=$(date -u +%H:%M:%S)
if [ "$(basename "$EXE")" = "java" ]; then
  timeout ${HARD}s "$EXE" -Xmx2g $PROPS $OPENS -cp "$CP" org.junit.runner.JUnitCore $CLS > "$OUT" 2>&1
else
  DUMPARG=""
  [ "$DUMP" != "0" ] && DUMPARG="--stack-dump-on-timeout=$DUMP"
  timeout ${HARD}s "$EXE" --java-home /home/victor/jdk25 --Xmx 2g $DUMPARG ${EXTRA_VM_ARGS:-} \
    $PROPS $OPENS -c "$CP" org.junit.runner.JUnitCore $CLS > "$OUT" 2>&1
fi
rc=$?
[ -n "$SSPID" ] && kill $SSPID 2>/dev/null
if grep -q '^OK (' "$OUT"; then r=PASS
elif grep -q 'Close delay was' "$OUT"; then r="CLOSE_DELAY($(grep -c 'Close delay was' "$OUT"))"
elif [ $rc -eq 124 ]; then r=TIMEOUT
elif [ $rc -eq 134 ]; then r=HANG_DUMPED
else r="FAIL(rc=$rc)"; fi
echo "$(basename $EXE) $r start=$start end=$(date -u +%H:%M:%S) out=$OUT"
