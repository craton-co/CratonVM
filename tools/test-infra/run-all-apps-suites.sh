#!/usr/bin/env bash
# Run apps/ test suites: CratonVM first; HotSpot only when CratonVM PASS.
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
HS="$JDK/bin/java.exe"
POOL="${POOL:-$ROOT/apps/probe/test-infra/regression-pool}"
[ -d "$POOL" ] || POOL="$ROOT/test-infra/regression-pool"
TS=$(date +%Y%m%d-%H%M%S)
LOGDIR="$ROOT/test-infra/suite-results/apps-all-$TS"
OUT="$LOGDIR/results.tsv"
MD="$ROOT/apps/APPS_SUITE_RESULTS.md"
TIMEOUT="${TIMEOUT:-600}"

mkdir -p "$LOGDIR" "$ROOT/test-infra/suite-results"
printf "suite\tvm\trc\tstate\twall_s\tnote\n" > "$OUT"

# Kill ONLY leftover cratonvm.exe processes launched from THIS tree ($ROOT),
# so a parallel suite / another worktree / a dev session is left untouched.
# Never let a no-match surface as a failure (the documented rc=1 footgun of
# `taskkill /F /IM cratonvm.exe`, which also kills unrelated processes).
kill_stray_cratonvm() {
  local root_win
  root_win=$(printf '%s' "$ROOT" | sed 's|/|\\\\|g')
  powershell.exe -NoProfile -Command \
    "Get-CimInstance Win32_Process -Filter \"Name='cratonvm.exe'\" |
       Where-Object { \$_.ExecutablePath -and \$_.ExecutablePath -like '${root_win}*' } |
       ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" \
    >/dev/null 2>&1 || true
}
kill_stray_cratonvm
[ -x "$CV" ] || { echo "ERROR: missing $CV"; exit 3; }

run_suite() {
  local label="$1" xmx="$2" cp="$3" main="$4" args="$5"
  local cvlog="$LOGDIR/${label}-cratonvm.log"
  local hslog="$LOGDIR/${label}-hotspot.log"
  echo "== $label =="
  local t0=$(date +%s%N)
  timeout "$TIMEOUT" "$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$xmx" \
    -cp "$cp" $main $args </dev/null >"$cvlog" 2>&1
  local rc=$?; local t1=$(date +%s%N)
  local wall=$(awk -v ms=$(((t1-t0)/1000000)) 'BEGIN{printf "%.1f", ms/1000}')
  local state=PASS sig note=""
  [ "$rc" -eq 124 ] && state=TIMEOUT
  grep -qaiE "SEGV|panic|abort|fatal error|stack overflow|implausible|System\.exit\(-1\)" "$cvlog" && state=CRASH
  grep -qaiE "Exception|Error:|FAILED|FAILURE" "$cvlog" && [ "$state" = PASS ] && state=FAIL
  grep -qE "RESULT.*ok=false|HIB_SMOKE.*FAIL" "$cvlog" && state=FAIL
  grep -qE ": Okay|All tests successful" "$cvlog" && ! grep -qaiE "Exception|Error:" "$cvlog" && state=PASS
  grep -qE "RESULT.*ok=true|HIB_SMOKE_OK" "$cvlog" && [ "$rc" -eq 0 ] && state=PASS
  # JUnitProbe success: EXEC_FOUND=N SUCCEEDED=N FAILED=0 — avoid "FAILED" false-positive above
  grep -qE "SUCCEEDED=[0-9]+ FAILED=0" "$cvlog" && [ "$rc" -eq 0 ] && state=PASS
  [ "$rc" -ne 0 ] && [ "$state" != PASS ] && state=FAIL
  sig=$(grep -aiE "Exception|Error|SEGV|panic|AbstractMethodError|NoSuchMethod|System\.exit" "$cvlog" \
    | grep -avE "^\s*at " | head -1 | sed 's/\x1b\[[0-9;]*m//g' | head -c 120)
  note="${sig:-$(tail -1 "$cvlog" | head -c 80)}"
  printf "  cratonvm rc=%s %-8s %ss  %s\n" "$rc" "$state" "$wall" "$note"
  printf "%s\tcratonvm\t%s\t%s\t%s\t%s\n" "$label" "$rc" "$state" "$wall" "$note" >>"$OUT"
  if [ "$state" = PASS ]; then
    t0=$(date +%s%N)
    timeout "$TIMEOUT" "$HS" -Xmx"$xmx" -cp "$cp" $main $args </dev/null >"$hslog" 2>&1
    rc=$?; t1=$(date +%s%N)
    wall=$(awk -v ms=$(((t1-t0)/1000000)) 'BEGIN{printf "%.1f", ms/1000}')
    state=PASS; [ "$rc" -ne 0 ] && state=FAIL
    note=$(tail -1 "$hslog" | head -c 80)
    printf "  hotspot  rc=%s %-8s %ss  %s\n" "$rc" "$state" "$wall" "$note"
    printf "%s\thotspot\t%s\t%s\t%s\t%s\n" "$label" "$rc" "$state" "$wall" "$note" >>"$OUT"
  else
    echo "  hotspot  (skipped — CratonVM did not PASS)"
    printf "%s\thotspot\t-\tSKIP\t-\tCratonVM not PASS\n" "$label" >>"$OUT"
  fi
  echo
}

