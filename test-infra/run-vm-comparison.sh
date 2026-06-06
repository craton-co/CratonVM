#!/usr/bin/env bash
# =============================================================================
#  CratonVM cross-VM comparison harness  (run manually)
# =============================================================================
#  Modes:  CratonVM-CPU (JIT)   CratonVM-GPU (--gpu)   HotSpot   TornadoVM
#
#  Runs:
#    1. Numeric micro-benchmark suite  (BenchSuite: arith/fib/sieve/matrix/
#       bintrees/vector-add) — wall ms + checksum correctness.  [4 modes]
#    2. Bouncy Castle core test suites  (ASN.1, math, crypto, pqc, util). [3 VMs]
#    3. Apache Commons Math full reactor (3204 JUnit5 tests). HotSpot & Tornado
#       run Maven under their own JAVA_HOME (genuine); CratonVM is measured via
#       the JUnit console launcher (currently BLOCKED — BufferedWriter.write gap).
#    4. Extras: JUnit Platform console --help, DaCapo avrora.            [4 modes]
#
#  (BC/Commons-Math are correctness suites; GPU offload only triggers on
#   eligible numeric kernels, so they run on the CPU build — adding a GPU
#   column there would be identical work. The CPU/GPU split is the benchmark
#   column.)
#
#  PREREQUISITE — the GPU binary must be built first:
#    cargo build --release -p cratonvm-cli --bin cratonvm \
#        --features gpu-driver --target-dir target-gpu
#
#  Every run is GENUINE per-VM: one process per (workload, variant); the only
#  thing that changes between variants is the `java` binary in front.
#
#  At the end it prints aligned comparison tables (and the raw TSV paths).
#
#  Usage:
#    bash test-infra/run-vm-comparison.sh                # everything
#    SECTIONS="bench bc" bash test-infra/run-vm-comparison.sh
#    SECTIONS=render bash test-infra/run-vm-comparison.sh # just redraw last run
#    BENCHES="fib44 sieve250k" VARIANTS="cratonvm-jit hotspot" bash ...
#
#  Env knobs: SECTIONS, VARIANTS, BENCHES, CM_MODULE, HEAP, TIMEOUT
# =============================================================================
set +e

# ---------------------------------------------------------------------------
# HARNESS ARTIFACT GUARD: under MSYS/Git-bash, command-line arguments that look
# like POSIX path lists get auto-converted on the way to a native .exe. A
# ';'-separated Windows -cp can be mangled so a classpath entry (e.g. the junit
# jar) is dropped, making the main class "not found" and the suite spuriously
# FAIL — identically on HotSpot, proving it is NOT a VM bug. Disabling arg
# path-conversion passes every classpath through verbatim to every VM.
export MSYS2_ARG_CONV_EXCL='*'   # MSYS2 / newer Git-bash
export MSYS_NO_PATHCONV=1        # Git-for-Windows bash

# ---- paths -----------------------------------------------------------------
ROOT="${ROOT:-C:/craton/CratonVM}"
CV="$ROOT/target/release/cratonvm.exe"            # CPU build (JIT)
CV_GPU="$ROOT/target-gpu/release/cratonvm.exe"    # GPU build (--features gpu-driver)
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
HOTSPOT="$JDK/bin/java.exe"
TORNADO="${TORNADO:-C:/craton/tornadovm/jdk-25.0.3/bin/java.exe}"
TORNADO_ARGF="@C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/tornado-argfile"
CV_SHIM="$ROOT/test-infra/cratonvm-java-shim.bat"
MVN="${MVN:-C:/tools/apache-maven-3.9.15/bin/mvn.cmd}"
BENCH_DIR="$ROOT/bench"
BC_DIR="$ROOT/apps/_test-suites/bc-java"
CM_DIR="$ROOT/apps/_test-suites/commons-math"
JUNIT3="${JUNIT3:-$TEMP/junit-3.8.2.jar}"
JUNIT_STANDALONE="$ROOT/.bench-cache/junit-platform-console-standalone-1.10.2.jar"
DACAPO_DIR="$ROOT/apps/_test-suites/dacapo"

# ---- knobs -----------------------------------------------------------------
SECTIONS="${SECTIONS:-bench bc commons-math extras render}"
HEAP="${HEAP:-8g}"
TIMEOUT="${TIMEOUT:-360}"
BENCHES="${BENCHES:-arith1500M fib44 sieve250k matrix600 bintrees18 vadd2_28}"
CM_MODULE="${CM_MODULE:-commons-math-legacy}"

