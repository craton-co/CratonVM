#!/usr/bin/env bash
# Repro for the deeper BC-JIT SPHINCS-256 signing failure (item A-remaining).
# Runs Sphincs256Test (SimpleTest main) with the org/bouncycastle JIT ban lifted,
# capturing BOTH stdout+stderr to a file so nothing is lost on a crash/rc=1.
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

WT="C:/craton/CratonVM-bcjit"
MAIN="C:/craton/CratonVM"
CV="$WT/target/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
BC_DIR="$MAIN/apps/_test-suites/bc-java"
BC_CP="$BC_DIR/core/build/classes/java/main;$BC_DIR/core/build/classes/java/test;$BC_DIR/core/build/resources/main;$BC_DIR/core/build/resources/test"

OUT="${OUT:-$WT/sphincs_run.log}"
HEAP="${HEAP:-2g}"
TIMEOUT="${TIMEOUT:-180}"

echo "binary: $CV  (mtime $(date -r "$CV" '+%H:%M:%S' 2>/dev/null))"
echo "out:    $OUT  heap=$HEAP timeout=${TIMEOUT}s"
echo

timeout "$TIMEOUT" env \
  CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/ \
  ${DBG:+$DBG} \
  "$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$HEAP" \
  -cp "$BC_CP" org.bouncycastle.pqc.crypto.test.Sphincs256Test \
  < /dev/null > "$OUT" 2>&1
rc=$?
echo "rc=$rc"
echo "=== tail of $OUT ==="
tail -40 "$OUT"