run_pool_probe() {
  local name="$1"
  echo "== pool:$name =="
  local log="$LOGDIR/pool-$name-cratonvm.log"
  RJVM="$CV" bash "$POOL/run.sh" "$name" >"$log" 2>&1
  local line=$(grep -aE "^${name}[[:space:]]+\|" "$log" | head -1)
  local rc=$(echo "$line" | awk -F'|' '{gsub(/ /,"",$2); gsub(/^rc=/,"",$2); print $2}')
  local wall=$(echo "$line" | awk -F'|' '{gsub(/ /,"",$3); gsub(/s$/,"",$3); print $3}')
  local st=$(echo "$line" | awk -F'|' '{gsub(/ /,"",$4); print $4}')
  [ -z "$line" ] && { rc=127; st=CRASH; wall=0; }
  local note=$(tail -3 "$log" | tr '\n' ' ' | head -c 120)
  printf "  cratonvm rc=%s %s %ss %s\n" "${rc:-?}" "${st:-?}" "${wall:-?}" "$note"
  printf "pool:%s\tcratonvm\t%s\t%s\t%s\t%s\n" "$name" "${rc:-127}" "${st:-CRASH}" "${wall:-0}" "$note" >>"$OUT"
  if [ "$st" = PASS ]; then
    local hslog="$LOGDIR/pool-$name-hotspot.log"
    RJVM="$HS" bash "$POOL/run.sh" "$name" >"$hslog" 2>&1
    line=$(grep -aE "^${name}[[:space:]]+\|" "$hslog" | head -1)
    rc=$(echo "$line" | awk -F'|' '{gsub(/ /,"",$2); gsub(/^rc=/,"",$2); print $2}')
    wall=$(echo "$line" | awk -F'|' '{gsub(/ /,"",$3); gsub(/s$/,"",$3); print $3}')
    st=$(echo "$line" | awk -F'|' '{gsub(/ /,"",$4); print $4}')
    printf "  hotspot  rc=%s %s %ss\n" "${rc:-?}" "${st:-?}" "${wall:-?}"
    printf "pool:%s\thotspot\t%s\t%s\t%s\t-\n" "$name" "${rc:-?}" "${st:-?}" "${wall:-?}" >>"$OUT"
  else
    echo "  hotspot  (skipped)"
    printf "pool:%s\thotspot\t-\tSKIP\t-\t-\n" "$name" >>"$OUT"
  fi
  echo
}

for app in kafka_2.13-3.7.0 spring-boot-4.0.6 apache-tomcat-10.1.31; do
  bash "$POOL/stage.sh" "$app" >>"$LOGDIR/stage.log" 2>&1
done

H2="$ROOT/apps/h2database/h2"
H2CP="temp;ext/jts-core-1.19.0.jar;ext/jakarta.servlet-api-5.0.0.jar;ext/javax.servlet-api-4.0.1.jar;ext/asm-9.5.jar;ext/lucene-core-9.7.0.jar;ext/lucene-analysis-common-9.7.0.jar;ext/lucene-queryparser-9.7.0.jar;ext/slf4j-api-2.0.7.jar;ext/junit-jupiter-api-5.10.0.jar;ext/apiguardian-1.1.2.jar;ext/org.osgi.core-5.0.0.jar;ext/org.osgi.service.jdbc-1.1.0.jar"
[ -f "$H2/temp/org/h2/test/TestAll.class" ] && ( cd "$H2" && run_suite "h2-testall-fast" "1g" "$H2CP" org.h2.test.TestAll "-fast" )

WF="$ROOT/apps/wildfly/health"
if [ -d "$WF/target/test-classes" ] && [ -f "$ROOT/apps/_test-harness/RunDirTests.class" ]; then
  WFCP="$ROOT/apps/_test-harness;$WF/target/classes;$WF/target/test-classes;$(cat "$WF/cratonvm-health-cp.txt" 2>/dev/null)"
  run_suite "wildfly-health" "2g" "$WFCP" RunDirTests "$WF/target/test-classes"
fi

