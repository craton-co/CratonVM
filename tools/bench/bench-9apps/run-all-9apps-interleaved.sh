#!/usr/bin/env bash
# Interleaved benchmarking for all 9 apps: CratonVM vs HotSpot
# Azure EPYC host
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNS_DIR="$HERE/results"
STAMP="$(date -u +%Y%m%d_%H%M%S)"
OUT="$RUNS_DIR/$STAMP"
mkdir -p "$OUT"

CV_BIN="/data/wt-bench-9apps/bin/cratonvm-bench-20260913125913"
JDK="/data/toolchain/jdk-25"
export JDK25="$JDK"
export JAVA_HOME="$JDK"
export PATH="$JDK/bin:/data/toolchain/maven/bin:$PATH"

log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$OUT/run.log"; }

log "========================================================================="
log "Starting 9-app interleaved benchmark: CratonVM vs HotSpot"
log "CratonVM: $CV_BIN"
log "HotSpot:  $JDK"
log "Host:     $(uname -a)"
log "Load before: $(uptime)"
log "========================================================================="

REPORT="$OUT/interleaved_benchmark_report.md"
cat > "$REPORT" << HEADER_EOF
# 9 Applications Interleaved Benchmark: CratonVM vs HotSpot

- **Date:** $(date -u)
- **Host:** Azure EPYC Linux Host (\`$(uname -m)\`, 8 vCPUs)
- **CratonVM Binary:** \`$CV_BIN\` (dev branch @ \`2a8aa3888\`)
- **HotSpot Baseline:** \`$JDK\` (Temurin 25.0.4+7-LTS)
- **Mode:** Interleaved runs (CratonVM / HotSpot alternating)

| Application / Workload | Suite / Probe Description | HotSpot (ms) | CratonVM (ms) | Ratio (CV / HS) | Verdict |
| :--- | :--- | :---: | :---: | :---: | :---: |
HEADER_EOF

run_interleaved() {
  local app_name="$1"
  local probe_desc="$2"
  local cv_cmd="$3"
  local hs_cmd="$4"
  local rounds="${5:-5}"

  log ">>> [$app_name] $probe_desc (interleaving $rounds pairs) ..."

  local cv_times=()
  local hs_times=()
  local r

  for (( r=1; r<=rounds; r++ )); do
    # Run CratonVM
    local t0 t1 cv_ms hs_ms rc_cv rc_hs
    t0=$(date +%s%N)
    eval "$cv_cmd" > "$OUT/temp_cv_${r}.log" 2>&1
    rc_cv=$?
    t1=$(date +%s%N)
    cv_ms=$(( (t1 - t0) / 1000000 ))

    # Run HotSpot
    t0=$(date +%s%N)
    eval "$hs_cmd" > "$OUT/temp_hs_${r}.log" 2>&1
    rc_hs=$?
    t1=$(date +%s%N)
    hs_ms=$(( (t1 - t0) / 1000000 ))

    if [ $rc_cv -ne 0 ]; then cv_ms="FAIL"; fi
    if [ $rc_hs -ne 0 ]; then hs_ms="FAIL"; fi

    cv_times+=("$cv_ms")
    hs_times+=("$hs_ms")
    log "    Round $r: CratonVM = ${cv_ms} ms | HotSpot = ${hs_ms} ms"
  done

  # Calculate medians
  local cv_sorted=($(printf '%s\n' "${cv_times[@]}" | grep -v "FAIL" | sort -n))
  local hs_sorted=($(printf '%s\n' "${hs_times[@]}" | grep -v "FAIL" | sort -n))

  local cv_med="FAIL"
  local hs_med="FAIL"
  local ratio="N/A"
  local verdict="PASS"

  if [ ${#cv_sorted[@]} -gt 0 ]; then
    cv_med="${cv_sorted[$(( ${#cv_sorted[@]} / 2 ))]}"
  fi
  if [ ${#hs_sorted[@]} -gt 0 ]; then
    hs_med="${hs_sorted[$(( ${#hs_sorted[@]} / 2 ))]}"
  fi

  if [[ "$cv_med" != "FAIL" && "$hs_med" != "FAIL" && "$hs_med" -gt 0 ]]; then
    ratio=$(awk "BEGIN {printf \"%.2fx\", $cv_med / $hs_med}")
    verdict="PASS (median ${ratio})"
  elif [[ "$cv_med" == "FAIL" ]]; then
    verdict="CV_ERROR"
  elif [[ "$hs_med" == "FAIL" ]]; then
    verdict="HS_ERROR"
  fi

  log "    --> MEDIAN: CratonVM = ${cv_med} ms | HotSpot = ${hs_med} ms | Ratio = ${ratio} ($verdict)"
  echo "| **$app_name** | $probe_desc | ${hs_med} ms | ${cv_med} ms | $ratio | $verdict |" >> "$REPORT"
}

# -----------------------------------------------------------------------------
# 1. Spring Framework
# -----------------------------------------------------------------------------
run_interleaved "Spring Framework" "Core AOT Generation Suite (AccessControlTests)" \
  "cd /data/cratonvm/apps/spring-suite-runner && ALLOW_INCOMPLETE_CP=1 CRATONVM_BIN='$CV_BIN' ./run-suite.sh run --category all --count 1 --jdk '$JDK'" \
  "cd /data/cratonvm/apps/spring-suite-runner && ALLOW_INCOMPLETE_CP=1 ./run-suite.sh hotspot --category all --count 1 --jdk '$JDK'" \
  5

# -----------------------------------------------------------------------------
# 2. Spring Boot
# -----------------------------------------------------------------------------
run_interleaved "Spring Boot" "Smoke batch ApplicationEnvironmentTests" \
  "cd /data/cratonvm/apps/spring-boot-suite-runner && pwsh -NoProfile -File ./run-spring-boot-suite.ps1 -Category all -ClassList smoke-batch.tsv -Count 1 -Vm craton -Exe '$CV_BIN' -JdkHome '$JDK' -Parallel 1 -TimeoutSec 60" \
  "cd /data/cratonvm/apps/spring-boot-suite-runner && pwsh -NoProfile -File ./run-spring-boot-suite.ps1 -Category all -ClassList smoke-batch.tsv -Count 1 -Vm hotspot -JdkHome '$JDK' -Parallel 1 -TimeoutSec 60" \
  5

# -----------------------------------------------------------------------------
# 3. Tomcat
# -----------------------------------------------------------------------------
run_interleaved "Tomcat" "Jakarta EL Core Tests (TestArrayELResolver)" \
  "cd /data/cratonvm/apps/tomcat-suite-runner && TC_ROOT='/data/cratonvm/apps/tomcat' CRATONVM_EXE='$CV_BIN' ./run-tomcat-suite.sh craton 0 1 tomcat_cv /data/cratonvm/apps/tomcat/.suite/all-tests.txt 1" \
  "cd /data/cratonvm/apps/tomcat-suite-runner && TC_ROOT='/data/cratonvm/apps/tomcat' ./run-tomcat-suite.sh hotspot 0 1 tomcat_hs /data/cratonvm/apps/tomcat/.suite/all-tests.txt 1" \
  5

# -----------------------------------------------------------------------------
# 4. Netty
# -----------------------------------------------------------------------------
run_interleaved "Netty" "Unix Native Inet Address & Channel Suite" \
  "/data/cratonvm/apps/netty-suite-runner/run_single_netty.sh craton io.netty.channel.unix.NativeInetAddressTest" \
  "/data/cratonvm/apps/netty-suite-runner/run_single_netty.sh hotspot io.netty.channel.unix.NativeInetAddressTest" \
  5

# -----------------------------------------------------------------------------
# 5. Hibernate ORM
# -----------------------------------------------------------------------------
run_interleaved "Hibernate ORM" "Bytecode enhancement & entity tests" \
  "cd /data/cratonvm/apps/hib-suite-runner && CV_BIN='$CV_BIN' ./run-hib.sh --count 1" \
  "cd /data/cratonvm/apps/hib-suite-runner && ./run-hib.sh --count 1" \
  5

# -----------------------------------------------------------------------------
# 6. Hibernate Reactive (with PostgreSQL driver)
# -----------------------------------------------------------------------------
run_interleaved "Hibernate Reactive / PostgreSQL" "Reactive CompletionStages & SCRAM against Docker Postgres" \
  "cd /data/cratonvm/apps/hibernate-reactive-suite-runner && CV_BIN='$CV_BIN' ./run-hibernate-reactive-suite.sh --list /data/cratonvm/apps/hibernate-reactive-suite-runner/validation-batch.txt --count 1" \
  "cd /data/cratonvm/apps/hibernate-reactive-suite-runner && ./run-hibernate-reactive-suite.sh --list /data/cratonvm/apps/hibernate-reactive-suite-runner/validation-batch.txt --count 1" \
  5

# -----------------------------------------------------------------------------
# 7. H2 Database
# -----------------------------------------------------------------------------
run_interleaved "H2 Database" "Database Alter & DDL Table Suite (TestAlter)" \
  "cd /data/cratonvm/apps/h2database-suite-runner && JDK25='$JDK' CRATONVM_BIN='$CV_BIN' ./run-h2-suite.sh run --category all --start 2 --count 1" \
  "cd /data/cratonvm/apps/h2database-suite-runner && JDK25='$JDK' CRATONVM_BIN='$CV_BIN' ./run-h2-suite.sh hotspot --category all --start 2 --count 1" \
  5

# -----------------------------------------------------------------------------
# 8. Apache Commons Math
# -----------------------------------------------------------------------------
MATH_CP="/data/toolchain/m2repo/org/apache/commons/commons-math4-core/4.0-SNAPSHOT/commons-math4-core-4.0-SNAPSHOT-tests.jar:/data/toolchain/m2repo/org/apache/commons/commons-math4-core/4.0-SNAPSHOT/commons-math4-core-4.0-SNAPSHOT.jar:/data/toolchain/m2repo/junit/junit/4.13.2/junit-4.13.2.jar:/data/toolchain/m2repo/org/hamcrest/hamcrest-core/1.3/hamcrest-core-1.3.jar"
run_interleaved "Apache Commons Math" "Core JdkMath / FastMath Numerical Kernel" \
  "'$CV_BIN' --java-home '$JDK' -cp '$MATH_CP' org.junit.runner.JUnitCore org.apache.commons.math4.core.jdkmath.JdkMathTest" \
  "'$JDK/bin/java' -cp '$MATH_CP' org.junit.runner.JUnitCore org.apache.commons.math4.core.jdkmath.JdkMathTest" \
  5

# -----------------------------------------------------------------------------
# 9. Bouncy Castle Java
# -----------------------------------------------------------------------------
BC_CP="/data/cratonvm/apps/bc-java/core/build/classes/java/main:/data/cratonvm/apps/bc-java/core/build/classes/java/test:/data/cratonvm/apps/bc-java/prov/build/classes/java/main:/data/cratonvm/apps/bc-java/prov/build/classes/java/test"
run_interleaved "Bouncy Castle Java" "AESFast / Symmetric Cipher Validation" \
  "'$CV_BIN' --java-home '$JDK' -cp '$BC_CP' org.bouncycastle.crypto.test.AESFastTest" \
  "'$JDK/bin/java' -cp '$BC_CP' org.bouncycastle.crypto.test.AESFastTest" \
  5

log "========================================================================="
log "Completed all 9 apps interleaved benchmarks!"
log "Load after: $(uptime)"
log "========================================================================="

cat "$REPORT"
