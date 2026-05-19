#!/usr/bin/env bash
# Run every app in C:/craton/CratonVM/apps/ under the CUDA-enabled rustjvm
# (`cargo build --release -p rustjvm-cli --bin rustjvm --features gpu-driver`).
# Captures rc + first error line + last stdout/err line per app into ROLLUP.md.
set +e

ROOT=C:/craton/CratonVM/.claude/worktrees/angry-brown-38c5dc
APPS=C:/craton/CratonVM/apps
RJVM="$ROOT/target/release/rustjvm.exe"
JDK="C:/Program Files/Java/jdk-25"
CRATON_GPU="$ROOT/target/release/build/craton-gpu-c33a31d23e695e7c/out/classes"
LOG="$ROOT/applogs/all-apps-$(date +%H%M%S)"
mkdir -p "$LOG"

ROLLUP="$LOG/ROLLUP.md"
echo "# All-apps run under CUDA-enabled rustjvm" > "$ROLLUP"
echo "" >> "$ROLLUP"
echo "JVM: \`$RJVM\` (built with --features gpu-driver)" >> "$ROLLUP"
echo "JDK: \`$JDK\`" >> "$ROLLUP"
echo "Logs: \`$LOG/\`" >> "$ROLLUP"
echo "" >> "$ROLLUP"
echo "| App | rc | First error / Last line |" >> "$ROLLUP"
echo "|-----|----|--------------------------|" >> "$ROLLUP"

run() {
    local name="$1"; shift
    local timeout_s="$1"; shift
    echo "----- $name -----" >&2
    timeout "$timeout_s" "$RJVM" --java-home "$JDK" "$@" \
        > "$LOG/$name.out" 2> "$LOG/$name.err"
    local rc=$?
    local first_err
    first_err=$(grep -vE '^\[rustjvm\]|^\[2m2026-|^WARN |^\[GRES-DBG|^\[URLRES-DBG|^\[OSTR-DBG|^\[CCE-DBG' "$LOG/$name.err" 2>/dev/null | head -1 | head -c 160)
    if [ -z "$first_err" ]; then
        first_err=$(tail -1 "$LOG/$name.out" 2>/dev/null | head -c 160)
    fi
    echo "$name rc=$rc | ${first_err}" >&2
    # Escape pipe for markdown table
    local esc_err
    esc_err=$(printf '%s' "$first_err" | sed 's/|/\\|/g')
    echo "| $name | $rc | $esc_err |" >> "$ROLLUP"
}

# --- Simple probes (already-built classes, no deps) ---
( cd "$APPS/enumtest" && run enumtest 25 -c . EnumTest )

# slf4j needs slf4j-api on cp
SLF4J=$(find "$APPS/ejbca-ce/lib/ext" -name "slf4j-api*.jar" 2>/dev/null | head -1)
( cd "$APPS/slf4j" && run slf4j 25 -c ".;$SLF4J" LogTest )

( cd "$APPS/sig_probe" && run sig_probe 25 -c classes SigProbe )
( cd "$APPS/cipher_probe" && run cipher_probe 25 -c . CipherProbe )
( cd "$APPS/cleaner_probe" && run cleaner_probe 25 -c classes CleanerProbe )

# cglib_probe needs cglib jar (we don't have it locally — try anyway)
( cd "$APPS/cglib_probe" && run cglib_probe 25 -c "classes;payload" CglibProbe2 )

# netty needs netty jars (also missing)
( cd "$APPS/netty" && run netty 15 -c . NettyEchoTest )

# --- Big servers / launchers ---
( cd "$APPS/wildfly-32.0.1.Final" && run wildfly 30 --jar jboss-modules.jar -- -mp modules org.jboss.as.standalone -Djboss.home.dir=. -Djboss.server.base.dir=standalone )

( cd "$APPS/keycloak-16.1.1" && run keycloak16 30 --jar jboss-modules.jar -- -mp modules org.keycloak.Main )

( cd "$APPS/keycloak-26.2.4/lib" && run keycloak26 30 --jar quarkus-run.jar -- --help )

# --- Big data ---
HBCP=$(find "$APPS/hbase-2.5.10/lib" -name "*.jar" 2>/dev/null | tr '\n' ';' | sed 's/;$//')
run hbase 30 -c "$HBCP" "-Dhbase.home.dir=$APPS/hbase-2.5.10" org.apache.hadoop.hbase.util.VersionInfo

HDCP=$(find "$APPS/hadoop-3.3.6/share/hadoop/common" -name "*.jar" 2>/dev/null | tr '\n' ';' | sed 's/;$//')
HDCP2=$(find "$APPS/hadoop-3.3.6/share/hadoop/common/lib" -name "*.jar" 2>/dev/null | tr '\n' ';' | sed 's/;$//')
run hadoop 30 -c "$HDCP;$HDCP2" "-DHADOOP_HOME=$APPS/hadoop-3.3.6" org.apache.hadoop.util.VersionInfo

# --- Spring Boot fat-jars ---
if [ -f "$APPS/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar" ]; then
    ( cd "$APPS/insurance-backend" && run insurance 30 --jar target/insurance-0.0.1-SNAPSHOT.jar )
fi

# --- GPU benchmark apps ---
if [ -d "$APPS/gpu-bench/classes" ]; then
    run gpu_correctness 25 --gpu --print-gpu-decisions -c "$APPS/gpu-bench/classes;$CRATON_GPU" GpuCorrectness
    run gpu_simple 25 --gpu --print-gpu-decisions -c "$APPS/gpu-bench/classes;$CRATON_GPU" SimpleGpuTest
fi

# --- Maven-only apps (no built artifact present, just record) ---
for app in demo letsgo SportMe-master ejbca-ce; do
    if [ -d "$APPS/$app" ]; then
        echo "| $app | n/a | (skipped — no built artifact; needs mvn/gradle build) |" >> "$ROLLUP"
    fi
done

echo "" >> "$ROLLUP"
echo "## Summary" >> "$ROLLUP"
echo "" >> "$ROLLUP"
echo "Apps reporting rc=0:" >> "$ROLLUP"
grep -E '^\| [a-z_0-9]+ \| 0 \|' "$ROLLUP" | sed 's/^/  /' >> "$ROLLUP"

cat "$ROLLUP"
