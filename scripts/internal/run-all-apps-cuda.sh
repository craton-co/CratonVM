#!/usr/bin/env bash
# Run every app in C:/craton/CratonVM/apps/ under the CUDA-enabled cratonvm
# (`cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver`)
# IN GPU MODE ONLY (--gpu --print-gpu-decisions). Captures rc + first error
# line per app into ROLLUP.md.
set +e

ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
APPS="${APPS:-$ROOT/apps}"
RJVM="$ROOT/target/release/cratonvm.exe"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
CRATON_GPU=$(ls -d "$ROOT/target/release/build/craton-gpu-"*/out/classes 2>/dev/null | head -1)
M2="$HOME/.m2/repository"
LOG="$ROOT/applogs/all-apps-gpu-$(date +%H%M%S)"
mkdir -p "$LOG"

ROLLUP="$LOG/ROLLUP.md"
echo "# All-apps run under CUDA-enabled cratonvm (GPU mode only)" > "$ROLLUP"
echo "" >> "$ROLLUP"
echo "JVM: \`$RJVM\` (--features gpu-driver)" >> "$ROLLUP"
echo "Flags: \`--gpu --print-gpu-decisions\`" >> "$ROLLUP"
echo "JDK: \`$JDK\`" >> "$ROLLUP"
echo "Logs: \`$LOG/\`" >> "$ROLLUP"
echo "" >> "$ROLLUP"
echo "| App | rc | First error / Last line |" >> "$ROLLUP"
echo "|-----|----|--------------------------|" >> "$ROLLUP"

run() {
    local name="$1"; shift
    local timeout_s="$1"; shift
    echo "----- $name -----" >&2
    # GPU mode is on for every app. CRATONVM_DISABLE_JIT=1 to dodge the
    # known JIT int[]-loop regression so the runner isn't dominated by
    # that single bug.
    CRATONVM_DISABLE_JIT=1 timeout "$timeout_s" "$RJVM" \
        --java-home "$JDK" --gpu --print-gpu-decisions "$@" \
        > "$LOG/$name.out" 2> "$LOG/$name.err"
    local rc=$?
    local first_err
    first_err=$(grep -vE '^\[cratonvm\]|^\[2m2026-|^WARN |^\[GRES-DBG|^\[URLRES-DBG|^\[OSTR-DBG|^\[CCE-DBG|^\[DEBUG|^SLF4J' "$LOG/$name.err" 2>/dev/null | head -1 | head -c 160)
    if [ -z "$first_err" ]; then
        first_err=$(tail -1 "$LOG/$name.out" 2>/dev/null | head -c 160)
    fi
    echo "$name rc=$rc | ${first_err}" >&2
    local esc_err
    esc_err=$(printf '%s' "$first_err" | sed 's/|/\\|/g')
    echo "| $name | $rc | $esc_err |" >> "$ROLLUP"
}

# --- Simple probes ---
( cd "$APPS/enumtest" && run enumtest 25 -c . EnumTest )

SLF4J=$(find "$APPS/ejbca-ce/lib/ext" -name "slf4j-api*.jar" 2>/dev/null | head -1)
( cd "$APPS/slf4j" && run slf4j 25 -c ".;$SLF4J" LogTest )

( cd "$APPS/sig_probe" && run sig_probe 25 -c classes SigProbe )
( cd "$APPS/cipher_probe" && run cipher_probe 25 -c . CipherProbe )
( cd "$APPS/cleaner_probe" && run cleaner_probe 25 -c classes CleanerProbe )
( cd "$APPS/cglib_probe" && run cglib_probe 25 -c "classes;payload" CglibProbe2 )
( cd "$APPS/netty" && run netty 15 -c . NettyEchoTest )

# --- Big servers / launchers ---
( cd "$APPS/wildfly-32.0.1.Final" && run wildfly 30 --jar jboss-modules.jar -- -mp modules org.jboss.as.standalone -Djboss.home.dir=. -Djboss.server.base.dir=standalone )
( cd "$APPS/keycloak-16.1.1" && run keycloak16 30 --jar jboss-modules.jar -- -mp modules org.jboss.as.standalone -Djboss.home.dir=. -Djboss.server.base.dir=standalone )
( cd "$APPS/keycloak-26.2.4/lib" && run keycloak26 30 --jar quarkus-run.jar -- --help )

