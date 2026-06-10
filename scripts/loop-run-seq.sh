#!/usr/bin/env bash
set +e
ITER="${1:-rseq}"
ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
LOGDIR="$ROOT/applogs/loop-$ITER"
mkdir -p "$LOGDIR"
CRATONVM="$ROOT/target/release/cratonvm.exe"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
APPS="${APPS:-$ROOT/apps}"

run_app() {
    local name="$1"; shift
    local timeout_s="$1"; shift
    echo "===== running $name =====" >&2
    timeout "$timeout_s" "$CRATONVM" --java-home "$JDK" "$@" \
        > "$LOGDIR/$name.out.txt" 2> "$LOGDIR/$name.err.txt"
    local rc=$?
    echo "$rc" > "$LOGDIR/$name.rc.txt"
    echo "$name rc=$rc"
}

# ── Batch 2: HBase, Ignite, Hazelcast, Spark, Flink ────────────────────────
HB="$APPS/hbase-2.5.10"
HBCP=$(find "$HB/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app hbase 25 --Xmx 512m -c "$HBCP" \
    "-Dhbase.home.dir=$HB" \
    org.apache.hadoop.hbase.util.VersionInfo

IG="$APPS/apache-ignite-2.16.0-bin"
IGCP=$(find "$IG/libs" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app ignite 25 --Xmx 512m -c "$IGCP" \
    "-DIGNITE_HOME=$IG" \
    org.apache.ignite.startup.cmdline.CommandLineStartup

run_app hazelcast 25 --Xmx 512m \
    --jar "$APPS/hazelcast.jar"

SP="$APPS/spark-3.5.1-bin-hadoop3"
SPCP=$(find "$SP/jars" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app spark 25 --Xmx 512m -c "$SPCP" \
    org.apache.spark.deploy.SparkSubmit

FL="$APPS/flink-1.18.1"
FLCP=$(find "$FL/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app flink 25 --Xmx 512m -c "$FLCP" \
    "-DFLINK_HOME=$FL" \
    org.apache.flink.client.cli.CliFrontend

# ── Batch 3: payara, eclipse, netbeans, hadoop, mindustry ───────────────
PA="$APPS/payara6"
PACP=$(find "$PA/glassfish/modules" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app payara 25 --Xmx 512m -c "$PACP" \
    "-Dcom.sun.aas.installRoot=$PA/glassfish" \
    com.sun.enterprise.glassfish.bootstrap.ASMain

EC="$APPS/eclipse"
ECLAUNCHER=$(ls "$EC/plugins"/org.eclipse.equinox.launcher_*.jar | head -1)
run_app eclipse 25 --Xmx 512m \
    --jar "$ECLAUNCHER"

NB="$APPS/netbeans"
# NetBeans launcher needs platform/lib (boot.jar w/ org.netbeans.Main +
# MainImpl + BootClassLoader) AND platform/core (core.jar w/ the default
# `netbeans.mainclass` = org.netbeans.core.startup.Main). Without core/
# the BootClassLoader.loadClass call throws ClassNotFoundException.
NBCP_LIB=$(find "$NB/platform/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
NBCP_CORE=$(find "$NB/platform/core" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
mkdir -p "$LOGDIR/nb-userdir"
# `netbeans.user` must be a writable dir or MainImpl falls back to
# parsing `--userdir` from argv (we have none, so it would NPE).
# `netbeans.dirs` is the cluster-dirs list — set it to platform so
# MainImpl.execute's build_cp picks up platform/core too.
run_app netbeans 25 --Xmx 512m -c "$NBCP_LIB;$NBCP_CORE" \
    "-Dnetbeans.home=$NB/platform" \
    "-Dnetbeans.dirs=$NB/platform" \
    "-Dnetbeans.user=$LOGDIR/nb-userdir" \
    org.netbeans.Main

HD="$APPS/hadoop-3.3.6"
HDCP=$(find "$HD/share/hadoop/common" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
HDCP2=$(find "$HD/share/hadoop/common/lib" -name "*.jar" 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app hadoop 25 --Xmx 512m -c "$HDCP;$HDCP2" \
    "-DHADOOP_HOME=$HD" \
    org.apache.hadoop.util.VersionInfo

run_app mindustry 25 --Xmx 512m \
    --jar "$APPS/mindustry.jar"

# ── Batch 4: Nexus, CAS, gRPC, RabbitMQ, JDownloader, FreeMind ───────────
B4="$APPS/batch4"

# Nexus Repository — Karaf-based launcher.
run_app nexus 30 --Xmx 512m -c "$B4/nexus-main.jar;$B4/karaf-main.jar" \
    org.sonatype.nexus.karaf.NexusMain

# Apereo CAS — Spring Boot command-line shell variant.
run_app cas 30 --Xmx 512m --jar "$B4/cas-shell.jar"

# gRPC Java — HelloWorld example server (canonical boot test).
run_app grpc 30 --Xmx 512m -c "$B4/grpc-examples.jar" \
    io.grpc.examples.helloworld.HelloWorldServer

# RabbitMQ — perf-test CLI fat-jar (canonical companion to amqp-client).
run_app rabbitmq 30 --Xmx 512m --jar "$B4/perf-test.jar"

# JDownloader — desktop fat-jar with Main-Class in MANIFEST.
run_app jdownloader 30 --Xmx 512m --jar "$B4/JDownloader.jar"

# FreeMind — mind-mapping desktop app.
FM="$B4/freemind_ext"
FMCP=$(find "$FM/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app freemind 30 --Xmx 512m -c "$FMCP" \
    "-Dfreemind.base.dir=$FM" \
    freemind.main.FreeMindStarter

echo "=== SUMMARY iter=$ITER ==="
for f in "$LOGDIR"/*.rc.txt; do
    name=$(basename "$f" .rc.txt)
    rc=$(cat "$f")
    last=$(tail -1 "$LOGDIR/$name.err.txt" 2>/dev/null | head -c 140)
    echo "$name rc=$rc | err-last: $last"
done
