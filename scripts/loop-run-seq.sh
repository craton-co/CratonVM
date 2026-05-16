#!/usr/bin/env bash
set +e
ITER="${1:-rseq}"
ROOT="C:/Projects/CratonVM/.claude/worktrees/infallible-solomon-1d423b"
LOGDIR="$ROOT/applogs/loop-$ITER"
mkdir -p "$LOGDIR"
RUSTJVM="$ROOT/target/release/rustjvm.exe"
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"
APPS="C:/Projects/cratonvm/apps"

run_app() {
    local name="$1"; shift
    local timeout_s="$1"; shift
    echo "===== running $name =====" >&2
    timeout "$timeout_s" "$RUSTJVM" --java-home "$JDK" "$@" \
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

echo "=== SUMMARY iter=$ITER ==="
for f in "$LOGDIR"/*.rc.txt; do
    name=$(basename "$f" .rc.txt)
    rc=$(cat "$f")
    last=$(tail -1 "$LOGDIR/$name.err.txt" 2>/dev/null | head -c 140)
    echo "$name rc=$rc | err-last: $last"
done
