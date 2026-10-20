#!/usr/bin/env bash
# dod-arms.sh <mode> [arms...]     mode = strict | compat | hotspot
#
# The roadmap §6 definition of done, as five arms over the three named
# workloads. One cratonvm process per arm, each writing its own
# `--jdk-only-report`, so a report is never shared between vectors.
#
#   sbsimple   a Spring Boot application            smoke-test-simple
#   tcssl      a servlet container serving HTTPS    smoke-test-tomcat-ssl
#   tcnetssl   the same, without Spring in the way  apache tomcat TestSsl
#   jdbc       a JDBC workload                      java.sql against H2
#   h2jdbc     the engine's own JDBC conformance    12 org.h2.test.jdbc classes
#
# `tcssl` and `tcnetssl` are both the HTTPS requirement on purpose: the first is
# the servlet container an application actually ships with, the second is the
# container's own TLS test class, which drives handshakes, client certs and
# renegotiation that no application-level fetch reaches.
#
# Paths are the azure-host-2 layout (`azureuser@20.80.105.49`, repo
# `/data/cratonvm`); override DOD_APPS to point elsewhere. Appends a terminal
# DODARMS-DONE marker on every exit path so a client-side poll ends on failure
# rather than extending.
set -u
[ -r /data/toolchain/env.sh ] && . /data/toolchain/env.sh

MODE="${1:-strict}"; shift || true
ARMS="${*:-sbsimple tcssl tcnetssl jdbc h2jdbc}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCREEN="$HERE/dodscreen-linux.sh"
CLASSES="${DOD_CLASSES:-$HERE/out}"
APPS="${DOD_APPS:-/data/cratonvm/apps}"
SB="$APPS/spring-boot/smoke-test"
H2="$APPS/h2database/h2"
TC="$APPS/tomcat"
WORK="${DOD_WORK:-/data/dod-work}"
export DOD_TIMEOUT="${DOD_TIMEOUT:-1800}"

mkdir -p "$WORK/h2" "$WORK/h2suite" "$TC/output/test-tmp"

for arm in $ARMS; do
  echo "=== ARM $arm mode=$MODE $(date -Is)"
  case "$arm" in
    sbsimple)
      # Full context refresh, auto-configuration, classpath scanning, CGLIB
      # configuration proxies, logback, and the runners. Returns from main.
      env DOD_CP_EXTRA="$CLASSES" DOD_CWD= DOD_JVM_ARGS= \
        bash "$SCREEN" sbsimple "$MODE" \
          "$SB/spring-boot-smoke-test-simple/build/cratonvm-test-cp.txt" \
          DodSpringApp smoketest.simple.SampleSimpleApplication ;;
    tcssl)
      # Embedded Tomcat with a JKS bundle on 8443, three real HTTPS requests,
      # then a clean context close. `/actuator/health` is 503 BY DESIGN: the
      # sample keystore's certificate is expired and the SSL health indicator
      # reports OUT_OF_SERVICE, which the upstream test also asserts.
      env DOD_CP_EXTRA="$CLASSES" DOD_CWD= DOD_JVM_ARGS= \
        bash "$SCREEN" tcssl "$MODE" \
          "$SB/spring-boot-smoke-test-tomcat-ssl/build/cratonvm-test-cp.txt" \
          DodSpringApp smoketest.tomcat.ssl.SampleTomcatSslApplication \
          --server.port="${DOD_SSL_PORT:-18443}" \
          "https://localhost:${DOD_SSL_PORT:-18443}/" \
          "https://localhost:${DOD_SSL_PORT:-18443}/actuator/info" \
          "https://localhost:${DOD_SSL_PORT:-18443}/actuator/health#503" ;;
    tcnetssl)
      # 21 tests. `testClientInitiatedRenegotiation[JSSE]` is expected RED in
      # both modes and on this VM only -- rustls implements no TLS 1.2
      # renegotiation, so the HandshakeCompletedEvent never fires. That is a
      # recorded design limit (known-issues/tomcat/
      # ssl-renegotiation-emulation-limits.md), not a --jdk-only defect.
      env DOD_CP_EXTRA="$CLASSES:$TC/.build-libs/hamcrest-3.0/hamcrest-3.0.jar" \
          DOD_CWD="$TC" \
          DOD_JVM_ARGS="-Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true -Dtomcat.test.basedir=$TC/output/build -Dtomcat.test.temp=$TC/output/test-tmp -Dtomcat.test.tomcatbuild=$TC/output/build -Dtomcat.test.relaxTiming=true --add-opens java.base/java.lang=ALL-UNNAMED --add-opens java.base/java.io=ALL-UNNAMED --add-opens java.base/java.util=ALL-UNNAMED --add-opens java.base/java.util.concurrent=ALL-UNNAMED" \
        bash "$SCREEN" tcnetssl "$MODE" "$TC/.suite/cp-linux-fixed.txt" \
          DodJUnitRunner org.apache.tomcat.util.net.TestSsl ;;
    jdbc)
      # `craton-testcp.txt` carries H2's DEPENDENCIES only; the engine and its
      # META-INF/services/java.sql.Driver live in target/classes, without which
      # every arm fails identically with "No suitable driver found" -- on
      # HotSpot too, which is how to tell it from a VM defect.
      env DOD_CP_EXTRA="$CLASSES:$H2/target/classes:$H2/target/test-classes" \
          DOD_CWD="$WORK/h2" DOD_JVM_ARGS= \
        bash "$SCREEN" jdbc "$MODE" "$H2/craton-testcp.txt" \
          DodJdbcWorkload "$WORK/h2" ;;
    h2jdbc)
      env DOD_CP_EXTRA="$CLASSES:$H2/target/classes:$H2/target/test-classes" \
          DOD_CWD="$WORK/h2suite" DOD_JVM_ARGS= \
        bash "$SCREEN" h2jdbc "$MODE" "$H2/craton-testcp.txt" DodH2JdbcSuite \
          org.h2.test.jdbc.TestPreparedStatement \
          org.h2.test.jdbc.TestResultSet \
          org.h2.test.jdbc.TestMetaData \
          org.h2.test.jdbc.TestStatement \
          org.h2.test.jdbc.TestBatchUpdates \
          org.h2.test.jdbc.TestConnection \
          org.h2.test.jdbc.TestUpdatableResultSet \
          org.h2.test.jdbc.TestTransactionIsolation \
          org.h2.test.jdbc.TestGetGeneratedKeys \
          org.h2.test.jdbc.TestLobApi \
          org.h2.test.jdbc.TestCallableStatement \
          org.h2.test.jdbcx.TestDataSource ;;
    *) echo "unknown arm $arm" ;;
  esac
done
echo "DODARMS-DONE mode=$MODE $(date -Is)"
