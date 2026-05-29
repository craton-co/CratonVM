#!/usr/bin/env bash
# Run BC core test suites + every DaCapo benchmark with a per-test timeout,
# capture rc + wall-time + last 5 stderr lines into a TSV.
#
# Usage:  bash test-infra/run-bc-dacapo-suite.sh [timeout_secs]   (default 180)

set +e

ROOT="C:/craton/CratonVM"
CRATONVM="$ROOT/target/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
JUNIT="${JUNIT:-$TEMP/junit-3.8.2.jar}"

TIMEOUT="${1:-180}"
RESULTS="$ROOT/test-infra/suite-results/bc-dacapo-$(date +%Y%m%d-%H%M%S).tsv"
mkdir -p "$(dirname "$RESULTS")"

printf "suite\tvariant\trc\twall_s\tsummary\n" > "$RESULTS"

run() {
  local suite="$1"
  local variant="$2"
  local cmd="$3"
  local errfile=$(mktemp)
  printf "[%s/%s] starting...\n" "$suite" "$variant" >&2
  local t0=$(date +%s%N)
  timeout "$TIMEOUT" bash -c "$cmd" > /dev/null 2> "$errfile"
  local rc=$?
  local t1=$(date +%s%N)
  local wall_ms=$(( (t1 - t0) / 1000000 ))
  local wall_s=$(awk -v ms="$wall_ms" 'BEGIN{printf "%.2f", ms/1000}')
  local summary=$(tail -3 "$errfile" 2>/dev/null | tr '\n' ' | ' | sed 's/\x1b\[[0-9;]*m//g' | head -c 200)
  printf "%-30s | %-10s | rc=%-3s | %7ss | %s\n" "$suite" "$variant" "$rc" "$wall_s" "$summary"
  printf "%s\t%s\t%s\t%s\t%s\n" "$suite" "$variant" "$rc" "$wall_s" "$summary" >> "$RESULTS"
  rm -f "$errfile"
}

# ============ BC core tests ============
BC_DIR="$ROOT/apps/_test-suites/bc-java"
BC_CP="$BC_DIR/core/build/classes/java/main;$BC_DIR/core/build/classes/java/test;$BC_DIR/core/build/resources/main;$BC_DIR/core/build/resources/test"

# bc-asn1: SimpleTest-style — run main directly
run "bc-asn1-regression" "cratonvm" \
  "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP\" org.bouncycastle.asn1.test.RegressionTest"

# bc-math-ec: JUnit AllTests
run "bc-math-ec"          "cratonvm" \
  "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP;$JUNIT\" junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests"

# bc-math-raw: JUnit AllTests
run "bc-math-raw"         "cratonvm" \
  "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP;$JUNIT\" junit.textui.TestRunner org.bouncycastle.math.raw.test.AllTests"

# bc-math: JUnit AllTests (top-level math)
run "bc-math"             "cratonvm" \
  "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP;$JUNIT\" junit.textui.TestRunner org.bouncycastle.math.test.AllTests"

# bc-crypto: SimpleTest RegressionTest main
run "bc-crypto-regression" "cratonvm" \
  "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP\" org.bouncycastle.crypto.test.RegressionTest"

# bc-crypto-prng: RegressionTest main
run "bc-crypto-prng-regression" "cratonvm" \
  "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP\" org.bouncycastle.crypto.prng.test.RegressionTest"

# bc-pqc-crypto: RegressionTest main
run "bc-pqc-crypto-regression" "cratonvm" \
  "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP\" org.bouncycastle.pqc.crypto.test.RegressionTest"

# bc-util-encoders: JUnit AllTests
run "bc-util-encoders"    "cratonvm" \
  "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP;$JUNIT\" junit.textui.TestRunner org.bouncycastle.util.encoders.test.AllTests"

# bc-util-utiltest: JUnit AllTests
run "bc-util-utiltest"    "cratonvm" \
  "$CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -cp \"$BC_CP;$JUNIT\" junit.textui.TestRunner org.bouncycastle.util.utiltest.AllTests"

# ============ DaCapo ============
DACAPO_DIR="$ROOT/apps/dacapo"
DACAPO_JAR="$DACAPO_DIR/dacapo.jar"
# Quick benchmarks first, then heavier ones; skip Tomcat / Eclipse (known-broken or selector-issue)
for bench in avrora luindex lusearch lusearch-fix fop sunflow xalan pmd jython h2 batik tradebeans tradesoap eclipse tomcat; do
  run "dacapo-$bench" "cratonvm" \
    "cd \"$DACAPO_DIR\" && $CRATONVM --java-home \"$JDK\" --stack-dump-on-timeout 0 -Xmx1g -jar dacapo.jar $bench"
done

echo
echo "Results: $RESULTS"
