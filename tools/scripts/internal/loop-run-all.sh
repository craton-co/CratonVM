#!/usr/bin/env bash
# Run all CratonVM test apps and capture errors. Iteration label as $1 (e.g. r1, r2).
set +e

ITER="${1:-r1}"
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

# All apps in parallel-ish batches. Limit parallelism to ~4 to avoid resource pressure.
run_app letsgo-eureka 25 --Xmx 512m \
    --jar "$APPS/letsgo/eureka-server/target/eureka-server-0.0.1-SNAPSHOT.jar" &
P1=$!

run_app insurance 25 --Xmx 1g \
    --jar "$APPS/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar" &
P2=$!

run_app sportme 25 --Xmx 1g \
    --jar "$APPS/SportMe-master/target/sportme-backend.jar" &
P3=$!

run_app demo 25 --Xmx 1g \
    --jar "$APPS/demo/target/demo-0.0.1-SNAPSHOT.jar" &
P4=$!

wait $P1 $P2 $P3 $P4

# Second batch: probes
run_app bc_probe 25 -c "$APPS/bc_probe;$APPS/ejbca-ce-main/lib/bcprov-jdk18on-1.80.2.jar" BcProbe &
P1=$!

run_app cleaner_probe 25 -c "$APPS/cleaner_probe" CleanerProbe &
P2=$!

run_app bytebuddy_probe 25 -c "$APPS/bytebuddy_probe;$APPS/bytebuddy_probe/lib/byte-buddy-1.14.18.jar" ByteBuddyProbe &
P3=$!

run_app cglib_probe 25 -c "$APPS/cglib_probe;$APPS/cglib_probe/lib/cglib-3.3.0.jar;$APPS/cglib_probe/lib/asm-9.5.jar" CglibProbe &
P4=$!

wait $P1 $P2 $P3 $P4

# Third batch: big apps
CP=$(find "$APPS/kafka_2.13-4.2.0/libs" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app kafka 25 --Xmx 512m -c "$CP" kafka.Kafka &
P1=$!

run_app wildfly 25 --Xmx 512m \
    --jar "$APPS/wildfly-39.0.1.Final/jboss-modules.jar" -- \
    -mp "$APPS/wildfly-39.0.1.Final/modules" \
    org.jboss.as.standalone "-Djboss.home.dir=$APPS/wildfly-39.0.1.Final" &
P2=$!

run_app keycloak 30 --Xmx 1g \
    --jar "$APPS/keycloak-26.2.4/lib/quarkus-run.jar" show-config &
P3=$!

# Elasticsearch 8.15.5 — server CLI launcher
ES="$APPS/elasticsearch-8.15.5"
ES_CP=$(find "$ES/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app elasticsearch 25 --Xmx 512m -c "$ES_CP" \
    "-Dcli.name=server" \
    "-Dcli.script=$ES/bin/elasticsearch" \
    "-Dcli.libs=lib/tools/server-cli" \
    "-Des.path.home=$ES" \
    "-Des.path.conf=$ES/config" \
    "-Des.distribution.type=default" \
    org.elasticsearch.launcher.CliToolLauncher &
P4=$!

wait $P1 $P2 $P3 $P4

echo "=== SUMMARY iter=$ITER ==="
for f in "$LOGDIR"/*.rc.txt; do
    name=$(basename "$f" .rc.txt)
    rc=$(cat "$f")
    last=$(tail -1 "$LOGDIR/$name.err.txt" 2>/dev/null | head -c 140)
    echo "$name rc=$rc | err-last: $last"
done
