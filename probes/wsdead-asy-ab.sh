#!/usr/bin/env bash
# Focused interleaved A/B of TestAsyncMessagesPerformance only.
# usage: asy-ab.sh <rounds> <exeA> <tagA> <exeB> <tagB> [exeC tagC]
set -u
TC=/data/data/apps/tomcat
P=/data/data/wsdead-probes
CP="$(cat $TC/.suite/cp-linux-fixed.txt):/home/victor/tomcat-build-libs/hamcrest-3.0/hamcrest-3.0.jar"
export CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog CRATONVM_JIT=rootsnap-cache
unset CRATONVM_REAL_NET_SOCKETS CRATONVM_REAL_AQS CRATONVM_DISABLE_DEFAULT_WATCHDOG CRATONVM_ROOTSNAP_CACHE
CLS=org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance
PROPS="-Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true \
  -Dtomcat.test.basedir=$TC/output/build -Dtomcat.test.temp=$TC/output/test-tmp \
  -Dtomcat.test.tomcatbuild=$TC/output/build -Dtomcat.test.relaxTiming=true"
N=$1; shift
run() { # tag exe round
  local out=$P/asy-$1-$3.log
  if [ "$(basename "$2")" = "java" ]; then
    timeout 400s "$2" -Xmx2g $PROPS -cp "$CP" org.junit.runner.JUnitCore "$CLS" > "$out" 2>&1
  else
    timeout 400s "$2" --java-home /home/victor/jdk25 --Xmx 2g $PROPS -c "$CP" \
      org.junit.runner.JUnitCore "$CLS" > "$out" 2>&1
  fi
  local res=PASS
  grep -q '^OK (' "$out" || res=FAIL
  echo "$1 r$3 $res SEQ0=$(grep -ac SEQ0 "$out") SEQ1=$(grep -ac SEQ1 "$out") SEQ2=$(grep -ac SEQ2 "$out") time=$(grep -a '^Time:' "$out" | tr -d '\n') load=$(cut -d' ' -f1 /proc/loadavg)"
}
ARMS=("$@")
for r in $(seq 1 "$N"); do
  i=0
  while [ $i -lt ${#ARMS[@]} ]; do
    run "${ARMS[$((i+1))]}" "${ARMS[$i]}" "$r"
    i=$((i+2))
  done
done
