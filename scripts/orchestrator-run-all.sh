#!/usr/bin/env bash
# Orchestrator runner — exercises every app folder under apps/ with the CPU build
# of CratonVM and a real JDK, captures stdout/stderr per app, and prints a one-line
# triage line: "<name> | rc=N | first-error-line".
#
# Each app gets a short timeout. Daemons that never exit are still considered a
# pass if no error line is produced inside the window.

set +e

ROOT="C:/Projects/CratonVM"
RJVM="$ROOT/target/release/cratonvm.exe"
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"
APPS="$ROOT/apps"

STAMP="${1:-$(date +%Y%m%d-%H%M%S)}"
LOGDIR="$ROOT/applogs/orchestrator-$STAMP"
mkdir -p "$LOGDIR"

# `name` is used for the log filename + console label.
# Returns rc 124 on timeout, otherwise the program's exit code.
run() {
    local name="$1"; shift
    local timeout_s="$1"; shift
    echo "--- $name ---" >&2
    timeout --foreground -k 5 "$timeout_s" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 "$@" \
        > "$LOGDIR/$name.out" 2> "$LOGDIR/$name.err"
    local rc=$?
    # First non-noise line of stderr (skip ANSI/log preambles)
    local first_err
    first_err=$(grep -vE '^\[2m|^\[cratonvm\]|^\[rustjvm\]|short-circuited' "$LOGDIR/$name.err" \
        | head -1 | head -c 220)
    if [ -z "$first_err" ]; then
        first_err=$(head -1 "$LOGDIR/$name.err" | head -c 220)
    fi
    echo "$name | rc=$rc | $first_err"
}

cp_glob() {
    # cp_glob <dir1> [dir2 ...]  — collect all *.jar, normalize to Windows ; separator
    find "$@" -name '*.jar' 2>/dev/null \
        | sed 's|^/c|C:|' \
        | tr '\n' ';' \
        | sed 's/;$//'
}

# ----- Probes ----------------------------------------------------------------
# These first because they're tiny and self-contained.

if [ -f "$APPS/bytebuddy_probe/ByteBuddyProbe.class" ]; then
    BBCP="$APPS/bytebuddy_probe;$APPS/bytebuddy_probe/byte-buddy-1.14.18.jar"
    run bytebuddy_probe 30 -c "$BBCP" ByteBuddyProbe
else
    echo "bytebuddy_probe | rc=skip | no ByteBuddyProbe.class"
fi

if [ -f "$APPS/cglib_probe/CglibProbe.class" ]; then
    CGCP="$APPS/cglib_probe;$APPS/cglib_probe/cglib-3.3.0.jar;$APPS/cglib_probe/asm-9.5.jar"
    run cglib_probe 30 -c "$CGCP" CglibProbe
else
    echo "cglib_probe | rc=skip | no CglibProbe.class"
fi

# ----- Single-jar apps -------------------------------------------------------

[ -f "$APPS/jedit.jar"      ] && run jedit      15 --jar "$APPS/jedit.jar"
[ -f "$APPS/hazelcast.jar"  ] && run hazelcast  30 --jar "$APPS/hazelcast.jar"
[ -f "$APPS/mindustry.jar"  ] && run mindustry  15 --jar "$APPS/mindustry.jar"
[ -f "$APPS/jenkins.war"    ] && run jenkins    15 --jar "$APPS/jenkins.war" -- --version --enable-future-java

# ----- Server / distro layouts ----------------------------------------------

if [ -d "$APPS/jetty-home-11.0.20" ]; then
    run jetty 30 --jar "$APPS/jetty-home-11.0.20/start.jar" -- --list-config
fi

if [ -d "$APPS/wlp" ]; then
    run liberty 30 --jar "$APPS/wlp/bin/tools/ws-server.jar" -- --version
fi

if [ -d "$APPS/apache-activemq-5.18.3" ]; then
    AMCP=$(cp_glob "$APPS/apache-activemq-5.18.3/lib")
    [ -n "$AMCP" ] && run activemq 30 -c "$AMCP" org.apache.activemq.console.Main -- --version
fi

if [ -d "$APPS/apache-cassandra-4.1.4" ]; then
    ASCP=$(cp_glob "$APPS/apache-cassandra-4.1.4/lib")
    [ -n "$ASCP" ] && run cassandra 30 -c "$ASCP" org.apache.cassandra.tools.NodeTool -- version
fi

if [ -d "$APPS/apache-ignite-2.16.0-bin" ]; then
    IGCP=$(cp_glob "$APPS/apache-ignite-2.16.0-bin/libs")
    [ -n "$IGCP" ] && run ignite 30 -c "$IGCP" "-DIGNITE_HOME=$APPS/apache-ignite-2.16.0-bin" org.apache.ignite.startup.cmdline.CommandLineStartup -- --help
fi

if [ -d "$APPS/elasticsearch-8.15.5" ]; then
    ESCP=$(cp_glob "$APPS/elasticsearch-8.15.5/lib")
    run elasticsearch 30 -c "$ESCP" "-Dcli.name=server" "-Des.path.home=$APPS/elasticsearch-8.15.5" "-Des.path.conf=$APPS/elasticsearch-8.15.5/config" org.elasticsearch.launcher.CliToolLauncher -- --version
