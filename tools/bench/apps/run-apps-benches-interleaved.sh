#!/usr/bin/env bash
# Interleaved benchmark runner for supported apps test suites / benchmarks
# CratonVM vs HotSpot (JDK 25)
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-$HERE/results}"
STAMP="$(date -u +%Y%m%d_%H%M%S)"
OUT="${OUT:-$RESULTS_DIR/$STAMP}"
mkdir -p "$OUT"

CV_BIN="${CV_BIN:-$ROOT/target/release/cratonvm}"
JDK_HOME="${JDK_HOME:-/usr/lib/jvm/java-25-openjdk-amd64}"
JAVA_BIN="$JDK_HOME/bin/java"
TIMEOUT="${TIMEOUT:-1200}"
APPS_ROOT="${APPS_ROOT:-/root/CVM/apps}"
FAILED=0
CASE=0
[[ "$APPS_ROOT" =~ ^/[A-Za-z0-9_./-]+$ ]] && [ -d "$APPS_ROOT" ] || {
  echo 'apps_root must be an existing absolute path without shell characters' >&2; exit 2;
}
[ -x "$CV_BIN" ] && [ -x "$JAVA_BIN" ] || { echo 'Missing VM or JDK binary' >&2; exit 2; }
export CRATONVM_BOOT_IMAGE=off

log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$OUT/run.log"; }

log "========================================================================="
log "Starting Comprehensive App Performance Benchmarks: CratonVM vs HotSpot"
log "CratonVM: $CV_BIN"
log "HotSpot:  $JAVA_BIN ($($JAVA_BIN -version 2>&1 | head -n 1))"
log "Host:     $(uname -a)"
log "Timeout:  ${TIMEOUT}s per run"
log "Revision: $(git -C "$ROOT" rev-parse HEAD)"
log "Initial load: $(cat /proc/loadavg)"
log "========================================================================="
sha256sum "$CV_BIN" > "$OUT/cratonvm-sha256.txt"
"$JAVA_BIN" -version > "$OUT/java-version.txt" 2>&1

REPORT="$OUT/interleaved_benchmark_report.md"
cat > "$REPORT" << HEADER_EOF
# Supported Apps Interleaved Benchmark: CratonVM vs HotSpot