FIX="$ROOT/apps/probe/.smoke-cache/hibernate-ri10"
if [ -f "$FIX/fixture/HibernateSmoke.class" ]; then
  HIB_CP=$(ls "$FIX"/*.jar 2>/dev/null | tr '\n' ';')
  run_suite "hibernate-smoke" "1g" "$FIX/fixture;${HIB_CP%;}" HibernateSmoke ""
fi

ES="$ROOT/apps/elasticsearch-8.15.5"
if [ -d "$ES/lib" ]; then
  ES_CP=$(find "$ES/lib" -name '*.jar' 2>/dev/null | sed 's|^/c|C:|' | tr '\n' ';')
  echo "== elasticsearch-version =="
  cvlog="$LOGDIR/elasticsearch-version-cratonvm.log"
  t0=$(date +%s%N)
  timeout "$TIMEOUT" "$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 512m \
    -cp "${ES_CP%;}" -Dcli.name=server "-Des.path.home=$ES" "-Des.path.conf=$ES/config" \
    org.elasticsearch.launcher.CliToolLauncher -V </dev/null >"$cvlog" 2>&1
  rc=$?; t1=$(date +%s%N); wall=$(awk -v ms=$(((t1-t0)/1000000)) 'BEGIN{printf "%.1f", ms/1000}')
  st=PASS; grep -qE "Version:|BUILD" "$cvlog" || st=FAIL; [ "$rc" -ne 0 ] && st=FAIL
  note=$(grep -aoE "Version:|Exception|Error" "$cvlog" | head -1)
  printf "  cratonvm rc=%s %s %ss %s\n" "$rc" "$st" "$wall" "$note"
  printf "elasticsearch-version\tcratonvm\t%s\t%s\t%s\t%s\n" "$rc" "$st" "$wall" "${note:-}" >>"$OUT"
  if [ "$st" = PASS ] && [ "$rc" -eq 0 ]; then
    hslog="$LOGDIR/elasticsearch-version-hotspot.log"
    t0=$(date +%s%N)
    timeout "$TIMEOUT" "$HS" -Xmx512m -cp "${ES_CP%;}" -Dcli.name=server \
      "-Des.path.home=$ES" "-Des.path.conf=$ES/config" \
      org.elasticsearch.launcher.CliToolLauncher -V </dev/null >"$hslog" 2>&1
    rc=$?; t1=$(date +%s%N); wall=$(awk -v ms=$(((t1-t0)/1000000)) 'BEGIN{printf "%.1f", ms/1000}')
    st=PASS; [ "$rc" -ne 0 ] && st=FAIL
    printf "  hotspot  rc=%s %s %ss\n" "$rc" "$st" "$wall"
    printf "elasticsearch-version\thotspot\t%s\t%s\t%s\t-\n" "$rc" "$st" "$wall" >>"$OUT"
  else
    echo "  hotspot  (skipped)"; printf "elasticsearch-version\thotspot\t-\tSKIP\t-\t-\n" >>"$OUT"
  fi
  echo
fi

[ -f "$ROOT/apps/gpu-bench/classes/CpuOnlyBench.class" ] && \
  run_suite "gpu-bench-cpu" "512m" "$ROOT/apps/gpu-bench/classes" CpuOnlyBench ""

GO="$ROOT/apps/_test-suites/gpu-offload"
[ -f "$GO/GpuProbe.class" ] && run_suite "gpu-offload-probe" "512m" "$GO" GpuProbe ""

BC="$ROOT/apps/_test-suites/bc-java"
BCCP="$BC/core/build/classes/java/main;$BC/core/build/classes/java/test;$BC/core/build/resources/main;$BC/core/build/resources/test"
if [ -d "$BC/core/build/classes/java/test" ]; then
  run_suite "bc-asn1-regression" "1g" "$BCCP" org.bouncycastle.asn1.test.RegressionTest ""
  run_suite "bc-crypto-prng" "1g" "$BCCP" org.bouncycastle.crypto.prng.test.RegressionTest ""
fi

CM="$ROOT/apps/_test-suites/commons-math"
JUNIT="$ROOT/.bench-cache/junit-platform-console-standalone-1.10.2.jar"
M2="${HOME}/.m2/repository"
# commons-math-transform/test-classes pull in commons-math3 + commons-rng-simple
# (transitive test dependencies declared in commons-math-transform/pom.xml).
# Without them, TransformUtilsTest.<clinit> fails with NoClassDefFoundError on
# org/apache/commons/math3/analysis/function/Sin during execute (discover only
# reads annotations and never triggers <clinit>, so the gap is invisible until
# the launcher tries to instantiate the test).
CMCP="$JUNIT;$CM/commons-math-transform/target/classes;$CM/commons-math-transform/target/test-classes;$CM/commons-math-core/target/classes;$M2/org/apache/commons/commons-numbers-rng/1.2/commons-numbers-rng-1.2.jar;$M2/org/apache/commons/commons-numbers-core/1.2/commons-numbers-core-1.2.jar"
for opt_jar in \
  "$M2/org/apache/commons/commons-numbers-complex/1.3/commons-numbers-complex-1.3.jar" \
  "$M2/org/apache/commons/commons-math3/3.6.1/commons-math3-3.6.1.jar" \
  "$M2/org/apache/commons/commons-rng-simple/1.7/commons-rng-simple-1.7.jar" \
  "$M2/org/apache/commons/commons-rng-client-api/1.7/commons-rng-client-api-1.7.jar" \
  "$M2/org/apache/commons/commons-rng-core/1.7/commons-rng-core-1.7.jar" \
; do
  [ -f "$opt_jar" ] && CMCP="$CMCP;$opt_jar"
done
[ -f "$ROOT/bench/JUnitProbe.class" ] && [ -d "$CM/commons-math-transform/target/test-classes" ] && \
  run_suite "commons-math-junit-probe" "2g" "$ROOT/bench;$CMCP" JUnitProbe ""

DAC="$ROOT/apps/_test-suites/dacapo"
if [ -f "$DAC/dacapo.jar" ]; then
  echo "== dacapo-avrora =="
  t0=$(date +%s%N)
  ( cd "$DAC" && timeout "$TIMEOUT" "$CV" --java-home "$JDK" --Xmx 4g -jar dacapo.jar avrora ) \
    >"$LOGDIR/dacapo-avrora-cratonvm.log" 2>&1
  rc=$?; t1=$(date +%s%N); wall=$(awk -v ms=$(((t1-t0)/1000000)) 'BEGIN{printf "%.1f", ms/1000}')
  st=PASS; grep -q PASSED "$LOGDIR/dacapo-avrora-cratonvm.log" || st=FAIL; [ "$rc" -ne 0 ] && st=FAIL
  note=$(grep -aoE "PASSED|FAILED|Exception|SEGV" "$LOGDIR/dacapo-avrora-cratonvm.log" | head -1)
  printf "  cratonvm rc=%s %s %ss %s\n" "$rc" "$st" "$wall" "$note"
  printf "dacapo-avrora\tcratonvm\t%s\t%s\t%s\t%s\n" "$rc" "$st" "$wall" "$note" >>"$OUT"
  if [ "$st" = PASS ] && [ "$rc" -eq 0 ]; then
    t0=$(date +%s%N)
    ( cd "$DAC" && timeout "$TIMEOUT" "$HS" -Xmx4g -jar dacapo.jar avrora ) >"$LOGDIR/dacapo-avrora-hotspot.log" 2>&1
    rc=$?; t1=$(date +%s%N); wall=$(awk -v ms=$(((t1-t0)/1000000)) 'BEGIN{printf "%.1f", ms/1000}')
    note=$(grep -aoE "PASSED|FAILED|Validation" "$LOGDIR/dacapo-avrora-hotspot.log" | head -1)
    printf "  hotspot  rc=%s %ss %s\n" "$rc" "$wall" "$note"
    printf "dacapo-avrora\thotspot\t%s\tN/S\t%s\t%s\n" "$rc" "$wall" "$note" >>"$OUT"
  fi
  echo
fi

for probe in kafka-codec spring-boot-run tomcat-server-info; do
  run_pool_probe "$probe"
done

{
  echo "# Apps folder — suite run results"
  echo; echo "**Date:** $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "**CratonVM:** \`$CV\`"; echo "**HotSpot:** \`$HS\`"
  echo "**Logs:** \`$LOGDIR/\`"; echo "**TSV:** \`$OUT\`"
  echo; echo "## Results"; echo
  echo "| Suite | CratonVM | HotSpot | Notes |"
  echo "|-------|----------|---------|-------|"
  awk -F'\t' 'NR>1{key=$1;vm=$2;st[key SUBSEP vm]=$4;wall[key SUBSEP vm]=$5;note[key SUBSEP vm]=$6;seen[key]=1}
    END{for(k in seen){cv=st[k SUBSEP "cratonvm"];hs=st[k SUBSEP "hotspot"];cwall=wall[k SUBSEP "cratonvm"];hwall=wall[k SUBSEP "hotspot"];cnote=note[k SUBSEP "cratonvm"];gsub(/\|/,"/",cnote);printf "| %s | %s (%ss) | %s (%ss) | %s |\n",k,cv,cwall,(hs==""?"—":hs),(hwall==""?"—":hwall),cnote}}' "$OUT" | sort
  echo; echo "## CratonVM crashes / failures"; echo
  awk -F'\t' 'NR>1 && $2=="cratonvm" && $4!="PASS"{gsub(/\|/,"/",$6);printf "- **%s** — %s (rc=%s, %ss): %s\n",$1,$4,$3,$5,$6}' "$OUT"
} > "$MD"
echo "Wrote $MD"; echo "TSV: $OUT"; echo "Done."
