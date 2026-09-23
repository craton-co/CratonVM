#!/usr/bin/env bash
# =============================================================================
#  Session comparison harness — built for the 2026-06-11 request:
#    §1 micro-benchmarks (6) in BOTH CratonVM JIT modes + HotSpot + TornadoVM
#    §1b JUnit Platform --help in the same 4 variants
#    §2 Bouncy Castle core suites           [cratonvm, hotspot, tornadovm]
#    §3 Apache Commons Math full reactor    [hotspot, tornadovm via Maven;
#                                            cratonvm via JUnit console launcher]
#  Produces TSVs + an aligned console render + a markdown summary.
#
#  Methodology notes (inherited from run-vm-comparison.sh):
#   * surefire's -Djvm is SILENTLY IGNORED, so a VM is measured by running Maven
#     ITSELF under that VM's JAVA_HOME (HotSpot / TornadoVM JDK can drive Maven).
#     CratonVM cannot drive Maven -> measured via the JUnit console launcher on
#     the transform module (honest partial), result recorded as-is.
#   * MSYS arg path-conversion is disabled so ';'-classpaths reach every .exe
#     verbatim (else a dropped jar would FAIL identically on HotSpot too).
#
#  Usage:
#    bash test-infra/run-session-cmp.sh                  # everything
#    SECTIONS="bench" bash test-infra/run-session-cmp.sh
#    SECTIONS=render bash test-infra/run-session-cmp.sh  # redraw last TSVs
# =============================================================================
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

ROOT="${ROOT:-C:/craton/CratonVM}"
# CV defaults to ROOT's binary but is overridable so we can run from a worktree
# build while keeping ROOT=main (which holds the compiled BC / commons-math classes,
# bench classes, and apps fixtures — build artifacts are not git-tracked).
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
[ -x "$JDK/bin/java.exe" ] || JDK="C:/craton/tornadovm/jdk-25.0.3"   # fallback JDK for --java-home
HOTSPOT="${HOTSPOT:-$(command -v java)}"
TORNADO="${TORNADO:-C:/craton/tornadovm/jdk-25.0.3/bin/java.exe}"
TORNADO_JH="C:/craton/tornadovm/jdk-25.0.3"
TORNADO_ARGF="@C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/tornado-argfile"
MVN="${MVN:-C:/tools/apache-maven-3.9.15/bin/mvn.cmd}"
BENCH_DIR="$ROOT/bench"
BC_DIR="$ROOT/apps/_test-suites/bc-java"
CM_DIR="$ROOT/apps/_test-suites/commons-math"
JUNIT3="${JUNIT3:-$TEMP/junit-3.8.2.jar}"
JUNIT_STANDALONE="$ROOT/.bench-cache/junit-platform-console-standalone-1.10.2.jar"

SECTIONS="${SECTIONS:-bench junit-help bc commons-math render}"
HEAP="${HEAP:-8g}"
TIMEOUT="${TIMEOUT:-360}"
BENCHES="${BENCHES:-arith1500M fib44 sieve250k matrix600 bintrees18 vadd2_28}"
BENCH_VARIANTS="${BENCH_VARIANTS:-cratonvm-jit cratonvm-nojit hotspot tornadovm}"

RESDIR="$ROOT/test-infra/suite-results"
mkdir -p "$RESDIR"
RUN_TS="${RUN_TS:-$(date +%Y%m%d-%H%M%S)}"
BENCH_TSV="$RESDIR/sess-bench-$RUN_TS.tsv"
JH_TSV="$RESDIR/sess-junit-help-$RUN_TS.tsv"
BC_TSV="$RESDIR/sess-bc-$RUN_TS.tsv"
CM_TSV="$RESDIR/sess-commons-math-$RUN_TS.tsv"
MD="$RESDIR/SESSION-COMPARISON-$RUN_TS.md"
echo "$BENCH_TSV" > "$RESDIR/.sess-latest-bench"
echo "$JH_TSV"    > "$RESDIR/.sess-latest-jh"
echo "$BC_TSV"    > "$RESDIR/.sess-latest-bc"
echo "$CM_TSV"    > "$RESDIR/.sess-latest-cm"
echo "$MD"        > "$RESDIR/.sess-latest-md"