- **Date:** $(date -u)
- **Host:** Linux Host (\`$(uname -m)\`, $(nproc) vCPUs)
- **CratonVM Binary:** \`$CV_BIN\` (dev branch)
- **HotSpot Baseline:** OpenJDK 25 (\`$JDK_HOME\`)
- **Timeout:** ${TIMEOUT} seconds per benchmark arm
- **Execution Mode:** Interleaved (CratonVM / HotSpot)

| Application | Benchmark / Probe Description | HotSpot (ms) | CratonVM (ms) | Ratio (CV / HS) | Verdict |
| :--- | :--- | :---: | :---: | :---: | :---: |
HEADER_EOF

run_bench() {
  CASE=$((CASE + 1))
  local app_name="$1"
  local probe_desc="$2"
  local cv_cmd="$3"
  local hs_cmd="$4"
  mkdir -p "$OUT/work/$CASE/cvm" "$OUT/work/$CASE/hotspot"
  printf '%s\t%s\t%s\n' "$CASE" "$cv_cmd" "$hs_cmd" >> "$OUT/commands.tsv"

  log ">>> [$app_name] $probe_desc ..."

  # 1. Run CratonVM arm
  local t0 t1 cv_ms hs_ms rc_cv rc_hs
  log "    [Arm 1: CratonVM] Running with timeout ${TIMEOUT}s..."
  t0=$(date +%s%N)
  ( cd "$OUT/work/$CASE/cvm" && timeout "${TIMEOUT}s" bash -c "$cv_cmd" ) > "$OUT/${CASE}_cvm.log" 2>&1
  rc_cv=$?
  t1=$(date +%s%N)
  cv_ms=$(( (t1 - t0) / 1000000 ))
  if [ $rc_cv -eq 124 ]; then
    cv_ms="TIMEOUT"
    log "    CratonVM: TIMEOUT (> ${TIMEOUT}s)"
  elif [ $rc_cv -ne 0 ]; then
    cv_ms="FAIL (rc=$rc_cv)"
    log "    CratonVM: FAIL (rc=$rc_cv, ${cv_ms} ms)"
  else
    log "    CratonVM: ${cv_ms} ms (PASS)"
  fi

  # 2. Run HotSpot arm
  log "    [Arm 2: HotSpot] Running with timeout ${TIMEOUT}s..."
  t0=$(date +%s%N)
  ( cd "$OUT/work/$CASE/hotspot" && timeout "${TIMEOUT}s" bash -c "$hs_cmd" ) > "$OUT/${CASE}_hotspot.log" 2>&1
  rc_hs=$?
  t1=$(date +%s%N)
  hs_ms=$(( (t1 - t0) / 1000000 ))
  if [ $rc_hs -eq 124 ]; then
    hs_ms="TIMEOUT"
    log "    HotSpot: TIMEOUT (> ${TIMEOUT}s)"
  elif [ $rc_hs -ne 0 ]; then
    hs_ms="FAIL (rc=$rc_hs)"
    log "    HotSpot: FAIL (rc=$rc_hs, ${hs_ms} ms)"
  else
    log "    HotSpot: ${hs_ms} ms (PASS)"
  fi

  local ratio="N/A"
  local verdict="PASS"

  if [[ "$cv_ms" =~ ^[0-9]+$ && "$hs_ms" =~ ^[0-9]+$ ]]; then
    if [ "$hs_ms" -gt 0 ]; then
      ratio=$(awk "BEGIN {printf \"%.2fx\", $cv_ms / $hs_ms}")
      verdict="PASS (${ratio})"
    fi
  elif [[ "$cv_ms" == "TIMEOUT" || "$hs_ms" == "TIMEOUT" ]]; then
    verdict="TIMEOUT"
  else
    verdict="ERROR"
  fi
  local cv_sum hs_sum
  cv_sum=$(grep -oE 'CHECKSUM=-?[0-9]+' "$OUT/${CASE}_cvm.log" | tail -1)
  hs_sum=$(grep -oE 'CHECKSUM=-?[0-9]+' "$OUT/${CASE}_hotspot.log" | tail -1)
  if [ -n "$cv_sum$hs_sum" ] && [ "$cv_sum" != "$hs_sum" ]; then verdict="ERROR"; fi
  if [[ "$cv_cmd" == *org.junit.runner.JUnitCore* ]]; then
    local cv_tests hs_tests
    cv_tests=$(grep -oE '^OK \([1-9][0-9]* tests?\)' "$OUT/${CASE}_cvm.log" | tail -1)
    hs_tests=$(grep -oE '^OK \([1-9][0-9]* tests?\)' "$OUT/${CASE}_hotspot.log" | tail -1)
    if [ -z "$cv_tests" ] || [ "$cv_tests" != "$hs_tests" ]; then verdict="ERROR"; fi
  fi
  if grep -Eq 'FAILURES!!!|Exception in thread|^FAIL:' "$OUT/${CASE}_cvm.log" "$OUT/${CASE}_hotspot.log"; then verdict="ERROR"; fi
  if [ "$verdict" = "TIMEOUT" ] || [ "$verdict" = "ERROR" ]; then FAILED=1; fi

  log "    --> RESULT: HotSpot = ${hs_ms} ms | CratonVM = ${cv_ms} ms | Ratio = ${ratio} ($verdict)"
  echo "| **$app_name** | $probe_desc | ${hs_ms} ms | ${cv_ms} ms | $ratio | $verdict |" >> "$REPORT"
}

TC_CP="$(cat "${TC_CP_FILE:-/root/tc-cp.txt}")"
MATH_CP="/root/.m2/repository/org/apache/commons/commons-math4-core/4.0-SNAPSHOT/commons-math4-core-4.0-SNAPSHOT-tests.jar:/root/.m2/repository/org/apache/commons/commons-math4-core/4.0-SNAPSHOT/commons-math4-core-4.0-SNAPSHOT.jar:$APPS_ROOT/bc-java/libs/junit-4.13.2.jar:/root/tomcat-build-libs/hamcrest-3.0/hamcrest-3.0.jar"
MATH_PERF_CP="$OUT/classes:$(cat "${MATH_CP_FILE:-/root/math-cp.txt}")"
BC_CP="$APPS_ROOT/bc-java/core/build/classes/java/main:$APPS_ROOT/bc-java/core/build/classes/java/test:$APPS_ROOT/bc-java/prov/build/classes/java/main:$APPS_ROOT/bc-java/prov/build/classes/java/test:$APPS_ROOT/bc-java/libs/junit-4.13.2.jar:/root/.m2/repository/org/hamcrest/hamcrest-core/1.3/hamcrest-core-1.3.jar"
H2_CP="$APPS_ROOT/h2database/h2/target/test-classes:$APPS_ROOT/h2database/h2/target/h2-2.5.259-SNAPSHOT.jar"
SB_CP="$APPS_ROOT/spring-boot/loader/spring-boot-loader/build/classes/java/main:$APPS_ROOT/spring-boot/loader/spring-boot-loader/build/classes/java/test"
NETTY_CP="$APPS_ROOT/netty/common/target/classes:$APPS_ROOT/netty/buffer/target/classes:$APPS_ROOT/netty/codec-base/target/classes:$APPS_ROOT/netty/codec-http/target/classes:$(find /root/.m2/repository/org/openjdk/jmh/jmh-core -name 'jmh-core-*.jar' | head -1):/root/.m2/repository/org/jctools/jctools-core/4.0.7/jctools-core-4.0.7.jar:/root/.m2/repository/org/jctools/jctools-core-jdk11/4.0.7/jctools-core-jdk11-4.0.7.jar"

mkdir -p "$OUT/classes"
"$JDK_HOME/bin/javac" -cp "$NETTY_CP" -d "$OUT/classes" \
  "$HERE/netty/DateFormatterBenchmark.java" "$HERE/RunNettyDate.java" "$HERE/netty/NettyByteBufProbe.java" || exit 2
"$JDK_HOME/bin/javac" -cp "$SB_CP" -d "$OUT/classes" "$HERE/RunVirtualZip.java" || exit 2
NETTY_CP="$OUT/classes:$NETTY_CP"
"$JDK_HOME/bin/javac" -cp "$MATH_PERF_CP" -d "$OUT/classes" \
  "$HERE/commons-math/PerfTestUtils.java" "$HERE/commons-math/JdkMathTestPerformance.java" || exit 2
if [ "${PREPARE_ONLY:-0}" = 1 ]; then echo 'APP_FIXTURES_OK'; exit 0; fi

# 1. Tomcat TestResponsePerformance
run_bench "Tomcat" "URI toAbsolute resolution benchmark (TestResponsePerformance, 1M ops)" \
  "cd '$APPS_ROOT/tomcat' && $CV_BIN --java-home $JDK_HOME -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.catalina.connector.TestResponsePerformance" \
  "cd '$APPS_ROOT/tomcat' && $JAVA_BIN -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.catalina.connector.TestResponsePerformance"

# 2. Tomcat TestELParserPerformance
run_bench "Tomcat" "Jakarta EL Parser throughput & reinit benchmark (TestELParserPerformance)" \
  "cd '$APPS_ROOT/tomcat' && $CV_BIN --java-home $JDK_HOME -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.el.parser.TestELParserPerformance" \
  "cd '$APPS_ROOT/tomcat' && $JAVA_BIN -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.el.parser.TestELParserPerformance"

# 3. Tomcat TestMethodPerformance
run_bench "Tomcat" "HTTP method parsing benchmark (TestMethodPerformance, 100M ops)" \
  "cd '$APPS_ROOT/tomcat' && $CV_BIN --java-home $JDK_HOME -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.tomcat.util.http.TestMethodPerformance" \
  "cd '$APPS_ROOT/tomcat' && $JAVA_BIN -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.tomcat.util.http.TestMethodPerformance"

# 4. Tomcat TestCharsetCachePerformance
run_bench "Tomcat" "Charset lookup cache hit throughput (TestCharsetCachePerformance)" \
  "cd '$APPS_ROOT/tomcat' && $CV_BIN --java-home $JDK_HOME -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.tomcat.util.buf.TestCharsetCachePerformance" \
  "cd '$APPS_ROOT/tomcat' && $JAVA_BIN -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.tomcat.util.buf.TestCharsetCachePerformance"

# 5. Tomcat TestOneLineFormatterPerformance
run_bench "Tomcat" "JULI log record format throughput (TestOneLineFormatterPerformance, 100K ops)" \
  "cd '$APPS_ROOT/tomcat' && $CV_BIN --java-home $JDK_HOME -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.juli.TestOneLineFormatterPerformance" \
  "cd '$APPS_ROOT/tomcat' && $JAVA_BIN -cp '$TC_CP' org.junit.runner.JUnitCore org.apache.juli.TestOneLineFormatterPerformance"

# 6. H2 Database TestBenchmark
run_bench "H2 Database" "Multi-threaded MVStore Concurrency Benchmark (TestBenchmark)" \
  "$CV_BIN --java-home $JDK_HOME -cp '$H2_CP' org.h2.test.store.TestBenchmark" \
  "$JAVA_BIN -cp '$H2_CP' org.h2.test.store.TestBenchmark"

# 7. H2 Database TestMVStoreBenchmark
run_bench "H2 Database" "MVStore Key-Value Storage Engine Throughput (TestMVStoreBenchmark)" \
  "$CV_BIN --java-home $JDK_HOME -cp '$H2_CP' org.h2.test.store.TestMVStoreBenchmark" \
  "$JAVA_BIN -cp '$H2_CP' org.h2.test.store.TestMVStoreBenchmark"

# 8. H2 Database TestPerformance
run_bench "H2 Database" "Full SQL Benchmark across Embedded, C/S TCP & In-Memory (TestPerformance)" \
  "$CV_BIN --java-home $JDK_HOME -cp '$H2_CP' org.h2.test.bench.TestPerformance -exit" \
  "$JAVA_BIN -cp '$H2_CP' org.h2.test.bench.TestPerformance -exit"

run_bench "H2 Database" "Disk-backed TCP, size 5000 (TestPerformance, G1)" \
  "$CV_BIN --java-home $JDK_HOME -XX:+UseG1GC -cp '$H2_CP' org.h2.test.bench.TestPerformance -db 4 -size 5000 -exit" \
  "$JAVA_BIN -XX:+UseG1GC -cp '$H2_CP' org.h2.test.bench.TestPerformance -db 4 -size 5000 -exit"

# 9. Apache Commons Math JdkMathTest
run_bench "Apache Commons Math" "JdkMath numerical evaluation kernel (JdkMathTest)" \
  "$CV_BIN --java-home $JDK_HOME -cp '$MATH_CP' org.junit.runner.JUnitCore org.apache.commons.math4.core.jdkmath.JdkMathTest" \
  "$JAVA_BIN -cp '$MATH_CP' org.junit.runner.JUnitCore org.apache.commons.math4.core.jdkmath.JdkMathTest"

# 10. Apache Commons Math JdkMathTestPerformance
run_bench "Apache Commons Math" "Comprehensive 100K Transcendental Evaluation (JdkMathTestPerformance)" \
  "$CV_BIN --java-home $JDK_HOME -DtestRuns=100000 -cp '$MATH_PERF_CP' org.apache.commons.math4.userguide.JdkMathTestPerformance" \
  "$JAVA_BIN -DtestRuns=100000 -cp '$MATH_PERF_CP' org.apache.commons.math4.userguide.JdkMathTestPerformance"

# 11. Bouncy Castle Java AESFastTest
run_bench "Bouncy Castle Java" "AESFast Symmetric Cipher Validation & Throughput" \
  "$CV_BIN --java-home $JDK_HOME -cp '$BC_CP' org.bouncycastle.crypto.test.AESFastTest" \
  "$JAVA_BIN -cp '$BC_CP' org.bouncycastle.crypto.test.AESFastTest"

# 12. Bouncy Castle Java ECPointPerformanceTest
run_bench "Bouncy Castle Java" "Elliptic Curve Point Multiplication Suite (ECPointPerformanceTest)" \
  "$CV_BIN --java-home $JDK_HOME -cp '$BC_CP' org.junit.runner.JUnitCore org.bouncycastle.math.ec.test.ECPointPerformanceTest" \
  "$JAVA_BIN -cp '$BC_CP' org.junit.runner.JUnitCore org.bouncycastle.math.ec.test.ECPointPerformanceTest"

# 13. Spring Framework IntroductionBenchmarkTests
run_bench "Spring Framework" "AOP Introductions delegation benchmark (IntroductionBenchmarkTests, 100K ops)" \
  "cd '$APPS_ROOT/spring-suite-runner' && $CV_BIN --java-home $JDK_HOME @$APPS_ROOT/spring-suite-runner/.af_spring.txt CratonRunner org.springframework.aop.framework.IntroductionBenchmarkTests" \
  "cd '$APPS_ROOT/spring-suite-runner' && $JAVA_BIN @$APPS_ROOT/spring-suite-runner/.af_spring.txt CratonRunner org.springframework.aop.framework.IntroductionBenchmarkTests"

# 14. Spring Framework BenchmarkTests
run_bench "Spring Framework" "AspectJ vs Spring AOP proxy invocation benchmark (BenchmarkTests, 8 suites)" \
  "cd '$APPS_ROOT/spring-suite-runner' && $CV_BIN --java-home $JDK_HOME @$APPS_ROOT/spring-suite-runner/.af_spring.txt CratonRunner org.springframework.aop.aspectj.autoproxy.benchmark.BenchmarkTests" \
  "cd '$APPS_ROOT/spring-suite-runner' && $JAVA_BIN @$APPS_ROOT/spring-suite-runner/.af_spring.txt CratonRunner org.springframework.aop.aspectj.autoproxy.benchmark.BenchmarkTests"

# 15. Spring Boot VirtualZipPerformanceTests
run_bench "Spring Boot" "VirtualZip Archive Entry Access (VirtualZipPerformanceTests)" \
  "$CV_BIN --java-home $JDK_HOME -cp '$OUT/classes:$SB_CP' RunVirtualZip" \
  "$JAVA_BIN -cp '$OUT/classes:$SB_CP' RunVirtualZip"

# 16. Netty DateFormatterBenchmark
run_bench "Netty" "HTTP Date Parser & Formatter Throughput (DateFormatterBenchmark, 500K ops)" \
  "$CV_BIN --java-home $JDK_HOME -cp '$NETTY_CP' RunNettyDate" \
  "$JAVA_BIN -cp '$NETTY_CP' RunNettyDate"

# 17. Netty ByteBufAccessBenchmark
for policy in allow default; do
  if bash "$HERE/netty/run-bytebuf-probe.sh" "$CV_BIN" "$JDK_HOME" "$NETTY_CP" "$OUT/netty-bytebuf-$policy" "$policy"; then
    python3 - "$OUT/netty-bytebuf-$policy" "$policy" >> "$REPORT" <<'PY' || FAILED=1
import pathlib, re, statistics, sys
directory, policy = pathlib.Path(sys.argv[1]), sys.argv[2]
for kind in range(4):
    medians = {}
    for arm in ('hotspot', 'foundational', 'compatible'):
        values = []
        for log in directory.glob(f'*-{kind}-{arm}.log'):
            values.extend(int(ns) / 1_000_000 for round_, ns in
                          re.findall(r'round=(\d+) ns=(\d+)', log.read_text()) if int(round_) > 0)
        if len(values) != 16:
            raise SystemExit(f'Expected 16 samples for {kind}/{arm}; got {len(values)}')
        medians[arm] = statistics.median(values)
    print(f'| **Netty** | ByteBuf kind {kind}, {policy} | {medians["hotspot"]:.3f} ms | '
          f'{medians["foundational"]:.3f} ms foundational / {medians["compatible"]:.3f} ms compatible | '
          f'{medians["foundational"] / medians["hotspot"]:.2f}x / '
          f'{medians["compatible"] / medians["hotspot"]:.2f}x | PASS |')
PY
  else
    FAILED=1
    echo "| **Netty** | ByteBuf 1M pairs ($policy, all four buffer kinds) | — | — | — | ERROR |" >> "$REPORT"
  fi
done

# 18. Hibernate ORM ActionQueuePerformanceTest
run_bench "Hibernate ORM" "Graph vs Legacy ActionQueue Entity Insertion (ActionQueuePerformanceTest)" \
  "cd /root/hibs/runner && $CV_BIN --java-home $JDK_HOME @common.args CratonRunner org.hibernate.action.queue.ActionQueuePerformanceTest\$SimpleGraphTest" \
  "cd /root/hibs/runner && $JAVA_BIN @common.args CratonRunner org.hibernate.action.queue.ActionQueuePerformanceTest\$SimpleGraphTest"

# 19. Hibernate ORM ElementCollectionPerfTest
run_bench "Hibernate ORM" "Collection Element Batch Persistence & Query (ElementCollectionPerfTest)" \
  "cd /root/hibs/runner && $CV_BIN --java-home $JDK_HOME @common.args CratonRunner org.hibernate.orm.test.mapping.collections.ElementCollectionPerfTest" \
  "cd /root/hibs/runner && $JAVA_BIN @common.args CratonRunner org.hibernate.orm.test.mapping.collections.ElementCollectionPerfTest"

log "========================================================================="
log "All comprehensive interleaved benchmarks completed!"
log "========================================================================="

cat "$REPORT"
exit "$FAILED"
