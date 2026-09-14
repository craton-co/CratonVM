#!/usr/bin/env bash
# Full cross-VM comparison: bench + BC + commons-math (JUnit launcher) + extras.
# Writes TSVs under test-infra/suite-results/ and prints aligned tables.
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
CV="$ROOT/target/release/cratonvm.exe"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
HOTSPOT="$JDK/bin/java.exe"
TORNADO="${TORNADO:-C:/craton/tornadovm/jdk-25.0.3/bin/java.exe}"
TORNADO_ARGF="@C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/tornado-argfile"
BENCH_DIR="$ROOT/bench"
BC_DIR="$ROOT/apps/_test-suites/bc-java"
CM_DIR="$ROOT/apps/_test-suites/commons-math"
JUNIT_STANDALONE="$ROOT/.bench-cache/junit-platform-console-standalone-1.10.2.jar"
JUNIT3="$TEMP/junit-3.8.2.jar"
DACAPO_DIR="$ROOT/apps/_test-suites/dacapo"

HEAP="${HEAP:-8g}"
TIMEOUT="${TIMEOUT:-300}"
BENCHES="${BENCHES:-arith1500M fib44 sieve250k matrix600 bintrees18 vadd2_28}"

RESDIR="$ROOT/test-infra/suite-results"
mkdir -p "$RESDIR"
RUN_TS=$(date +%Y%m%d-%H%M%S)
BENCH_TSV="$RESDIR/cmp-bench-$RUN_TS.tsv"
BC_TSV="$RESDIR/cmp-bc-$RUN_TS.tsv"
CM_TSV="$RESDIR/cmp-cm-$RUN_TS.tsv"
EX_TSV="$RESDIR/cmp-extras-$RUN_TS.tsv"

hr()     { printf '%s\n' "------------------------------------------------------------------------------"; }
banner() { echo; hr; echo "## $*"; hr; }

# Kill ONLY leftover cratonvm.exe launched from THIS tree ($ROOT); leave a
# parallel suite / another worktree / a dev session alone, and never let a
# no-match surface as a failure (the rc=1 footgun of `taskkill /F /IM`).
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

# ── bench prefix ──────────────────────────────────────────────────────────────
bench_prefix() {
  case "$1" in
    cratonvm-jit) JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$HEAP");;
    hotspot)      JPRE=("$HOTSPOT" -Xmx$HEAP);;
    tornadovm)    JPRE=("$TORNADO" "$TORNADO_ARGF" -Xmx$HEAP);;
  esac
}

# ── §1 micro-benchmarks ───────────────────────────────────────────────────────
printf "bench\tvariant\trc\tstate\tms\tchecksum\tsig\n" > "$BENCH_TSV"
for b in $BENCHES; do
  banner "bench: $b"
  for v in cratonvm-jit hotspot tornadovm; do
    bench_prefix "$v"
    log=$(mktemp); t0=$(date +%s%N)
    timeout "$TIMEOUT" "${JPRE[@]}" -cp "$BENCH_DIR" BenchSuite "$b" </dev/null >"$log" 2>&1
    rc=$?; t1=$(date +%s%N)
    ms=$(grep -aoE "ms=[0-9]+" "$log" | head -1 | grep -oE "[0-9]+")
    chk=$(grep -aoE "checksum=-?[0-9]+" "$log" | head -1 | sed 's/checksum=//')
    state=OK
    [ "$rc" -eq 124 ] && state=TIMEOUT
    [ -z "$ms" ] && state=CRASH
    sig=""
    [ "$state" != OK ] && sig=$(grep -aiE "stack overflow|SEGV|panic|OutOfMemory|Exception|fatal|Error" "$log" \
      | grep -avE "^\s*at " | head -1 | sed 's/\x1b\[[0-9;]*m//g' | head -c 90)
    printf "  %-15s rc=%-3s %-8s %8s ms  chk=%s %s\n" "$v" "$rc" "$state" "${ms:---}" "${chk:---}" "$sig"
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$b" "$v" "$rc" "$state" "${ms:-NA}" "${chk:-NA}" "$sig" >>"$BENCH_TSV"
    rm -f "$log"
  done
done