# --- Big data ---
HBCP=$(find "$APPS/hbase-2.5.10/lib" -name "*.jar" 2>/dev/null | tr '\n' ';' | sed 's/;$//')
run hbase 30 -c "$HBCP" "-Dhbase.home.dir=$APPS/hbase-2.5.10" org.apache.hadoop.hbase.util.VersionInfo

HDCP=$(find "$APPS/hadoop-3.3.6/share/hadoop/common" -name "*.jar" 2>/dev/null | tr '\n' ';' | sed 's/;$//')
HDCP2=$(find "$APPS/hadoop-3.3.6/share/hadoop/common/lib" -name "*.jar" 2>/dev/null | tr '\n' ';' | sed 's/;$//')
run hadoop 30 -c "$HDCP;$HDCP2" "-DHADOOP_HOME=$APPS/hadoop-3.3.6" org.apache.hadoop.util.VersionInfo

# --- Spring Boot apps that have a fat jar built ---
if [ -f "$APPS/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar" ]; then
    ( cd "$APPS/insurance-backend" && run insurance 30 --jar target/insurance-0.0.1-SNAPSHOT.jar )
fi

# --- Spring Boot / app projects with target/classes + Maven local repo on cp ---
# These don't have fat jars; we compose a classpath from their target/classes
# plus every jar in ~/.m2/repository. Will likely still NCDFE on transitive
# deps that aren't in the local repo, but it's the honest GPU-mode attempt.
M2CP=$(find "$M2" -name "*.jar" 2>/dev/null | tr '\n' ';' | sed 's/;$//' | head -c 120000)

if [ -f "$APPS/demo/target/demo-0.0.1-SNAPSHOT.jar" ]; then
    ( cd "$APPS/demo" && run demo 25 --jar target/demo-0.0.1-SNAPSHOT.jar )
elif [ -d "$APPS/demo/target/classes" ]; then
    run demo 25 -c "$APPS/demo/target/classes;$M2CP" com.example.demo.DemoApplication
fi

if [ -d "$APPS/letsgo/letsgo-main/target/classes" ]; then
    run letsgo 25 -c "$APPS/letsgo/letsgo-main/target/classes;$M2CP" com.digsol.main.MainServiceApplication
fi

if [ -d "$APPS/SportMe-master/target/classes" ]; then
    run sportme 25 -c "$APPS/SportMe-master/target/classes;$M2CP" ru.sberbank.sportme.SportMeApplication
fi

# ejbca-ce: Gradle, no obvious single entry-point class — try common launcher
if [ -d "$APPS/ejbca-ce/build/classes/java/main" ]; then
    EJBCA_MAIN=$(find "$APPS/ejbca-ce/build/classes/java/main" -name "EjbcaMain*.class" -o -name "Main.class" 2>/dev/null | head -1)
    if [ -n "$EJBCA_MAIN" ]; then
        run ejbca 25 -c "$APPS/ejbca-ce/build/classes/java/main;$APPS/ejbca-ce/lib/*" "$(basename "$EJBCA_MAIN" .class)"
    fi
fi

# --- GPU benchmark apps ---
if [ -d "$APPS/gpu-bench/classes" ]; then
    run gpu_correctness 25 -c "$APPS/gpu-bench/classes;$CRATON_GPU" GpuCorrectness
    run gpu_simple 25 -c "$APPS/gpu-bench/classes;$CRATON_GPU" SimpleGpuTest
    run gpu_bench 60 -c "$APPS/gpu-bench/classes;$CRATON_GPU" GpuBench 1048576 5 2
fi

echo "" >> "$ROLLUP"
echo "## Summary" >> "$ROLLUP"
echo "" >> "$ROLLUP"
echo "Apps reporting rc=0:" >> "$ROLLUP"
grep -E '^\| [a-z_0-9]+ \| 0 \|' "$ROLLUP" | sed 's/^/  /' >> "$ROLLUP"

cat "$ROLLUP"
