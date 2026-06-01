#!/usr/bin/env bash
# 3-suite x 4-way regression + timing harness.
#
#   Suites:   1. Apache Commons Math (Maven Surefire reactor)
#             2. BouncyCastle core   (green functional suites, direct JUnit)
#             3. DaCapo benchmarks   (-jar dacapo.jar <bench>)
#   Variants: cratonvm-cpu | cratonvm-gpu | hotspot | tornadovm
#
# Every (suite-task, variant) records wall_s + pass/fail into a TSV so speed
# regressions are visible. Per the retirement rule: anything that PASSES on
# cratonvm (cpu) is also measured on hotspot + tornadovm (+ cratonvm-gpu).
#
# Usage:
#   bash regression-3suite-4way.sh [--variants "cratonvm-cpu hotspot ..."]
#                                  [--bc] [--dacapo] [--commons-math]
#                                  [--timeout SECS] [--gpu-bin PATH]
set +e

ROOT="C:/craton/CratonVM"
CV_CPU="${CV_CPU:-$ROOT/target/release/cratonvm.exe}"
CV_GPU="${CV_GPU:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="C:/Program Files/Java/jdk-25"
HOTSPOT="$JDK/bin/java.exe"
TORNADO="C:/craton/tornadovm/jdk-25.0.3/bin/java.exe"
TORNADO_ARGF="@C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/tornado-argfile"
JUNIT="${JUNIT:-$TEMP/junit-3.8.2.jar}"
BC_DIR="$ROOT/apps/_test-suites/bc-java"
BC_CP="$BC_DIR/core/build/classes/java/main;$BC_DIR/core/build/classes/java/test;$BC_DIR/core/build/resources/main;$BC_DIR/core/build/resources/test"
DACAPO_DIR="$ROOT/apps/dacapo"
TS=$(date +%Y%m%d-%H%M%S)
OUT="$ROOT/test-infra/suite-results/3suite-4way-$TS.tsv"
mkdir -p "$(dirname "$OUT")"
printf "suite\ttask\tvariant\trc\tpass\twall_s\tnote\n" > "$OUT"

VARIANTS="cratonvm-cpu cratonvm-gpu hotspot tornadovm"
RUN_BC=0; RUN_DC=0; RUN_CM=0; TIMEOUT=400
while [ $# -gt 0 ]; do case "$1" in
  --variants) VARIANTS="$2"; shift 2;;
  --bc) RUN_BC=1; shift;;
  --dacapo) RUN_DC=1; shift;;
  --commons-math) RUN_CM=1; shift;;
  --timeout) TIMEOUT="$2"; shift 2;;
  --gpu-bin) CV_GPU="$2"; shift 2;;
  *) echo "unknown arg $1" >&2; exit 2;;
esac; done
[ "$RUN_BC$RUN_DC$RUN_CM" = "000" ] && { RUN_BC=1; RUN_DC=1; RUN_CM=1; }

# jvm_prefix VARIANT -> populates the global `JPRE` array with the launcher
# command + any VM-specific flags (cratonvm needs --java-home; others don't).
JPRE=()
jvm_prefix() {
  case "$1" in
    cratonvm-cpu) JPRE=("$CV_CPU" --java-home "$JDK" --stack-dump-on-timeout 0);;
    cratonvm-gpu) JPRE=("$CV_GPU" --gpu --java-home "$JDK" --stack-dump-on-timeout 0);;
    hotspot)      JPRE=("$HOTSPOT");;
    tornadovm)    JPRE=("$TORNADO" "$TORNADO_ARGF");;
  esac
}
have_variant() { case " $VARIANTS " in *" $1 "*) return 0;; *) return 1;; esac; }

# pass-detect: scans combined output for a per-suite success signal
PASS_RE='OK \(|All tests successful|PASSED|BUILD SUCCESS'
FAIL_RE='FAILURES!!!|BUILD FAILURE|Exception in thread|FAILED'

# run_jvm VARIANT LOGFILE -- ARGS...   (ARGS use | as separator inside prefix)
# Builds the JVM-agnostic invocation (passed after `--`) onto the variant prefix.
run_one() { # suite task variant invocation...
  local suite="$1" task="$2" variant="$3"; shift 3
  have_variant "$variant" || return 0
  jvm_prefix "$variant"
  local -a cmd=("${JPRE[@]}" "$@")
  local log; log=$(mktemp)
  local t0 t1 rc
  t0=$(date +%s)
  timeout "$TIMEOUT" "${cmd[@]}" > "$log" 2>&1
  rc=$?; t1=$(date +%s)
  local pass=FAIL
  if [ "$rc" -eq 0 ] && grep -qE "$PASS_RE" "$log" && ! grep -qE "$FAIL_RE" "$log"; then pass=PASS; fi
  [ "$rc" -eq 124 ] && pass=TIMEOUT
  local note; note=$(grep -oE "$PASS_RE|$FAIL_RE|Tests run[^,]*" "$log" | head -1)
  printf "%-13s %-22s %-13s rc=%-3s %-7s %4ss  %s\n" "$suite" "$task" "$variant" "$rc" "$pass" "$((t1-t0))" "$note"
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$suite" "$task" "$variant" "$rc" "$pass" "$((t1-t0))" "$note" >> "$OUT"
  rm -f "$log"
}

# ---- BC green suites (direct JUnit/main; JVM-agnostic args) ----
declare -a BC_JUNIT=(
  "math:org.bouncycastle.math.test.AllTests"
  "math-raw:org.bouncycastle.math.raw.test.AllTests"
  "util-encoders:org.bouncycastle.util.encoders.test.AllTests"
  "util-utiltest:org.bouncycastle.util.utiltest.AllTests"
)
declare -a BC_MAIN=(
  "asn1:org.bouncycastle.asn1.test.RegressionTest"
  "crypto-prng:org.bouncycastle.crypto.prng.test.RegressionTest"
)
run_bc_for() { # variant
  local v="$1"
  local e cls
  for e in "${BC_JUNIT[@]}"; do cls="${e#*:}"
    run_one bc "${e%%:*}" "$v" -Xmx1g -cp "$BC_CP;$JUNIT" junit.textui.TestRunner "$cls"
  done
  for e in "${BC_MAIN[@]}"; do cls="${e#*:}"
    run_one bc "${e%%:*}" "$v" -Xmx1g -cp "$BC_CP" "$cls"
  done
}

# ---- DaCapo ----
run_dc_for() { # variant
  local v="$1" b
  for b in avrora luindex sunflow fop; do
    ( cd "$DACAPO_DIR" && true )  # dacapo writes scratch in cwd; run from there
    run_one dacapo "$b" "$v" -Xmx1g -jar "$DACAPO_DIR/dacapo.jar" "$b"
  done
}

# Run from a CLEAN scratch dir (no top-level *.jar): TornadoVM's argfile puts
# `.` on --module-path and tries to module-ize any jar in cwd (fails on
# dacapo.jar). DaCapo is launched via an ABSOLUTE jar path and writes its
# scratch/ into this cwd, so a clean dir works for every variant.
SCRATCH="$ROOT/test-infra/suite-results/_run-scratch-$TS"
mkdir -p "$SCRATCH"; cd "$SCRATCH"

for v in $VARIANTS; do
  echo "########## variant: $v ##########"
  [ "$RUN_BC" = 1 ] && run_bc_for "$v"
  [ "$RUN_DC" = 1 ] && run_dc_for "$v"
done

echo
echo "Results: $OUT"