hr()     { printf '%s\n' "------------------------------------------------------------------------------"; }
banner() { echo; hr; echo "## $*"; hr; }

kill_stray_cratonvm() {
  # Scope to the CV binary's OWN tree (the worktree), so a concurrent session
  # running cratonvm from a different checkout (e.g. main's target/release) is
  # left completely untouched. Derive the tree prefix from $CV.
  local cv_dir cv_win
  cv_dir=$(printf '%s' "$CV" | sed -E 's|/target/release/cratonvm\.exe$||')
  cv_win=$(printf '%s' "$cv_dir" | sed 's|/|\\\\|g')
  powershell.exe -NoProfile -Command \
    "Get-CimInstance Win32_Process -Filter \"Name='cratonvm.exe'\" |
       Where-Object { \$_.ExecutablePath -and \$_.ExecutablePath -like '${cv_win}*' } |
       ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" \
    >/dev/null 2>&1 || true
}

# ── variant -> java command prefix ──────────────────────────────────────────
jpre() {  # $1=variant  $2=heap
  local heap="${2:-$HEAP}"
  case "$1" in
    cratonvm-jit)   JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$heap");;
    cratonvm-nojit) JPRE=("$CV" --java-home "$JDK" --nojit --stack-dump-on-timeout 0 --Xmx "$heap");;
    cratonvm)       JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$heap");;
    hotspot)        JPRE=("$HOTSPOT" -Xmx"$heap");;
    tornadovm)      JPRE=("$TORNADO" "$TORNADO_ARGF" -Xmx"$heap");;
  esac
}

# ── §1 benchmarks ───────────────────────────────────────────────────────────
run_bench() {
  kill_stray_cratonvm
  printf "bench\tvariant\trc\tstate\tms\tchecksum\tsig\n" > "$BENCH_TSV"
  for b in $BENCHES; do
    banner "benchmark: $b"
    for v in $BENCH_VARIANTS; do
      jpre "$v"; local log=$(mktemp)
      timeout "$TIMEOUT" "${JPRE[@]}" -cp "$BENCH_DIR" BenchSuite "$b" </dev/null >"$log" 2>&1
      local rc=$? ms chk state=OK sig=""
      ms=$(grep -aoE "ms=[0-9]+" "$log" | head -1 | grep -oE "[0-9]+")
      chk=$(grep -aoE "checksum=-?[0-9]+" "$log" | head -1 | sed 's/checksum=//')
      if [ "$rc" -eq 124 ]; then state=TIMEOUT
      elif [ -z "$ms" ]; then state=CRASH
        sig=$(grep -aiE "stack overflow|SIGSEGV|SEGV|panic|OutOfMemory|implausible|Exception|fatal|abort|Error" "$log" | grep -avE "^\s*at " | head -1 | sed 's/\x1b\[[0-9;]*m//g' | head -c 90); fi
      printf "  %-16s rc=%-3s %-8s %9s ms  chk=%s %s\n" "$v" "$rc" "$state" "${ms:-—}" "${chk:-—}" "$sig"
      printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$b" "$v" "$rc" "$state" "${ms:-NA}" "${chk:-NA}" "$sig" >>"$BENCH_TSV"
      rm -f "$log"; kill_stray_cratonvm
    done
  done
}

# ── §1b JUnit Platform --help ───────────────────────────────────────────────
run_junit_help() {
  kill_stray_cratonvm
  printf "item\tvariant\trc\tstate\twall_s\tsummary\n" > "$JH_TSV"
  banner "JUnit Platform console --help"
  for v in $BENCH_VARIANTS; do
    jpre "$v" 4g; local log=$(mktemp); local t0=$(date +%s%N)
    timeout "$TIMEOUT" "${JPRE[@]}" -jar "$JUNIT_STANDALONE" --help </dev/null >"$log" 2>&1
    local rc=$?; local t1=$(date +%s%N)
    local wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
    local clean=$(sed 's/\x1b\[[0-9;]*m//g' "$log")
    local state=OK; [ "$rc" -eq 124 ] && state=TIMEOUT
    local good bad
    bad=$(printf '%s' "$clean" | grep -aoiE "System.exit\(-1\)|SIGSEGV|SEGV|panic|charset|index out of|stack overflow" | head -1)
    good=$(printf '%s' "$clean" | grep -aoiE "Usage: junit|ConsoleLauncher \[|Thanks for using JUnit|--help" | head -1)
    if [ "$state" = OK ] && { [ -n "$bad" ] || [ -z "$good" ]; }; then state=FAIL; fi
    local summary=$(printf '%s' "$clean" | grep -aiE "Usage:|SEGV|panic|charset|index out of|Exception|NoSuchMethod|Error" | grep -avE "^\s*at " | tail -1 | head -c 150)
    [ -z "$summary" ] && summary="(help banner printed)"
    printf "  %-16s rc=%-3s %-8s %6ss  %s\n" "$v" "$rc" "$state" "$wall_s" "$summary"
    printf "junit-help\t%s\t%s\t%s\t%s\t%s\n" "$v" "$rc" "$state" "$wall_s" "$summary" >>"$JH_TSV"
    rm -f "$log"; kill_stray_cratonvm
  done
}

