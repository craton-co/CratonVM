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

# ── Spring Boot probes & demos ───────────────────────────────────────────────
run_app demo 25 --Xmx 1g \
    --jar "$APPS/demo/target/demo-0.0.1-SNAPSHOT.jar"

# ── Native-lib reflection / proxy probes ─────────────────────────────────────
run_app bytebuddy_probe 25 \
    -c "$APPS/bytebuddy_probe;$APPS/bytebuddy_probe/lib/byte-buddy-1.14.18.jar" \
    ByteBuddyProbe
run_app cglib_probe 25 \
    -c "$APPS/cglib_probe;$APPS/cglib_probe/lib/cglib-3.3.0.jar;$APPS/cglib_probe/lib/asm-9.5.jar" \
    CglibProbe

# ── WildFly 39 (jboss-modules launcher) ──────────────────────────────────────
run_app wildfly 25 --Xmx 512m \
    --jar "$APPS/wildfly-39.0.1.Final/jboss-modules.jar" -- \
    -mp "$APPS/wildfly-39.0.1.Final/modules" \
    org.jboss.as.standalone "-Djboss.home.dir=$APPS/wildfly-39.0.1.Final"

# ── Keycloak 26 (Quarkus) ────────────────────────────────────────────────────
run_app keycloak 60 --Xmx 1g \
    --jar "$APPS/keycloak-26.2.4/lib/quarkus-run.jar" show-config

# ── Keycloak 16 (WildFly-based) ──────────────────────────────────────────────
run_app keycloak-16 25 --Xmx 512m \
    --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" -- \
    -mp "$APPS/keycloak-16.1.1/modules" \
    org.jboss.as.standalone "-Djboss.home.dir=$APPS/keycloak-16.1.1"

# ── Elasticsearch 8.15.5 ─────────────────────────────────────────────────────
ES="$APPS/elasticsearch-8.15.5"
ES_CP=$(find "$ES/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app elasticsearch 25 --Xmx 512m -c "$ES_CP" \
    "-Dcli.name=server" "-Dcli.script=$ES/bin/elasticsearch" \
    "-Dcli.libs=lib/tools/server-cli" "-Des.path.home=$ES" \
    "-Des.path.conf=$ES/config" "-Des.distribution.type=default" \
    org.elasticsearch.launcher.CliToolLauncher

# ── Jenkins LTS 2.452.3 (Winstone-launched WAR) ──────────────────────────────
run_app jenkins 25 --Xmx 512m \
    --jar "$APPS/jenkins.war"

echo "=== SUMMARY iter=$ITER (sequential) ==="
for f in "$LOGDIR"/*.rc.txt; do
    name=$(basename "$f" .rc.txt)
    rc=$(cat "$f")
    last=$(tail -1 "$LOGDIR/$name.err.txt" 2>/dev/null | head -c 140)
    echo "$name rc=$rc | err-last: $last"
done