RESDIR="$ROOT/test-infra/suite-results"
mkdir -p "$RESDIR"
RUN_TS=$(date +%Y%m%d-%H%M%S)
BENCH_TSV="$RESDIR/cmp-bench-$RUN_TS.tsv"
BC_TSV="$RESDIR/cmp-bc-$RUN_TS.tsv"
CM_TSV="$RESDIR/cmp-commons-math-$RUN_TS.tsv"
EX_TSV="$RESDIR/cmp-extras-$RUN_TS.tsv"
# remember newest of each for the render pass
ln_latest() { printf '%s\n' "$2" > "$RESDIR/.latest-$1"; }
get_latest() { cat "$RESDIR/.latest-$1" 2>/dev/null; }

hr() { printf '%s\n' "------------------------------------------------------------------------------"; }
banner() { echo; hr; echo "## $*"; hr; }

# ===========================================================================
#  1. micro-benchmark suite
# ===========================================================================
bench_prefix() {
  case "$1" in
    cratonvm-cpu)  JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$HEAP");;
    cratonvm-gpu)  JPRE=("$CV_GPU" --gpu --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$HEAP");;
    hotspot)       JPRE=("$HOTSPOT" -Xmx$HEAP);;
    tornadovm)     JPRE=("$TORNADO" "$TORNADO_ARGF" -Xmx$HEAP);;
  esac
}
run_bench() {
  local VARS="${VARIANTS:-cratonvm-cpu cratonvm-gpu hotspot tornadovm}"
  printf "bench\tvariant\trc\tstate\tms\tchecksum\tsig\n" > "$BENCH_TSV"
  ln_latest bench "$BENCH_TSV"
  for b in $BENCHES; do
    banner "benchmark: $b"
    for v in $VARS; do
      bench_prefix "$v"
      local log=$(mktemp)
      timeout "$TIMEOUT" "${JPRE[@]}" -cp "$BENCH_DIR" BenchSuite "$b" </dev/null >"$log" 2>&1
      local rc=$? ms chk state=OK sig=""
      ms=$(grep -aoE "ms=[0-9]+" "$log" | head -1 | grep -oE "[0-9]+")
      chk=$(grep -aoE "checksum=-?[0-9]+" "$log" | head -1 | sed 's/checksum=//')
      if [ "$rc" -eq 124 ]; then state=TIMEOUT
      elif [ -z "$ms" ]; then state=CRASH
        sig=$(grep -aiE "stack overflow|SIGSEGV|SEGV|panic|OutOfMemory|implausible|Exception|fatal|abort|Error" "$log" | grep -avE "^\s*at " | head -1 | sed 's/\x1b\[[0-9;]*m//g' | head -c 90); fi
      printf "  %-15s rc=%-3s %-8s %8s ms  chk=%s %s\n" "$v" "$rc" "$state" "${ms:-—}" "${chk:-—}" "$sig"
      printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$b" "$v" "$rc" "$state" "${ms:-NA}" "${chk:-NA}" "$sig" >>"$BENCH_TSV"
      rm -f "$log"
    done
  done
}

# ===========================================================================
#  2. Bouncy Castle core suites
# ===========================================================================
# Heap is per-suite (4th '|' field) so a memory-hungry suite gets a fair
# allotment on EVERY VM: crypto-regression OOMs at 1g on HotSpot too, so 1g
# would be an unfair test there, not a CratonVM failure.
bc_prefix() {
  local heap="${2:-1g}"
  case "$1" in
    cratonvm)  JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$heap");;
    hotspot)   JPRE=("$HOTSPOT" -Xmx"$heap");;
    tornadovm) JPRE=("$TORNADO" -Xmx"$heap");;
  esac
}
run_bc() {
  local VARS="${VARIANTS:-cratonvm hotspot tornadovm}"
  local CP="$BC_DIR/core/build/classes/java/main;$BC_DIR/core/build/classes/java/test;$BC_DIR/core/build/resources/main;$BC_DIR/core/build/resources/test"
  # name | needs-junit(0/1) | heap | mainclass-and-args
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
  ln_latest bc "$BC_TSV"
  for entry in "${SUITES[@]}"; do
    IFS='|' read -r name needj heap main <<< "$entry"
    local cp="$CP"; [ "$needj" = "1" ] && cp="$CP;$JUNIT3"
    banner "bc: $name (heap=$heap)"
    for v in $VARS; do
      bc_prefix "$v" "$heap"
      local log=$(mktemp); local t0=$(date +%s%N)
      timeout "$TIMEOUT" "${JPRE[@]}" -cp "$cp" $main </dev/null >"$log" 2>&1
      local rc=$?; local t1=$(date +%s%N)
      local wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
      local state=OK; [ "$rc" -eq 124 ] && state=TIMEOUT
      local ok=$(grep -aoE "OK \([0-9]+ tests\)|: Okay" "$log" | head -1)
      [ "$state" = "OK" ] && [ -z "$ok" ] && state=FAIL
      local summary=$(grep -aiE "OK \(|: Okay|Tests run|Failures:|FAILED|Exception|SEGV|panic|stack overflow|Error" "$log" | grep -avE "^\s*at " | sed 's/\x1b\[[0-9;]*m//g' | tail -1 | head -c 150)
      printf "  %-11s rc=%-3s %-8s %6ss  heap=%-3s %s\n" "$v" "$rc" "$state" "$wall_s" "$heap" "$summary"
      printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$name" "$v" "$rc" "$state" "$wall_s" "$heap" "$summary" >>"$BC_TSV"
      rm -f "$log"
    done
  done
}

