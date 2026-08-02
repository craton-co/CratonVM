#!/usr/bin/env bash
# WebSocket cluster regression check: usage wscluster2.sh <exe|/path/to/java> <tag> [reps]
# Includes the REAL TestWsRemoteEndpointImplServerDeadlock, not just the probe replica.
set -u
TC=/data/data/apps/tomcat
P=/data/data/wsdead-probes
CP="$(cat $TC/.suite/cp-linux-fixed.txt):/home/victor/tomcat-build-libs/hamcrest-3.0/hamcrest-3.0.jar"
# Grouped spelling. The old per-flag CRATONVM_REAL_NET_SOCKETS=1 / CRATONVM_REAL_AQS=1 /
# CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 / CRATONVM_ROOTSNAP_CACHE=1 are no longer honored.
export CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog CRATONVM_JIT=rootsnap-cache
unset CRATONVM_REAL_NET_SOCKETS CRATONVM_REAL_AQS CRATONVM_DISABLE_DEFAULT_WATCHDOG CRATONVM_ROOTSNAP_CACHE
EXE=$1; TAG=$2; REPS=${3:-1}
PROPS="-Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true \
  -Dtomcat.test.basedir=$TC/output/build -Dtomcat.test.temp=$TC/output/test-tmp \
  -Dtomcat.test.tomcatbuild=$TC/output/build -Dtomcat.test.relaxTiming=true"
for rep in $(seq 1 "$REPS"); do
for CLS in \
  org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock \
  org.apache.tomcat.websocket.TestWsPingPongMessages \
  org.apache.tomcat.websocket.pojo.TestEncodingDecoding \
  org.apache.tomcat.websocket.TestWsSessionSuspendResume \
  org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance \
  org.apache.tomcat.websocket.TestWsRemoteEndpointImplClient ; do
  short=$(echo "$CLS" | sed 's/.*\.//')
  out=$P/cl2-$TAG-$short-$rep.log
  if [ "$(basename "$EXE")" = "java" ]; then
    timeout 400s "$EXE" -Xmx2g $PROPS -cp "$CP" org.junit.runner.JUnitCore "$CLS" > "$out" 2>&1
  else
    timeout 400s "$EXE" --java-home /home/victor/jdk25 --Xmx 2g $PROPS \
      -c "$CP" org.junit.runner.JUnitCore "$CLS" > "$out" 2>&1
  fi
  rc=$?
  if grep -q '^OK (' "$out"; then
    r="PASS($(grep -oE 'OK \([0-9]+ test' "$out" | head -1 | grep -oE '[0-9]+'))"
  elif [ $rc -eq 124 ]; then
    r=TIMEOUT
  else
    r="FAIL[$(grep -oE 'Tests run: [0-9]+,  Failures: [0-9]+' "$out" | head -1)]"
  fi
  echo "$TAG rep$rep $short $r rejects=$(grep -c 'Executor rejected socket' "$out") closedelay=$(grep -c 'Close delay was' "$out")"
done
done
