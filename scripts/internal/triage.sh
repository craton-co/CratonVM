#!/usr/bin/env bash
# Triage: run every app, capture rc + first meaningful error line.
set +e
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APPS="$ROOT/apps"
J="$ROOT/target/release/cratonvm.exe"
TS=$(date +%H%M%S)
LOG="$ROOT/applogs/triage-$TS"
mkdir -p "$LOG"

run() {
  local name="$1"; local to="$2"; shift 2
  echo "=== $name ===" >&2
  timeout "$to" "$J" "$@" > "$LOG/$name.out" 2> "$LOG/$name.err"
  local rc=$?
  local err
  err=$(grep -avE '^\[cratonvm\]|^\[DBG\]|^\[2m|stack-dump|watchdog' "$LOG/$name.err" "$LOG/$name.out" 2>/dev/null \
        | grep -aiE 'exception|error|panic|caused by|not found|unsupported|NoClass|abort' | head -1 | head -c 200)
  echo "$name | rc=$rc | $err"
}

cp_of() { find "$1" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//'; }

# Maven Spring Boot apps
run demo 30 --Xmx 1g --jar "$APPS/demo/target/demo-0.0.1-SNAPSHOT.jar"
run insurance 30 --Xmx 1g --jar "$APPS/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar"

# Binary distros
run tomcat 30 --Xmx 512m -c "$APPS/apache-tomcat-10.1.31/bin/bootstrap.jar;$APPS/apache-tomcat-10.1.31/bin/tomcat-juli.jar" \
  "-Dcatalina.home=$APPS/apache-tomcat-10.1.31" "-Dcatalina.base=$APPS/apache-tomcat-10.1.31" org.apache.catalina.startup.Bootstrap

KFCP=$(cp_of "$APPS/kafka_2.13-3.7.0/libs")
run kafka 25 --Xmx 512m -c "$KFCP" kafka.Kafka

ASCP=$(cp_of "$APPS/apache-cassandra-4.1.4/lib")
run cassandra 30 --Xmx 512m -c "$ASCP" org.apache.cassandra.tools.NodeTool version

run activemq 30 --Xmx 512m -c "$APPS/apache-activemq-5.18.3/activemq-all-5.18.3.jar" org.apache.activemq.console.Main --version

run jetty 30 --Xmx 512m --jar "$APPS/jetty-home-11.0.20/start.jar" --list-config

run felix 20 --Xmx 256m --jar "$APPS/felix-framework-7.0.5/bin/felix.jar"

HZCP=$(cp_of "$APPS/hazelcast-5.4.0/lib")
run hazelcast 25 --Xmx 512m -c "$HZCP" com.hazelcast.core.server.HazelcastMemberStarter

# org.apache.solr.cli.SolrCLI lives in solr-core-*.jar under
# server/solr-webapp/webapp/WEB-INF/lib (NOT server/lib). This mirrors
# bin/solr.cmd's own classpath: WEB-INF/lib/* plus server/lib/ext/*.
SOCP="$(cp_of "$APPS/solr-9.5.0/server/solr-webapp/webapp/WEB-INF/lib");$(cp_of "$APPS/solr-9.5.0/server/lib/ext")"
run solr 25 --Xmx 512m -c "$SOCP" org.apache.solr.cli.SolrCLI version

run wildfly 40 --Xmx 512m --jar "$APPS/wildfly-32.0.1.Final/jboss-modules.jar" -mp "$APPS/wildfly-32.0.1.Final/modules" \
  org.jboss.as.standalone "-Djboss.home.dir=$APPS/wildfly-32.0.1.Final"

run keycloak16 40 --Xmx 512m --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" -mp "$APPS/keycloak-16.1.1/modules" \
  org.jboss.as.standalone "-Djboss.home.dir=$APPS/keycloak-16.1.1"

run keycloak26 40 --Xmx 1g --jar "$APPS/keycloak-26.2.4/lib/quarkus-run.jar" show-config

echo "=== triage logs: $LOG ==="