# ===========================================================================
#  3. Commons Math (full JUnit5 reactor) — measured the HONEST way.
#  NOTE: surefire's -Djvm is SILENTLY IGNORED here (verified: a nonexistent path
#  still BUILD SUCCESS), so the only faithful way to measure a VM is to run Maven
#  ITSELF under that VM's JAVA_HOME. HotSpot and the TornadoVM JDK can drive
#  Maven. CratonVM cannot drive Maven, and its JUnit-Platform launcher crashes
#  (NoSuchMethodError java/io/BufferedWriter.write([BII)V) — so the JUnit5 suite
#  does NOT run on CratonVM; we record that verdict instead of a fake pass.
# ===========================================================================
run_commons_math() {
  local VARS="${VARIANTS:-hotspot tornadovm cratonvm}"
  printf "variant\trc\twall_s\ttests\tfailures\terrors\tskipped\tflakes\tbuild\n" > "$CM_TSV"
  ln_latest commons-math "$CM_TSV"
  local SKIPS="-Drat.skip=true -Dcheckstyle.skip=true -Dspotbugs.skip=true -Dpmd.skip=true -Denforcer.skip=true -Dmaven.javadoc.skip=true -Danimal.sniffer.skip=true"
  for v in $VARS; do
    banner "commons-math full reactor on $v"
    local log="$RESDIR/cm-full-$v-$RUN_TS.log"; local t0=$(date +%s%N)
    local rc tests fails errs skip flak build wall_s line
    if [ "$v" = "cratonvm" ]; then
      # genuine direct run via JUnit Platform console launcher under cratonvm
      local CP="$JUNIT_STANDALONE;$CM_DIR/commons-math-transform/target/classes;$CM_DIR/commons-math-transform/target/test-classes;$CM_DIR/commons-math-core/target/classes"
      ( cd "$CM_DIR" && timeout "${CM_TIMEOUT:-1200}" "$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 2g \
          -cp "$CP" org.junit.platform.console.ConsoleLauncher execute \
          --select-package org.apache.commons.math4.transform --details=summary --disable-banner ) >"$log" 2>&1
      rc=$?
      tests=$(grep -aoE "[0-9]+ tests found" "$log" | grep -oE "[0-9]+" | head -1)
      fails=$(grep -aoE "[0-9]+ tests failed" "$log" | grep -oE "[0-9]+" | head -1)
      if grep -qa "BufferedWriter.write" "$log"; then build="BLOCKED(BufferedWriter.write[BII]V)"
      elif [ -n "$tests" ]; then build="RAN(transform-only)"; else build="CRASH"; fi
    else
      local jh="$JDK"; [ "$v" = "tornadovm" ] && jh="C:/craton/tornadovm/jdk-25.0.3"
      ( cd "$CM_DIR" && JAVA_HOME="$jh" timeout "${CM_TIMEOUT:-1200}" "$MVN" -o test \
          -Dmaven.test.failure.ignore=true $SKIPS ) >"$log" 2>&1
      rc=$?
      line=$(grep -aE "Tests run: [0-9]+, Failures:" "$log" | tail -1)
      tests=$(echo "$line" | grep -aoE "Tests run: [0-9]+" | grep -oE "[0-9]+")
      fails=$(echo "$line" | grep -aoE "Failures: [0-9]+" | grep -oE "[0-9]+")
      errs=$(echo "$line"  | grep -aoE "Errors: [0-9]+"  | grep -oE "[0-9]+")
      skip=$(echo "$line"  | grep -aoE "Skipped: [0-9]+" | grep -oE "[0-9]+")
      flak=$(echo "$line"  | grep -aoE "Flakes: [0-9]+"  | grep -oE "[0-9]+")
      build=$(grep -aoE "BUILD (SUCCESS|FAILURE)" "$log" | tail -1)
    fi
    local t1=$(date +%s%N); wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
    [ "$rc" -eq 124 ] && build="TIMEOUT"
    printf "  %-10s rc=%s wall=%ss tests=%s fail=%s skip=%s flakes=%s  %s\n" "$v" "$rc" "$wall_s" "${tests:-?}" "${fails:-?}" "${skip:-?}" "${flak:-0}" "${build:-?}"
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$v" "$rc" "$wall_s" "${tests:-NA}" "${fails:-NA}" "${errs:-NA}" "${skip:-NA}" "${flak:-0}" "${build:-NA}" >>"$CM_TSV"
  done
}

# ===========================================================================
#  4. Extras (JUnit Platform --help, DaCapo avrora)
# ===========================================================================
ex_prefix() {
  case "$1" in
    cratonvm-cpu)  JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 4g);;
    cratonvm-gpu)  JPRE=("$CV_GPU" --gpu --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 4g);;
    hotspot)       JPRE=("$HOTSPOT" -Xmx4g);;
    tornadovm)     JPRE=("$TORNADO" -Xmx4g);;
  esac
}
run_extras() {
  local VARS="${VARIANTS:-cratonvm-cpu cratonvm-gpu hotspot tornadovm}"
  printf "item\tvariant\trc\tstate\twall_s\tsummary\n" > "$EX_TSV"
  ln_latest extras "$EX_TSV"
  local items="junit-help dacapo-avrora"
  for item in $items; do
    banner "$item"
    for v in $VARS; do
      ex_prefix "$v"
      local log=$(mktemp); local t0=$(date +%s%N)
      case "$item" in
        junit-help)    timeout "$TIMEOUT" "${JPRE[@]}" -jar "$JUNIT_STANDALONE" --help </dev/null >"$log" 2>&1;;
        dacapo-avrora) ( cd "$DACAPO_DIR" && timeout "$TIMEOUT" "${JPRE[@]}" -jar dacapo.jar avrora </dev/null ) >"$log" 2>&1;;
      esac
      local rc=$?; local t1=$(date +%s%N)
      local wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
      local state=OK; [ "$rc" -eq 124 ] && state=TIMEOUT
      # ANSI-stripped view of the log — the JUnit console launcher colourises its
      # help banner, so the "Usage:" line is NOT at column 0 in the raw bytes.
      local clean=$(sed 's/\x1b\[[0-9;]*m//g' "$log")
      case "$item" in
        junit-help)
          # a NoSuchMethodError / System.exit(-1) crash still prints partial
          # output; require BOTH a clean rc AND the help banner before calling OK.
          # Match "Usage: junit" anywhere on the line (no ^ anchor) post-ANSI-strip.
          local good bad
          bad=$(printf '%s' "$clean" | grep -aoiE "NoSuchMethodError|System.exit\(-1\)|SIGSEGV|panic" | head -1)
          good=$(printf '%s' "$clean" | grep -aoiE "Usage: junit|ConsoleLauncher \[|Thanks for using JUnit" | head -1)
          if [ "$state" = "OK" ] && { [ -n "$bad" ] || [ -z "$good" ]; }; then state=FAIL; fi
          ;;
        dacapo-avrora)
          # KNOWN NON-SIGNAL. DaCapo-9.12 validates a run by SHA-1-digesting
          # stderr.log and comparing it to da39a3ee…0709 (the digest of the EMPTY
          # string). JDK 25 writes warnings to stderr, so "Validation FAILED"
          # fires on HotSpot AND TornadoVM too — it is a DaCapo-vs-JDK25 artifact,
          # not a VM signal (and CratonVM's occasional PASS is luck: its stderr
          # happened to be empty). Don't score it pass/fail; only surface a
          # genuine VM crash. State "N/S" = non-signal.
          if [ "$state" = "OK" ]; then
            local crash
            crash=$(printf '%s' "$clean" | grep -aoiE "SIGSEGV|panic|abort|fatal error|NoSuchMethodError" | head -1)
            if [ -n "$crash" ]; then state=FAIL; else state="N/S"; fi
          fi
          ;;
      esac
      local summary=$(grep -aiE "PASSED|FAILED|Usage:|SEGV|panic|stack overflow|OutOfMemory|charset|index out of|Exception|fatal|Error" "$log" | grep -avE "^\s*at " | sed 's/\x1b\[[0-9;]*m//g' | tail -1 | head -c 150)
      printf "  %-15s rc=%-3s %-8s %6ss  %s\n" "$v" "$rc" "$state" "$wall_s" "$summary"
      printf "%s\t%s\t%s\t%s\t%s\t%s\n" "$item" "$v" "$rc" "$state" "$wall_s" "$summary" >>"$EX_TSV"
      rm -f "$log"
    done
  done
}

