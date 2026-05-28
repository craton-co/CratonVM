#!/usr/bin/env bash
# Systematic bisection of bc-math-ec JIT miscompile.
# Strategy: enable JIT for JDK + ONE BC sub-package at a time. If SEGV
# happens, the culprit is in that sub-package.

set -u
JUNIT='C:\Users\Victor\AppData\Local\Temp\junit-3.8.2.jar'
BC_CP='core/build/classes/java/main;core/build/classes/java/test;core/build/resources/main;core/build/resources/test'
CMD="C:/craton/CratonVM/target/release/cratonvm.exe --java-home \"C:/Program Files/Java/jdk-25\" --stack-dump-on-timeout 0 -Xmx1g -cp \"${BC_CP};${JUNIT}\" junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests"

cd C:/craton/CratonVM/apps/_test-suites/bc-java

run() {
  local label="$1"
  local only="$2"
  CRATONVM_JIT_BISECT_ONLY="$only" timeout 60 bash -c "$CMD" > /tmp/bisect.log 2>&1
  local rc=$?
  local segv=$(grep -c "inconsistent\|Segmentation" /tmp/bisect.log)
  local tests=$(grep -c "Tests run" /tmp/bisect.log)
  echo "$label: rc=$rc segv=$segv tests_completed=$tests only='$only'"
}

# Baseline: full JIT — should SEGV
echo "=== Baseline (full JIT) ==="
timeout 60 bash -c "$CMD" > /tmp/bisect.log 2>&1
echo "Full JIT: rc=$? segv=$(grep -c "inconsistent\|Segmentation" /tmp/bisect.log)"

# JDK-only — should be slow but no SEGV
echo
echo "=== JDK only ==="
run "JDK" "java/lang/,java/util/,sun/,jdk/"

# JDK + each BC sub-package
echo
echo "=== JDK + one BC sub-package ==="
for pkg in 'org/bouncycastle/math/raw/' 'org/bouncycastle/math/ec/' \
           'org/bouncycastle/util/' 'org/bouncycastle/asn1/' \
           'org/bouncycastle/crypto/' 'org/bouncycastle/jce/' \
           'org/bouncycastle/x509/'; do
  run "$pkg" "java/lang/,java/util/,sun/,jdk/,$pkg"
done
