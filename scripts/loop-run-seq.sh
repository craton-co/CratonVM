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

# ── Batch 1: Maven, Gradle, ActiveMQ, Felix, TomEE, GlassFish ─────────────────
MVN_CP=$(find "$APPS/apache-maven-3.9.9/boot" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app maven 25 --Xmx 256m -c "$MVN_CP" \
    "-Dmaven.home=$APPS/apache-maven-3.9.9" \
    "-Dclassworlds.conf=$APPS/apache-maven-3.9.9/bin/m2.conf" \
    org.codehaus.plexus.classworlds.launcher.Launcher

GRADLE_CP=$(find "$APPS/gradle-8.10.2/lib" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
run_app gradle 25 --Xmx 256m -c "$GRADLE_CP" org.gradle.launcher.GradleMain

run_app activemq 25 --Xmx 256m \
    --jar "$APPS/apache-activemq-6.1.4/bin/activemq.jar"

run_app felix 25 --Xmx 256m \
    --jar "$APPS/felix-framework-7.0.5/bin/felix.jar"

TC="$APPS/apache-tomee-plus-9.1.3"
TCCP="$TC/bin/bootstrap.jar;$TC/bin/tomcat-juli.jar"
run_app tomee 25 --Xmx 512m -c "$TCCP" \
    "-Dcatalina.home=$TC" "-Dcatalina.base=$TC" \
    org.apache.catalina.startup.Bootstrap version

run_app glassfish 25 --Xmx 512m \
    --jar "$APPS/glassfish7/glassfish/modules/glassfish.jar"

echo "=== SUMMARY iter=$ITER ==="
for f in "$LOGDIR"/*.rc.txt; do
    name=$(basename "$f" .rc.txt)
    rc=$(cat "$f")
    last=$(tail -1 "$LOGDIR/$name.err.txt" 2>/dev/null | head -c 140)
    echo "$name rc=$rc | err-last: $last"
done