# ===========================================================================
#  RENDER — pivot TSVs into aligned comparison tables
# ===========================================================================
# generic pivot: $1=tsv $2=rowcol(1-based) $3=variantcol $4=valuecol $5=title
pivot() {
  local tsv="$1" rc="$2" vc="$3" valc="$4" title="$5"
  [ -f "$tsv" ] || { echo "  (no data: $tsv)"; return; }
  awk -F'\t' -v RC="$rc" -v VC="$vc" -v VALC="$valc" -v TITLE="$title" '
    NR==1 { next }
    {
      row=$RC; var=$VC; val=$VALC;
      if(!(row in seenrow)){ seenrow[row]=1; rows[++nr]=row }
      if(!(var in seenvar)){ seenvar[var]=1; vars[++nv]=var }
      cell[row SUBSEP var]=val
    }
    END{
      # column widths
      w0=length(TITLE); for(i=1;i<=nr;i++) if(length(rows[i])>w0) w0=length(rows[i]);
      for(j=1;j<=nv;j++){ wv[j]=length(vars[j]); for(i=1;i<=nr;i++){ c=cell[rows[i] SUBSEP vars[j]]; if(length(c)>wv[j]) wv[j]=length(c) } }
      # header
      printf "  %-*s", w0, TITLE; for(j=1;j<=nv;j++) printf " | %-*s", wv[j], vars[j]; printf "\n";
      printf "  "; for(k=0;k<w0;k++) printf "-"; for(j=1;j<=nv;j++){ printf "-+-"; for(k=0;k<wv[j];k++) printf "-" } printf "\n";
      # rows
      for(i=1;i<=nr;i++){ printf "  %-*s", w0, rows[i]; for(j=1;j<=nv;j++){ c=cell[rows[i] SUBSEP vars[j]]; if(c=="") c="·"; printf " | %-*s", wv[j], c } printf "\n" }
    }' "$tsv"
}