# ── §2 Bouncy Castle ────────────────────────────────────────────────────────
run_bc() {
  kill_stray_cratonvm
  local VARS="${BC_VARIANTS:-cratonvm hotspot tornadovm}"
  local CP="$BC_DIR/core/build/classes/java/main;$BC_DIR/core/build/classes/java/test;$BC_DIR/core/build/resources/main;$BC_DIR/core/build/resources/test"
  local SUITES=(
    "asn1-regression|0|1g|org.bouncycastle.asn1.test.RegressionTest"
    "math-ec|1|1g|junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests"
    "math-raw|1|1g|junit.textui.TestRunner org.bouncycastle.math.raw.test.AllTests"
    "math|1|1g|junit.textui.TestRunner org.bouncycastle.math.test.AllTests"
    "crypto-regression|0|4g|org.bouncycastle.crypto.test.RegressionTest"
    "crypto-prng-regression|0|1g|org.bouncycastle.crypto.prng.test.RegressionTest"
    "pqc-crypto-regression|0|2g|org.bouncycastle.pqc.crypto.test.RegressionTest"
    "util-encoders|1|1g|junit.textui.TestRunner org.bouncycastle.util.encoders.test.AllTests"
  )
  printf "suite\tvariant\trc\tstate\twall_s\theap\tsummary\n" > "$BC_TSV"
  for entry in "${SUITES[@]}"; do
    IFS='|' read -r name needj heap main <<< "$entry"
    local cp="$CP"; [ "$needj" = "1" ] && cp="$CP;$JUNIT3"
    banner "bc: $name (heap=$heap)"
    for v in $VARS; do
      jpre "$v" "$heap"; local log=$(mktemp); local t0=$(date +%s%N)
      timeout "$TIMEOUT" "${JPRE[@]}" -cp "$cp" $main </dev/null >"$log" 2>&1
      local rc=$?; local t1=$(date +%s%N)
      local wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
      local state=OK; [ "$rc" -eq 124 ] && state=TIMEOUT
      local ok=$(grep -aoE "OK \([0-9]+ tests\)|: Okay" "$log" | head -1)
      [ "$state" = "OK" ] && [ -z "$ok" ] && state=FAIL
      local summary=$(grep -aiE "OK \(|: Okay|Tests run|Failures:|FAILED|Exception|SEGV|panic|stack overflow|Error" "$log" | grep -avE "^\s*at " | sed 's/\x1b\[[0-9;]*m//g' | tail -1 | head -c 150)
      printf "  %-11s rc=%-3s %-8s %6ss  heap=%-3s %s\n" "$v" "$rc" "$state" "$wall_s" "$heap" "$summary"
      printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$name" "$v" "$rc" "$state" "$wall_s" "$heap" "$summary" >>"$BC_TSV"
      rm -f "$log"; kill_stray_cratonvm
    done
  done
}

