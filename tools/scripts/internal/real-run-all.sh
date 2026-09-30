#!/usr/bin/env bash
# Run every shimmed app with the synthetic stubs dropped, so calls land on real
# bytecode (or a clear NoSuchMethodError) instead of a fake. Capture the first
# error line for each.
#
# This script used to export a per-app `CRATONVM_<APP>_REAL=1` for each of the
# 34 apps below. **Nothing in the VM ever read any of those variables** — the
# flag census (flag-census.md section 2) lists all 34 as dead —
# so every run here was a plain run and the results said nothing about the real
# path. The `run` helper's `"${env_var}=1" && export "${env_var}=1"` line was
# also a no-op: it tried to *execute* `CRATONVM_HBASE_REAL=1` as a command,
# which failed, and `&&` then skipped the export.
#
# The switch that actually exists is `CRATONVM_REAL=-stubs` (formerly
# `CRATONVM_NO_STUBS`): drop every SyntheticStub native at registration. That is
# what this script means, and it is now what it does — so expect these runs to
# fail in places the old ones "passed".
set +e
ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
LOGDIR="$ROOT/applogs/real-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$LOGDIR"
RJVM="$ROOT/target/release/cratonvm.exe"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
APPS="${APPS:-$ROOT/apps}"

run() {
    local name="$1"; shift
    local timeout_s="$1"; shift
    env CRATONVM_REAL=-stubs timeout "$timeout_s" "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 "$@" \
        > "$LOGDIR/$name.out" 2> "$LOGDIR/$name.err"
    local rc=$?
    local first_err=$(grep -vE '^\[2m|^\[cratonvm\]|short-circuited' "$LOGDIR/$name.err" | head -1 | head -c 180)
    echo "$name | rc=$rc | $first_err"
}