render() {
  local bt=$(get_latest bench) bc=$(get_latest bc) cm=$(get_latest commons-math) ex=$(get_latest extras)

  banner "COMPARISON TABLES"

  if [ -f "$bt" ]; then
    echo; echo "### Micro-benchmarks — wall time (ms)"; echo
    pivot "$bt" 1 2 5 "benchmark"
    echo; echo "### Micro-benchmarks — state (OK/CRASH/TIMEOUT)"; echo
    pivot "$bt" 1 2 4 "benchmark"
    echo; echo "### Micro-benchmarks — checksum (must be identical across a row)"; echo
    pivot "$bt" 1 2 6 "benchmark"
  fi

  if [ -f "$bc" ]; then
    echo; echo "### Bouncy Castle suites — state"; echo
    pivot "$bc" 1 2 4 "bc-suite"
    echo; echo "### Bouncy Castle suites — wall (s)"; echo
    pivot "$bc" 1 2 5 "bc-suite"
    echo; echo "### Bouncy Castle suites — heap (per-suite, same across VMs)"; echo
    pivot "$bc" 1 2 6 "bc-suite"
  fi

  if [ -f "$cm" ]; then
    echo; echo "### Commons Math full reactor — tests / fail / skip / build"; echo
    awk -F'\t' 'NR==1{next}{printf "  %-10s tests=%-6s fail=%-4s skip=%-4s flakes=%-3s wall=%-7ss %s\n",$1,$4,$5,$7,$8,$3,$9}' "$cm"
  fi

  if [ -f "$ex" ]; then
    echo; echo "### Extras — state"; echo
    pivot "$ex" 1 2 4 "item"
  fi

  echo; hr
  echo "Raw TSVs:"
  [ -f "$bt" ] && echo "  bench:        $bt"
  [ -f "$bc" ] && echo "  bc:           $bc"
  [ -f "$cm" ] && echo "  commons-math: $cm"
  [ -f "$ex" ] && echo "  extras:       $ex"
  hr
}

# ===========================================================================
#  driver
# ===========================================================================
echo "VM comparison harness — sections: $SECTIONS"
echo "  CratonVM : $CV"
echo "  HotSpot  : $HOTSPOT"
echo "  TornadoVM: $TORNADO"
for s in $SECTIONS; do
  case "$s" in
    bench)        run_bench;;
    bc)           run_bc;;
    commons-math) run_commons_math;;
    extras)       run_extras;;
    render)       render;;
    *) echo "unknown section: $s";;
  esac
done
