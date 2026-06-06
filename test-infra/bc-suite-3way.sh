#!/usr/bin/env bash
# Bouncy Castle core test suites across 3 VMs (cratonvm / hotspot / tornadovm).
# SimpleTest-style classes print "<Name>: Okay"; JUnit-textui AllTests print
# "OK (n tests)" or "Tests run: ... Failures:". Genuine per-VM execution: each
# variant just swaps the `java` binary in front of an identical classpath+main.
set +e

# ---------------------------------------------------------------------------
# HARNESS ARTIFACT GUARD: under MSYS/Git-bash, command-line arguments that look
# like POSIX path lists get auto-converted on the way to a native .exe. A
# ';'-separated Windows -cp (e.g. ".../main;.../test;.../junit.jar") can be
# mangled so a classpath entry — typically the junit jar — is dropped, which
# makes `junit.textui.TestRunner` "not found" and the suite spuriously FAIL
# (it fails identically on HotSpot, proving it is NOT a VM bug). Disabling arg
# path-conversion passes the classpath through verbatim to every VM.
export MSYS2_ARG_CONV_EXCL='*'   # MSYS2 / newer Git-bash
export MSYS_NO_PATHCONV=1        # Git-for-Windows bash

ROOT="C:/craton/CratonVM"
CV="$ROOT/target/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
HOTSPOT="$JDK/bin/java.exe"
TORNADO="C:/craton/tornadovm/jdk-25.0.3/bin/java.exe"
JUNIT="${JUNIT:-$TEMP/junit-3.8.2.jar}"
TIMEOUT="${TIMEOUT:-240}"
TS=$(date +%Y%m%d-%H%M%S)
OUT="$ROOT/test-infra/suite-results/bc-3way-$TS.tsv"
mkdir -p "$(dirname "$OUT")"
printf "suite\tvariant\trc\tstate\twall_s\theap\tsummary\n" > "$OUT"

BC_DIR="$ROOT/apps/_test-suites/bc-java"
BC_CP="$BC_DIR/core/build/classes/java/main;$BC_DIR/core/build/classes/java/test;$BC_DIR/core/build/resources/main;$BC_DIR/core/build/resources/test"

VARIANTS="${VARIANTS:-cratonvm hotspot tornadovm}"

# suite-name | needs-junit(0/1) | heap | mainclass-and-args
# Heap is per-suite so a memory-hungry suite gets a fair allotment on EVERY VM
# (crypto-regression OOMs at 1g on HotSpot too, so 1g is not a fair test).
SUITES=(
  "asn1-regression|0|1g|org.bouncycastle.asn1.test.RegressionTest"
  "math-ec|1|1g|junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests"
  "math-raw|1|1g|junit.textui.TestRunner org.bouncycastle.math.raw.test.AllTests"
  "math|1|1g|junit.textui.TestRunner org.bouncycastle.math.test.AllTests"
  "crypto-regression|0|4g|org.bouncycastle.crypto.test.RegressionTest"
  "crypto-prng-regression|0|1g|org.bouncycastle.crypto.prng.test.RegressionTest"
  "pqc-crypto-regression|0|2g|org.bouncycastle.pqc.crypto.test.RegressionTest"
  "util-encoders|1|1g|junit.textui.TestRunner org.bouncycastle.util.encoders.test.AllTests"
  "util-utiltest|1|1g|junit.textui.TestRunner org.bouncycastle.util.test.AllTests"
)

prefix() {
  local heap="${2:-1g}"
  case "$1" in
    cratonvm)  JPRE=("$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$heap");;
    hotspot)   JPRE=("$HOTSPOT" -Xmx"$heap");;
    tornadovm) JPRE=("$TORNADO" -Xmx"$heap");;
  esac
}

for entry in "${SUITES[@]}"; do
  IFS='|' read -r name needj heap main <<< "$entry"
  cp="$BC_CP"; [ "$needj" = "1" ] && cp="$BC_CP;$JUNIT"
  echo "================= bc: $name (heap=$heap) ================="
  for v in $VARIANTS; do
    prefix "$v" "$heap"
    log=$(mktemp)
    t0=$(date +%s%N)
    timeout "$TIMEOUT" "${JPRE[@]}" -cp "$cp" $main < /dev/null > "$log" 2>&1
    rc=$?
    t1=$(date +%s%N)
    wall_s=$(awk -v ms=$(( (t1-t0)/1000000 )) 'BEGIN{printf "%.1f", ms/1000}')
    state=OK
    [ "$rc" -eq 124 ] && state=TIMEOUT
    # success markers: "OK (" for junit textui, ": Okay" for SimpleTest, "Tests run" w/ 0 failures
    ok=$(grep -aoE "OK \([0-9]+ tests\)|: Okay" "$log" | head -1)
    fails=$(grep -aoE "Failures: [0-9]+|FAILURES" "$log" | head -1)
    summary=$(grep -aiE "OK \(|: Okay|Tests run|Failures:|FAILED|Exception|SEGV|panic|stack overflow|Error" "$log" | grep -avE "^\s*at " | sed 's/\x1b\[[0-9;]*m//g' | tail -2 | tr '\n' ' ' | head -c 180)
    if [ "$state" = "OK" ] && [ -z "$ok" ]; then state=FAIL; fi
    printf "  %-11s rc=%-3s %-8s %6ss  heap=%-3s %s\n" "$v" "$rc" "$state" "$wall_s" "$heap" "$summary"
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$name" "$v" "$rc" "$state" "$wall_s" "$heap" "$summary" >> "$OUT"
    rm -f "$log"
  done
done
echo
echo "Results: $OUT"