# ── §3 Commons Math full reactor ────────────────────────────────────────────
run_commons_math() {
  kill_stray_cratonvm
  local VARS="${CM_VARIANTS:-hotspot tornadovm cratonvm}"
  printf "variant\trc\twall_s\ttests\tfailures\terrors\tskipped\tbuild\n" > "$CM_TSV"
  local SKIPS="-Drat.skip=true -Dcheckstyle.skip=true -Dspotbugs.skip=true -Dpmd.skip=true -Denforcer.skip=true -Dmaven.javadoc.skip=true -Danimal.sniffer.skip=true"
  for v in $VARS; do
    banner "commons-math full reactor on $v"
    local log="$RESDIR/sess-cm-$v-$RUN_TS.log"; local t0=$(date +%s%N)
    local rc tests fails errs skip build wall_s line
    if [ "$v" = "cratonvm" ]; then
      local CP="$JUNIT_STANDALONE;$CM_DIR/commons-math-transform/target/classes;$CM_DIR/commons-math-transform/target/test-classes;$CM_DIR/commons-math-core/target/classes"
      for nj in \
        "$HOME/.m2/repository/org/apache/commons/commons-numbers-core/1.3/commons-numbers-core-1.3.jar" \
        "$HOME/.m2/repository/org/apache/commons/commons-numbers-complex/1.3/commons-numbers-complex-1.3.jar" \
        "$HOME/.m2/repository/org/apache/commons/commons-rng-simple/1.7/commons-rng-simple-1.7.jar" \
        "$HOME/.m2/repository/org/apache/commons/commons-rng-client-api/1.7/commons-rng-client-api-1.7.jar" \
        "$HOME/.m2/repository/org/apache/commons/commons-rng-core/1.7/commons-rng-core-1.7.jar" \
        "$HOME/.m2/repository/org/apache/commons/commons-math3/3.6.1/commons-math3-3.6.1.jar" \
      ; do [ -f "$nj" ] && CP="$CP;$nj"; done
      ( cd "$CM_DIR" && timeout "${CM_TIMEOUT:-1200}" "$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 2g \
          -cp "$CP" org.junit.platform.console.ConsoleLauncher execute \
          --select-package org.apache.commons.math4.transform --details=summary --disable-banner ) >"$log" 2>&1
      rc=$?
      tests=$(grep -aoE "[0-9]+ tests found" "$log" | grep -oE "[0-9]+" | head -1)
      fails=$(grep -aoE "[0-9]+ tests failed" "$log" | grep -oE "[0-9]+" | head -1)
      if grep -qaiE "BufferedWriter.write|SIGSEGV|panic|stack overflow" "$log"; then build="CRASH/BLOCKED"
      elif [ -n "$tests" ]; then build="RAN(transform-only)"; else build="NO-TESTS"; fi
    else
      local jh="$JDK"; [ "$v" = "tornadovm" ] && jh="$TORNADO_JH"
      ( cd "$CM_DIR" && JAVA_HOME="$jh" timeout "${CM_TIMEOUT:-1800}" "$MVN" -o test \
          -Dmaven.test.failure.ignore=true $SKIPS ) >"$log" 2>&1
      rc=$?
      line=$(grep -aE "Tests run: [0-9]+, Failures:" "$log" | tail -1)
      tests=$(echo "$line" | grep -aoE "Tests run: [0-9]+" | grep -oE "[0-9]+")
      fails=$(echo "$line" | grep -aoE "Failures: [0-9]+" | grep -oE "[0-9]+")
      errs=$(echo "$line"  | grep -aoE "Errors: [0-9]+"  | grep -oE "[0-9]+")
      skip=$(echo "$line"  | grep -aoE "Skipped: [0-9]+" | grep -oE "[0-9]+")
      build=$(grep -aoE "BUILD (SUCCESS|FAILURE)" "$log" | tail -1)
    fi
    local t1=$(date +%s%N); wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
    [ "$rc" -eq 124 ] && build="TIMEOUT"
    printf "  %-10s rc=%s wall=%ss tests=%s fail=%s skip=%s  %s\n" "$v" "$rc" "$wall_s" "${tests:-?}" "${fails:-?}" "${skip:-?}" "${build:-?}"
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$v" "$rc" "$wall_s" "${tests:-NA}" "${fails:-NA}" "${errs:-NA}" "${skip:-NA}" "${build:-NA}" >>"$CM_TSV"
  done
}

# ── RENDER ──────────────────────────────────────────────────────────────────
pivot() {
  local tsv="$1" rc="$2" vc="$3" valc="$4" title="$5"
  [ -f "$tsv" ] || { echo "  (no data: $tsv)"; return; }
  awk -F'\t' -v RC="$rc" -v VC="$vc" -v VALC="$valc" -v TITLE="$title" '
    NR==1 { next }
    { row=$RC; var=$VC; val=$VALC;
      if(!(row in seenrow)){ seenrow[row]=1; rows[++nr]=row }
      if(!(var in seenvar)){ seenvar[var]=1; vars[++nv]=var }
      cell[row SUBSEP var]=val }
    END{
      w0=length(TITLE); for(i=1;i<=nr;i++) if(length(rows[i])>w0) w0=length(rows[i]);
      for(j=1;j<=nv;j++){ wv[j]=length(vars[j]); for(i=1;i<=nr;i++){ c=cell[rows[i] SUBSEP vars[j]]; if(length(c)>wv[j]) wv[j]=length(c) } }
      printf "| %-*s", w0, TITLE; for(j=1;j<=nv;j++) printf " | %-*s", wv[j], vars[j]; printf " |\n";
      printf "|"; for(k=0;k<w0+2;k++) printf "-"; for(j=1;j<=nv;j++){ printf "|"; for(k=0;k<wv[j]+2;k++) printf "-" } printf "|\n";
      for(i=1;i<=nr;i++){ printf "| %-*s", w0, rows[i]; for(j=1;j<=nv;j++){ c=cell[rows[i] SUBSEP vars[j]]; if(c=="") c="·"; printf " | %-*s", wv[j], c } printf " |\n" }
    }' "$tsv"
}

render() {
  local bt=$(cat "$RESDIR/.sess-latest-bench" 2>/dev/null)
  local jh=$(cat "$RESDIR/.sess-latest-jh" 2>/dev/null)
  local bc=$(cat "$RESDIR/.sess-latest-bc" 2>/dev/null)
  local cm=$(cat "$RESDIR/.sess-latest-cm" 2>/dev/null)
  local md=$(cat "$RESDIR/.sess-latest-md" 2>/dev/null)
  {
    echo "# Cross-VM comparison — session run"
    echo
    echo "- **CratonVM:** \`$CV\`"
    echo "- **HotSpot:** \`$HOTSPOT\`"
    echo "- **TornadoVM:** \`$TORNADO\` (argfile)"
    echo "- **Heap:** $HEAP (bench), per-suite for BC; **timeout:** ${TIMEOUT}s"
    echo
    if [ -f "$bt" ]; then
      echo "## §1 Micro-benchmarks — wall time (ms)"; echo
      pivot "$bt" 1 2 5 "benchmark"; echo
      echo "## §1 Micro-benchmarks — state"; echo
      pivot "$bt" 1 2 4 "benchmark"; echo
      echo "## §1 Micro-benchmarks — checksum (must match across a row)"; echo
      pivot "$bt" 1 2 6 "benchmark"; echo
    fi
    if [ -f "$jh" ]; then
      echo "## §1b JUnit Platform --help — state / wall(s)"; echo
      pivot "$jh" 1 2 4 "item"; echo
      pivot "$jh" 1 2 5 "item"; echo
    fi
    if [ -f "$bc" ]; then
      echo "## §2 Bouncy Castle — state"; echo
      pivot "$bc" 1 2 4 "bc-suite"; echo
      echo "## §2 Bouncy Castle — wall (s)"; echo
      pivot "$bc" 1 2 5 "bc-suite"; echo
    fi
    if [ -f "$cm" ]; then
      echo "## §3 Commons Math full reactor"; echo
      echo "| variant | rc | wall(s) | tests | fail | skip | build |"
      echo "|---------|----|---------|-------|------|------|-------|"
      awk -F'\t' 'NR==1{next}{printf "| %s | %s | %s | %s | %s | %s | %s |\n",$1,$2,$3,$4,$5,$7,$8}' "$cm"
      echo
    fi
  } | tee "$md"
  echo "Wrote markdown: $md"
}

echo "Session comparison — sections: $SECTIONS"
echo "  CratonVM : $CV"
echo "  HotSpot  : $HOTSPOT"
echo "  TornadoVM: $TORNADO"
for s in $SECTIONS; do
  case "$s" in
    bench)        run_bench;;
    junit-help)   run_junit_help;;
    bc)           run_bc;;
    commons-math) run_commons_math;;
    render)       render;;
    *) echo "unknown section: $s";;
  esac
done
echo "Done — $(date)"
