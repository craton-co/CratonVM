#!/usr/bin/env bash
# Selector regression batch: the Tomcat classes that lean hardest on
# Selector/OP_WRITE readiness. Run it on the binary under test AND on the dev
# baseline -- the answer that matters is the diff, not the absolute pass list
# (several of these are red on HotSpot too in this fixture).
#
# usage: wsdead-netreg.sh <exe|/path/to/java> <tag>
set -u
TC=/data/data/apps/tomcat
P=/data/data/wsdead-probes
CP="$(cat $TC/.suite/cp-linux-fixed.txt):/home/victor/tomcat-build-libs/hamcrest-3.0/hamcrest-3.0.jar"
export CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog CRATONVM_JIT=rootsnap-cache
unset CRATONVM_REAL_NET_SOCKETS CRATONVM_REAL_AQS CRATONVM_DISABLE_DEFAULT_WATCHDOG CRATONVM_ROOTSNAP_CACHE
EXE=$1; TAG=$2
PROPS="-Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true \
  -Dtomcat.test.basedir=$TC/output/build -Dtomcat.test.temp=$TC/output/test-tmp \
  -Dtomcat.test.tomcatbuild=$TC/output/build -Dtomcat.test.relaxTiming=true"
for CLS in \
  org.apache.catalina.nonblocking.TestNonBlockingAPI \
  org.apache.coyote.TestIoTimeouts \
  org.apache.coyote.TestAbstractProtocol \
  org.apache.tomcat.util.net.TestXxxEndpoint \
  org.apache.catalina.connector.TestCoyoteAdapter \
  org.apache.catalina.startup.TestTomcat \
  org.apache.coyote.http2.TestHttp2Section_6_1 ; do
  short=$(echo "$CLS" | sed 's/.*\.//')
  out=$P/netreg-$TAG-$short.log
  if [ "$(basename "$EXE")" = "java" ]; then
    timeout 500s "$EXE" -Xmx2g $PROPS -cp "$CP" org.junit.runner.JUnitCore "$CLS" > "$out" 2>&1
  else
    timeout 500s "$EXE" --java-home /home/victor/jdk25 --Xmx 2g $PROPS -c "$CP" \
      org.junit.runner.JUnitCore "$CLS" > "$out" 2>&1
  fi
  rc=$?
  if grep -q '^OK (' "$out"; then
    r="PASS($(grep -oE 'OK \([0-9]+ test' "$out" | grep -oE '[0-9]+'))"
  elif [ $rc -eq 124 ]; then
    r=TIMEOUT
  else
    r="FAIL[$(grep -oE 'Tests run: [0-9]+,  Failures: [0-9]+(,  Errors: [0-9]+)?' "$out" | head -1)]"
  fi
  echo "$TAG $short $r"
done