# Batch 2 (on-disk)
HBCP=$(find "$APPS/hbase-2.5.10/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run hbase 30 -c "$HBCP" "-Dhbase.home.dir=$APPS/hbase-2.5.10" org.apache.hadoop.hbase.util.VersionInfo

IGCP=$(find "$APPS/apache-ignite-2.16.0-bin/libs" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run ignite 30 -c "$IGCP" "-DIGNITE_HOME=$APPS/apache-ignite-2.16.0-bin" org.apache.ignite.startup.cmdline.CommandLineStartup -- --help

run hazelcast 30 --jar "$APPS/hazelcast.jar"

SPCP=$(find "$APPS/spark-3.5.1-bin-hadoop3/jars" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run spark 30 -c "$SPCP" org.apache.spark.deploy.SparkSubmit -- --version

FLCP=$(find "$APPS/flink-1.18.1/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run flink 30 -c "$FLCP" "-DFLINK_HOME=$APPS/flink-1.18.1" org.apache.flink.client.cli.CliFrontend -- --help

# Batch 3
PACP=$(find "$APPS/payara6/glassfish/modules" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run payara 30 -c "$PACP" "-Dcom.sun.aas.installRoot=$APPS/payara6/glassfish" com.sun.enterprise.glassfish.bootstrap.ASMain -- --version

ECLAUNCHER=$(ls "$APPS/eclipse/plugins"/org.eclipse.equinox.launcher_*.jar | head -1)
run eclipse 30 --jar "$ECLAUNCHER" -- -application none

NBCP=$(find "$APPS/netbeans/platform/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run netbeans 30 -c "$NBCP" "-Dnetbeans.home=$APPS/netbeans/platform" org.netbeans.Main -- --help

HDCP=$(find "$APPS/hadoop-3.3.6/share/hadoop/common" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
HDCP2=$(find "$APPS/hadoop-3.3.6/share/hadoop/common/lib" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run hadoop 30 -c "$HDCP;$HDCP2" "-DHADOOP_HOME=$APPS/hadoop-3.3.6" org.apache.hadoop.util.VersionInfo

run mindustry 15 --jar "$APPS/mindustry.jar"

# Batch 4
run nexus 15 -c "$APPS/batch4/nexus-main.jar;$APPS/batch4/karaf-main.jar" org.sonatype.nexus.karaf.NexusMain
run cas 30 --jar "$APPS/batch4/cas-shell.jar" -- --help
run grpc 15 -c "$APPS/batch4/grpc-examples.jar" io.grpc.examples.helloworld.HelloWorldServer
run rabbitmq 15 --jar "$APPS/batch4/perf-test.jar" -- --help
run jdownloader 15 --jar "$APPS/batch4/JDownloader.jar" -- -h

FMCP=$(find "$APPS/batch4/freemind_ext/lib" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run freemind 15 -c "$FMCP" "-Dfreemind.base.dir=$APPS/batch4/freemind_ext" freemind.main.FreeMindStarter

# Wave 2 downloads
ASCP=$(find "$APPS/apache-cassandra-4.1.4/lib" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
[ -n "$ASCP" ] && run cassandra 30 -c "$ASCP" org.apache.cassandra.tools.NodeTool -- version

NJCP=$(find "$APPS/neo4j-community-5.18.1/lib" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
[ -n "$NJCP" ] && run neo4j 30 -c "$NJCP" org.neo4j.server.startup.Neo4jBoot -- version

# SolrCLI lives in solr-core-*.jar under server/solr-webapp/webapp/WEB-INF/lib;
# match bin/solr.cmd's classpath (WEB-INF/lib/* + server/lib/ext/*) rather than
# globbing every jar in the distro (which drags in 300+ optional module jars).
SOCP=$(find "$APPS/solr-9.5.0/server/solr-webapp/webapp/WEB-INF/lib" "$APPS/solr-9.5.0/server/lib/ext" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
[ -n "$SOCP" ] && run solr 30 -c "$SOCP" org.apache.solr.cli.SolrCLI -- version

JTHOME=$APPS/jetty-home-11.0.20
[ -d "$JTHOME" ] && run jetty 30 --jar "$JTHOME/start.jar" -- --list-config

WLP=$APPS/wlp
[ -d "$WLP" ] && run liberty 30 --jar "$WLP/bin/tools/ws-server.jar" -- --version

SQHOME=$(ls -d "$APPS"/sonarqube-* 2>/dev/null | head -1)
[ -n "$SQHOME" ] && run sonar 30 --jar "$SQHOME"/lib/sonar-application-*.jar -- --help

[ -f "$APPS/jenkins.war" ] && run jenkins 15 --jar "$APPS/jenkins.war" -- --version

GRCP=$(find "$APPS/gradle-8.10.2/lib" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
[ -n "$GRCP" ] && run gradle 30 -c "$GRCP" org.gradle.launcher.GradleMain -- --version

AMCP=$(find "$APPS/apache-activemq-5.18.3/lib" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
[ -n "$AMCP" ] && run activemq 30 -c "$AMCP" org.apache.activemq.console.Main -- --version

[ -d "$APPS/felix-framework-7.0.5" ] && run felix 15 --jar "$APPS/felix-framework-7.0.5/bin/felix.jar"

[ -d "$APPS/wildfly-39.0.1.Final" ] && run wildfly 60 --jar "$APPS/wildfly-39.0.1.Final/jboss-modules.jar" -- -mp "$APPS/wildfly-39.0.1.Final/modules" org.jboss.as.standalone "-Djboss.home.dir=$APPS/wildfly-39.0.1.Final" --version

[ -d "$APPS/keycloak-16.1.1" ] && run kc16 60 --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" -- -mp "$APPS/keycloak-16.1.1/modules" org.jboss.as.standalone "-Djboss.home.dir=$APPS/keycloak-16.1.1" --version

[ -d "$APPS/keycloak-26.2.4" ] && run kc26 60 --jar "$APPS/keycloak-26.2.4/lib/quarkus-run.jar" -- show-config

ESHOME=$(ls -d "$APPS"/elasticsearch-* 2>/dev/null | head -1)
if [ -n "$ESHOME" ]; then
    ESCP=$(find "$ESHOME/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
    run es 30 -c "$ESCP" "-Dcli.name=server" "-Des.path.home=$ESHOME" "-Des.path.conf=$ESHOME/config" org.elasticsearch.launcher.CliToolLauncher -- --version
fi

KFHOME=$(ls -d "$APPS"/kafka_* 2>/dev/null | head -1)
if [ -n "$KFHOME" ]; then
    KFCP=$(find "$KFHOME/libs" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
    run kafka 15 -c "$KFCP" kafka.Kafka
fi

# Probes (if probe sources are present)
if [ -f "$APPS/bytebuddy_probe/ByteBuddyProbe.class" ]; then
    BBCP="$APPS/bytebuddy_probe;$APPS/bytebuddy_probe/byte-buddy-1.14.18.jar"
    run bytebuddy_probe 15 -c "$BBCP" ByteBuddyProbe
fi
if [ -f "$APPS/cglib_probe/CglibProbe.class" ]; then
    CGCP="$APPS/cglib_probe;$APPS/cglib_probe/cglib-3.3.0.jar;$APPS/cglib_probe/asm-9.5.jar"
    run cglib_probe 15 -c "$CGCP" CglibProbe
fi
if [ -f "$APPS/jedit.jar" ]; then
    run jedit 10 --jar "$APPS/jedit.jar"
fi

echo "=== logs in $LOGDIR ==="
