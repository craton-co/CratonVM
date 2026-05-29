#!/usr/bin/env bash
# BC-core regression smoke for THIS machine (C:\Projects\cratonvm, Adoptium JDK).
# Adapted from run-bc-dacapo-suite.sh: corrected paths, BC-core only (the
# DaCapo jars are not staged here), JUnit-based suites auto-skip if no junit
# jar is found. The 4 SimpleTest "RegressionTest" main()s need no JUnit and are
# the primary smoke signal.
#
# Usage:  bash test-infra/run-bc-core-local.sh [timeout_secs]   (default 180)

set +e

ROOT="C:/Projects/cratonvm"
CRATONVM="$ROOT/target/release/cratonvm.exe"
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"

# Locate a JUnit 3.8.x jar (the JUnit-based AllTests suites need it). Search a
# few common spots; if absent, those suites are skipped (reported rc=SKIP).
JUNIT=""
for cand in \
  "$ROOT/apps/_test-suites/junit-3.8.2.jar" \
  "$ROOT/apps/_test-suites/bc-java/libs/junit-4.13.2.jar" \
  "$HOME"/.m2/repository/junit/junit/3.8.2/junit-3.8.2.jar \
  "$TEMP"/junit-3.8.2.jar \
  /c/Users/*/AppData/Local/Temp/junit-3.8.2.jar ; do
  if [ -f "$cand" ]; then JUNIT="$cand"; break; fi
done

TIMEOUT="${1:-180}"
RESULTS="$ROOT/test-infra/suite-results/bc-core-local-$(date +%Y%m%d-%H%M%S).tsv"
mkdir -p "$(dirname "$RESULTS")"
printf "suite\tvariant\trc\twall_s\tsummary\n" > "$RESULTS"

if [ ! -x "$CRATONVM" ] && [ ! -f "$CRATONVM" ]; then
  echo "ERROR: $CRATONVM not found — run 'cargo build --release' first." >&2
  exit 2
fi
echo "cratonvm: $CRATONVM"
echo "JDK:      $JDK"
echo "JUnit:    ${JUNIT:-<none found — JUnit suites will be skipped>}"
echo

run() {
  local suite="$1" variant="$2" cmd="$3"
  local errfile; errfile=$(mktemp)
  printf "[%s] starting...\n" "$suite" >&2
  local t0; t0=$(date +%s%N)
  timeout "$TIMEOUT" bash -c "$cmd" > /dev/null 2> "$errfile"
  local rc=$?
  local t1; t1=$(date +%s%N)
  local wall_s; wall_s=$(awk -v ms="$(( (t1 - t0) / 1000000 ))" 'BEGIN{printf "%.2f", ms/1000}')
  local summary; summary=$(tail -3 "$errfile" 2>/dev/null | tr '\n' ' ' | sed 's/\x1b\[[0-9;]*m//g' | head -c 200)
  printf "%-30s | rc=%-4s | %8ss | %s\n" "$suite" "$rc" "$wall_s" "$summary"
  printf "%s\t%s\t%s\t%s\t%s\n" "$suite" "$variant" "$rc" "$wall_s" "$summary" >> "$RESULTS"
  rm -f "$errfile"
}

run_junit() {
  local suite="$1" alltests="$2"
  if [ -z "$JUNIT" ]; then
    printf "%-30s | rc=SKIP | (no junit jar)\n" "$suite"
    printf "%s\t%s\tSKIP\t0\tno junit jar\n" "$suite" "cratonvm" >> "$RESULTS"
    return
  fi
  run "$suite" "cratonvm" \
    "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP;$JUNIT\" junit.textui.TestRunner $alltests"
}

BC_DIR="$ROOT/apps/_test-suites/bc-java"
BC_CP="$BC_DIR/core/build/classes/java/main;$BC_DIR/core/build/classes/java/test;$BC_DIR/core/build/resources/main;$BC_DIR/core/build/resources/test"

# --- SimpleTest main()s (no JUnit needed) — the primary smoke signal ---
run "bc-asn1-regression"        "cratonvm" "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP\" org.bouncycastle.asn1.test.RegressionTest"
run "bc-crypto-regression"      "cratonvm" "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP\" org.bouncycastle.crypto.test.RegressionTest"
run "bc-crypto-prng-regression" "cratonvm" "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP\" org.bouncycastle.crypto.prng.test.RegressionTest"
run "bc-pqc-crypto-regression"  "cratonvm" "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP\" org.bouncycastle.pqc.crypto.test.RegressionTest"

# --- JUnit AllTests suites (auto-skip if no junit jar) ---
run_junit "bc-math-ec"       org.bouncycastle.math.ec.test.AllTests
run_junit "bc-math-raw"      org.bouncycastle.math.raw.test.AllTests
run_junit "bc-math"          org.bouncycastle.math.test.AllTests
run_junit "bc-util-encoders" org.bouncycastle.util.encoders.test.AllTests
run_junit "bc-util-utiltest" org.bouncycastle.util.utiltest.AllTests

echo
echo "Results: $RESULTS"