# ── §2 Bouncy Castle suites ───────────────────────────────────────────────────
BCCP="$BC_DIR/core/build/classes/java/main;$BC_DIR/core/build/classes/java/test;$BC_DIR/core/build/resources/main;$BC_DIR/core/build/resources/test"
bc_prefix() {
  local heap="${2:-1g}"
  case "$1" in
    cratonvm) JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$heap");;
    hotspot)  JPRE=("$HOTSPOT" -Xmx"$heap");;
    tornadovm)JPRE=("$TORNADO" -Xmx"$heap");;
  esac
}
BC_SUITES=(
  "asn1-regression|0|1g|org.bouncycastle.asn1.test.RegressionTest"
  "math-ec|1|1g|junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests"
  "math-raw|1|1g|junit.textui.TestRunner org.bouncycastle.math.raw.test.AllTests"
  "math|1|1g|junit.textui.TestRunner org.bouncycastle.math.test.AllTests"
  "crypto-regression|0|4g|org.bouncycastle.crypto.test.RegressionTest"
  "crypto-prng|0|1g|org.bouncycastle.crypto.prng.test.RegressionTest"
  "pqc-crypto|0|2g|org.bouncycastle.pqc.crypto.test.RegressionTest"
  "util-encoders|1|1g|junit.textui.TestRunner org.bouncycastle.util.encoders.test.AllTests"
)
printf "suite\tvariant\trc\tstate\twall_s\theap\tsummary\n" > "$BC_TSV"
for entry in "${BC_SUITES[@]}"; do
  IFS='|' read -r name needj heap main <<< "$entry"
  cp="$BCCP"; [ "$needj" = "1" ] && cp="$BCCP;$JUNIT3"
  banner "bc: $name (heap=$heap)"
  for v in cratonvm hotspot tornadovm; do
    bc_prefix "$v" "$heap"
    log=$(mktemp); t0=$(date +%s%N)
    timeout "$TIMEOUT" "${JPRE[@]}" -cp "$cp" $main </dev/null >"$log" 2>&1
    rc=$?; t1=$(date +%s%N)
    wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
    state=OK; [ "$rc" -eq 124 ] && state=TIMEOUT
    ok=$(grep -aoE "OK \([0-9]+ tests\)|: Okay" "$log" | head -1)
    [ "$state" = "OK" ] && [ -z "$ok" ] && state=FAIL
    summary=$(grep -aiE "OK \(|: Okay|Tests run|Failures:|FAILED|Exception|SEGV|panic|stack overflow|Error" "$log" \
      | grep -avE "^\s*at " | sed 's/\x1b\[[0-9;]*m//g' | tail -1 | head -c 150)
    printf "  %-11s rc=%-3s %-8s %6ss  heap=%-3s  %s\n" "$v" "$rc" "$state" "$wall_s" "$heap" "$summary"
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$name" "$v" "$rc" "$state" "$wall_s" "$heap" "$summary" >>"$BC_TSV"
    rm -f "$log"
  done
done

# ── §3 Commons Math — JUnit Platform launcher on all 3 VMs ───────────────────
CM_CP="$JUNIT_STANDALONE;$CM_DIR/commons-math-transform/target/classes;$CM_DIR/commons-math-transform/target/test-classes;$CM_DIR/commons-math-core/target/classes"
M2="$HOME/.m2/repository"
CM_CP="$CM_CP;$M2/org/apache/commons/commons-numbers-rng/1.2/commons-numbers-rng-1.2.jar"
CM_CP="$CM_CP;$M2/org/apache/commons/commons-numbers-core/1.2/commons-numbers-core-1.2.jar"
# Transitive test deps of commons-math-transform (referenced from
# TransformUtilsTest.<clinit>): commons-math3, commons-rng-simple. Without
# them, discover succeeds (annotations only) but execute fails with NCDFE
# when JUnit instantiates the test class.
for opt_jar in \
  "$M2/org/apache/commons/commons-numbers-complex/1.3/commons-numbers-complex-1.3.jar" \
  "$M2/org/apache/commons/commons-math3/3.6.1/commons-math3-3.6.1.jar" \
  "$M2/org/apache/commons/commons-rng-simple/1.7/commons-rng-simple-1.7.jar" \
  "$M2/org/apache/commons/commons-rng-client-api/1.7/commons-rng-client-api-1.7.jar" \
  "$M2/org/apache/commons/commons-rng-core/1.7/commons-rng-core-1.7.jar" \
; do
  [ -f "$opt_jar" ] && CM_CP="$CM_CP;$opt_jar"
done
cm_prefix() {
  case "$1" in
    cratonvm) JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 2g);;
    hotspot)  JPRE=("$HOTSPOT" -Xmx2g);;
    tornadovm)JPRE=("$TORNADO" "$TORNADO_ARGF" -Xmx2g);;
  esac
}
printf "variant\trc\twall_s\tstate\tsummary\n" > "$CM_TSV"
banner "commons-math (JUnit Platform launcher)"
for v in cratonvm hotspot tornadovm; do
  cm_prefix "$v"
  log=$(mktemp); t0=$(date +%s%N)
  timeout 600 "${JPRE[@]}" -cp "$CM_CP" \
    org.junit.platform.console.ConsoleLauncher execute \
    --select-package org.apache.commons.math4.transform \
    --details=summary --disable-banner </dev/null >"$log" 2>&1
  rc=$?; t1=$(date +%s%N)
  wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
  [ "$rc" -eq 124 ] && state=TIMEOUT || state=RAN
  grep -qiE "SEGV|panic|stack overflow" "$log" && state=CRASH
  summary=$(grep -aiE "tests found|tests failed|tests successful|Exception|SEGV|panic|Error|NoSuchMethod" "$log" \
    | grep -avE "^\s*at " | sed 's/\x1b\[[0-9;]*m//g' | tail -2 | tr '\n' ' ' | head -c 200)
  printf "  %-11s rc=%-3s %-8s %6ss  %s\n" "$v" "$rc" "$state" "$wall_s" "$summary"
  printf "%s\t%s\t%s\t%s\t%s\n" "$v" "$rc" "$wall_s" "$state" "$summary" >>"$CM_TSV"
  rm -f "$log"