fi

if [ -d "$APPS/flink-1.18.1" ]; then
    FLCP=$(cp_glob "$APPS/flink-1.18.1/lib")
    [ -n "$FLCP" ] && run flink 30 -c "$FLCP" "-DFLINK_HOME=$APPS/flink-1.18.1" org.apache.flink.client.cli.CliFrontend -- --help
fi

if [ -d "$APPS/spark-3.5.1-bin-hadoop3" ]; then
    SPCP=$(cp_glob "$APPS/spark-3.5.1-bin-hadoop3/jars")
    [ -n "$SPCP" ] && run spark 30 -c "$SPCP" org.apache.spark.deploy.SparkSubmit -- --version
fi

if [ -d "$APPS/kafka_2.13-3.6.1" ]; then
    KFCP=$(cp_glob "$APPS/kafka_2.13-3.6.1/libs")
    [ -n "$KFCP" ] && run kafka 15 -c "$KFCP" kafka.Kafka
fi

if [ -d "$APPS/gradle-8.10.2" ]; then
    GRCP=$(cp_glob "$APPS/gradle-8.10.2/lib")
    [ -n "$GRCP" ] && run gradle 30 -c "$GRCP" org.gradle.launcher.GradleMain -- --version
fi

if [ -d "$APPS/solr-9.5.0" ]; then
    SOCP=$(cp_glob "$APPS/solr-9.5.0/server/solr-webapp/webapp/WEB-INF/lib" "$APPS/solr-9.5.0/server/lib/ext")
    [ -n "$SOCP" ] && run solr 30 -c "$SOCP" org.apache.solr.cli.SolrCLI -- version
fi

if [ -d "$APPS/neo4j-community-5.18.1" ]; then
    NJCP=$(cp_glob "$APPS/neo4j-community-5.18.1/lib")
    [ -n "$NJCP" ] && run neo4j 30 -c "$NJCP" org.neo4j.server.startup.Neo4jBoot -- version
fi

if [ -d "$APPS/felix-framework-7.0.5" ]; then
    run felix 15 --jar "$APPS/felix-framework-7.0.5/bin/felix.jar"
fi

if [ -d "$APPS/wildfly-39.0.1.Final" ]; then
    run wildfly 45 --jar "$APPS/wildfly-39.0.1.Final/jboss-modules.jar" -- -mp "$APPS/wildfly-39.0.1.Final/modules" org.jboss.as.standalone "-Djboss.home.dir=$APPS/wildfly-39.0.1.Final" --version
fi

if [ -d "$APPS/payara6" ]; then
    PACP=$(cp_glob "$APPS/payara6/glassfish/modules")
    [ -n "$PACP" ] && run payara 30 -c "$PACP" "-Dcom.sun.aas.installRoot=$APPS/payara6/glassfish" com.sun.enterprise.glassfish.bootstrap.ASMain -- --version
fi

if [ -d "$APPS/keycloak-16.1.1" ]; then
    run kc16 45 --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" -- -mp "$APPS/keycloak-16.1.1/modules" org.jboss.as.standalone "-Djboss.home.dir=$APPS/keycloak-16.1.1" --version
fi

if [ -d "$APPS/keycloak-26.2.4" ]; then
    run kc26 45 --jar "$APPS/keycloak-26.2.4/lib/quarkus-run.jar" -- show-config
fi

# ----- batch4: misc jars ----------------------------------------------------
if [ -d "$APPS/batch4" ]; then
    [ -f "$APPS/batch4/cas-shell.jar"      ] && run batch4_cas        30 --jar "$APPS/batch4/cas-shell.jar" -- --help
    [ -f "$APPS/batch4/grpc-examples.jar"  ] && run batch4_grpc       15 -c "$APPS/batch4/grpc-examples.jar" io.grpc.examples.helloworld.HelloWorldServer
    [ -f "$APPS/batch4/perf-test.jar"      ] && run batch4_rabbitmq   15 --jar "$APPS/batch4/perf-test.jar" -- --help
    [ -f "$APPS/batch4/JDownloader.jar"    ] && run batch4_jdownloader 15 --jar "$APPS/batch4/JDownloader.jar" -- -h
    if [ -d "$APPS/batch4/freemind_ext/lib" ]; then
        FMCP=$(cp_glob "$APPS/batch4/freemind_ext/lib")
        [ -n "$FMCP" ] && run batch4_freemind 15 -c "$FMCP" "-Dfreemind.base.dir=$APPS/batch4/freemind_ext" freemind.main.FreeMindStarter
    fi
    if [ -f "$APPS/batch4/nexus-main.jar" ] && [ -f "$APPS/batch4/karaf-main.jar" ]; then
        run batch4_nexus 15 -c "$APPS/batch4/nexus-main.jar;$APPS/batch4/karaf-main.jar;$APPS/batch4/osgi-core.jar" org.sonatype.nexus.karaf.NexusMain
    fi
fi

echo "=== logs in $LOGDIR ==="
