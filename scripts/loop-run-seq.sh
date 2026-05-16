#!/usr/bin/env bash
# Run all CratonVM test apps sequentially to avoid resource contention.
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

run_app letsgo-eureka 25 --Xmx 512m \
    --jar "$APPS/letsgo/eureka-server/target/eureka-server-0.0.1-SNAPSHOT.jar"
run_app insurance 25 --Xmx 1g \
    --jar "$APPS/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar"
run_app sportme 25 --Xmx 1g \
    --jar "$APPS/SportMe-master/target/sportme-backend.jar"
run_app demo 25 --Xmx 1g \
    --jar "$APPS/demo/target/demo-0.0.1-SNAPSHOT.jar"
run_app bc_probe 25 -c "$APPS/bc_probe;$APPS/ejbca-ce-main/lib/bcprov-jdk18on-1.80.2.jar" BcProbe
run_app cleaner_probe 25 -c "$APPS/cleaner_probe" CleanerProbe
run_app bytebuddy_probe 25 -c "$APPS/bytebuddy_probe;$APPS/bytebuddy_probe/lib/byte-buddy-1.14.18.jar" ByteBuddyProbe
run_app cglib_probe 25 -c "$APPS/cglib_probe;$APPS/cglib_probe/lib/cglib-3.3.0.jar;$APPS/cglib_probe/lib/asm-9.5.jar" CglibProbe
CP=$(find "$APPS/kafka_2.13-4.2.0/libs" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app kafka 25 --Xmx 512m -c "$CP" kafka.Kafka
run_app wildfly 25 --Xmx 512m \
    --jar "$APPS/wildfly-39.0.1.Final/jboss-modules.jar" -- \
    -mp "$APPS/wildfly-39.0.1.Final/modules" \
    org.jboss.as.standalone "-Djboss.home.dir=$APPS/wildfly-39.0.1.Final"
run_app keycloak 30 --Xmx 1g \
    --jar "$APPS/keycloak-26.2.4/lib/quarkus-run.jar" show-config

# Elasticsearch 8.15.5 — server CLI launcher.
ES="$APPS/elasticsearch-8.15.5"
ES_CP=$(find "$ES/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app elasticsearch 25 --Xmx 512m -c "$ES_CP" \
    "-Dcli.name=server" \
    "-Dcli.script=$ES/bin/elasticsearch" \
    "-Dcli.libs=lib/tools/server-cli" \
    "-Des.path.home=$ES" \
    "-Des.path.conf=$ES/config" \
    "-Des.distribution.type=default" \
    org.elasticsearch.launcher.CliToolLauncher

echo "=== SUMMARY iter=$ITER (sequential) ==="
for f in "$LOGDIR"/*.rc.txt; do
    name=$(basename "$f" .rc.txt)
    rc=$(cat "$f")
    last=$(tail -1 "$LOGDIR/$name.err.txt" 2>/dev/null | head -c 140)
    echo "$name rc=$rc | err-last: $last"
done
