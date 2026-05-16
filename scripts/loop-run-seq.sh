#!/usr/bin/env bash
# Run all CratonVM test apps sequentially.
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

# ── Carry-over blockers from prior rounds ────────────────────────────────────
run_app cglib_probe 25 \
    -c "$APPS/cglib_probe;$APPS/cglib_probe/lib/cglib-3.3.0.jar;$APPS/cglib_probe/lib/asm-9.5.jar" \
    CglibProbe

run_app wildfly 300 --Xmx 512m \
    --jar "$APPS/wildfly-39.0.1.Final/jboss-modules.jar" -- \
    -mp "$APPS/wildfly-39.0.1.Final/modules" \
    org.jboss.as.standalone "-Djboss.home.dir=$APPS/wildfly-39.0.1.Final"

run_app keycloak-16 25 --Xmx 512m \
    --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" -- \
    -mp "$APPS/keycloak-16.1.1/modules" \
    org.jboss.as.standalone "-Djboss.home.dir=$APPS/keycloak-16.1.1"

# ── BlueJ ────────────────────────────────────────────────────────────────────
run_app bluej 25 --Xmx 512m --jar "$APPS/BlueJ-540.jar"

# ── jEdit ────────────────────────────────────────────────────────────────────
run_app jedit 25 --Xmx 512m --jar "$APPS/jedit5.7.0install.jar"

# ── Arduino IDE 1.8.19 ───────────────────────────────────────────────────────
ACP=$(find "$APPS/arduino-1.8.19/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app arduino 25 --Xmx 512m -c "$ACP" processing.app.Base

# ── Apache Cassandra 4.1.4 ───────────────────────────────────────────────────
CASS="$APPS/apache-cassandra-4.1.4"
CASSCP=$(find "$CASS/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
CASSCP="$CASS/conf;$CASSCP"
run_app cassandra 25 --Xmx 1g -c "$CASSCP" \
    "-Dcassandra.config=file:///$CASS/conf/cassandra.yaml" \
    "-Dcassandra.storagedir=$CASS/data" \
    org.apache.cassandra.service.CassandraDaemon

# ── Apache Solr 9.4.1 ────────────────────────────────────────────────────────
SOLR="$APPS/solr-9.4.1"
SOLRCP=$(find "$SOLR/server" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app solr 25 --Xmx 512m -c "$SOLRCP" org.eclipse.jetty.start.Main --module=http

# ── Apache Lucene 8.11.3 (library — smoke via demo IndexFiles) ───────────────
LUCENE="$APPS/lucene-8.11.3"
LUCCP=$(find "$LUCENE" -maxdepth 2 -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app lucene 25 --Xmx 512m -c "$LUCCP" org.apache.lucene.demo.SearchFiles -help

# ── Neo4j 5.18.0 ─────────────────────────────────────────────────────────────
NEO="$APPS/neo4j-community-5.18.0"
NEOCP=$(find "$NEO/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
NEOCP="$NEO/conf;$NEOCP"
run_app neo4j 25 --Xmx 1g -c "$NEOCP" \
    "-Dneo4j.home=$NEO" \
    "-Dneo4j.conf.dir=$NEO/conf" \
    org.neo4j.server.CommunityEntryPoint

echo "=== SUMMARY iter=$ITER (sequential) ==="
for f in "$LOGDIR"/*.rc.txt; do
    name=$(basename "$f" .rc.txt)
    rc=$(cat "$f")
    last=$(tail -1 "$LOGDIR/$name.err.txt" 2>/dev/null | head -c 140)
    echo "$name rc=$rc | err-last: $last"
done
