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
run_app wildfly 300 --Xmx 512m \
    --jar "$APPS/wildfly-39.0.1.Final/jboss-modules.jar" -- \
    -mp "$APPS/wildfly-39.0.1.Final/modules" \
    org.jboss.as.standalone "-Djboss.home.dir=$APPS/wildfly-39.0.1.Final"

run_app keycloak-16 25 --Xmx 512m \
    --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" -- \
    -mp "$APPS/keycloak-16.1.1/modules" \
    org.jboss.as.standalone "-Djboss.home.dir=$APPS/keycloak-16.1.1"

run_app bluej 25 --Xmx 512m --jar "$APPS/BlueJ-540.jar"

SOLR="$APPS/solr-9.4.1"
SOLRCP=$(find "$SOLR/server" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app solr 25 --Xmx 512m -c "$SOLRCP" org.apache.solr.cli.SolrCLI --help

echo "=== SUMMARY iter=$ITER (sequential) ==="
for f in "$LOGDIR"/*.rc.txt; do
    name=$(basename "$f" .rc.txt)
    rc=$(cat "$f")
    last=$(tail -1 "$LOGDIR/$name.err.txt" 2>/dev/null | head -c 140)
    echo "$name rc=$rc | err-last: $last"
done