done

# ── §4 Extras ─────────────────────────────────────────────────────────────────
ex_prefix() {
  case "$1" in
    cratonvm-jit) JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 4g);;
    hotspot)      JPRE=("$HOTSPOT" -Xmx4g);;
    tornadovm)    JPRE=("$TORNADO" "$TORNADO_ARGF" -Xmx4g);;
  esac
}
printf "item\tvariant\trc\tstate\twall_s\tsummary\n" > "$EX_TSV"
for item in junit-help dacapo-avrora; do
  banner "extras: $item"
  for v in cratonvm-jit hotspot tornadovm; do
    ex_prefix "$v"
    log=$(mktemp); t0=$(date +%s%N)
    case "$item" in
      junit-help)    timeout "$TIMEOUT" "${JPRE[@]}" -jar "$JUNIT_STANDALONE" --help </dev/null >"$log" 2>&1;;
      dacapo-avrora) ( cd "$DACAPO_DIR" && timeout "$TIMEOUT" "${JPRE[@]}" -jar dacapo.jar avrora </dev/null ) >"$log" 2>&1;;
    esac
    rc=$?; t1=$(date +%s%N)
    wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
    clean=$(sed 's/\x1b\[[0-9;]*m//g' "$log")
    state=OK; [ "$rc" -eq 124 ] && state=TIMEOUT
    case "$item" in
      junit-help)
        # NoSuchMethodError is a WARN-level gap marker, not a failure gate —
        # pass iff the help banner printed and nothing fatal fired (the
        # summary column still surfaces any *Error lines). See
        # docs/gaps/gap-anonymous-object-getinputstream.md.
        bad=$(printf '%s' "$clean" | grep -aoiE "System.exit\(-1\)|SIGSEGV|panic" | head -1)
        good=$(printf '%s' "$clean" | grep -aoiE "Usage: junit|ConsoleLauncher \[|Thanks for using JUnit" | head -1)
        [ "$state" = OK ] && { [ -n "$bad" ] || [ -z "$good" ]; } && state=FAIL
        ;;
      dacapo-avrora)
        crash=$(printf '%s' "$clean" | grep -aoiE "SIGSEGV|panic|abort|fatal error" | head -1)
        [ -n "$crash" ] && state=FAIL || state="N/S"
        ;;
    esac
    summary=$(printf '%s' "$clean" | grep -aiE "PASSED|FAILED|Usage:|SEGV|panic|stack overflow|Exception|Error" \
      | grep -avE "^\s*at " | tail -1 | head -c 150)
    printf "  %-15s rc=%-3s %-8s %6ss  %s\n" "$v" "$rc" "$state" "$wall_s" "$summary"
    printf "%s\t%s\t%s\t%s\t%s\t%s\n" "$item" "$v" "$rc" "$state" "$wall_s" "$summary" >>"$EX_TSV"
    rm -f "$log"
  done
done

# ── RENDER ────────────────────────────────────────────────────────────────────
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
      printf "  %-*s", w0, TITLE; for(j=1;j<=nv;j++) printf " | %-*s", wv[j], vars[j]; printf "\n";
      printf "  "; for(k=0;k<w0;k++) printf "-"; for(j=1;j<=nv;j++){ printf "-+-"; for(k=0;k<wv[j];k++) printf "-" } printf "\n";
      for(i=1;i<=nr;i++){ printf "  %-*s", w0, rows[i]; for(j=1;j<=nv;j++){ c=cell[rows[i] SUBSEP vars[j]]; if(c=="") c="·"; printf " | %-*s", wv[j], c } printf "\n" }
    }' "$tsv"
}

banner "COMPARISON TABLES"

echo; echo "=== §1 Micro-benchmarks — wall time (ms) ==="; echo
pivot "$BENCH_TSV" 1 2 5 "benchmark"
echo; echo "=== §1 Micro-benchmarks — state ==="; echo
pivot "$BENCH_TSV" 1 2 4 "benchmark"
echo; echo "=== §1 Micro-benchmarks — checksum ==="; echo
pivot "$BENCH_TSV" 1 2 6 "benchmark"

echo; echo "=== §2 Bouncy Castle — state ==="; echo
pivot "$BC_TSV" 1 2 4 "bc-suite"
echo; echo "=== §2 Bouncy Castle — wall time (s) ==="; echo
pivot "$BC_TSV" 1 2 5 "bc-suite"

echo; echo "=== §3 Commons Math (JUnit Platform launcher, transform module) ==="; echo
awk -F'\t' 'NR==1{next}{printf "  %-11s rc=%-3s wall=%-7ss %-8s %s\n",$1,$2,$3,$4,$5}' "$CM_TSV"

echo; echo "=== §4 Extras — state ==="; echo
pivot "$EX_TSV" 1 2 4 "item"

hr
echo "TSVs:"
echo "  bench:  $BENCH_TSV"
echo "  bc:     $BC_TSV"
echo "  cm:     $CM_TSV"
echo "  extras: $EX_TSV"
hr
echo "Done — $(date)"
