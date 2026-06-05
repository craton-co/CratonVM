#!/usr/bin/env bash
# Catch the bc-math-ec 0x4 WRITER. Two detectors in one run:
#  - ECWATCH + ECWATCH_NATIVE: per-native detect on the (broadened) watch-list
#    (X9/Curve + HexFormat + Level). PRECISE: names the exact native after which
#    a watched cell flipped to 0x4, with the Java stack.
#  - YOUNGSCAN: whole-young one-shot scan (catches ANY victim class). STRIDE via
#    CRATONVM_YOUNGSCAN_STRIDE (passed as $2, default 1).
set -u
BIN=/c/craton/CratonVM-ecgc/target/release/cratonvm.exe
RUN=/c/craton/CratonVM-ecgc/target/release/cratonvm_seedhunt.exe
cp -f "$BIN" "$RUN"
cd /c/craton/CratonVM/apps/_test-suites/bc-java
CP="core/build/classes/java/main;core/build/classes/java/test;core/build/resources/main;core/build/resources/test;$TEMP/junit-3.8.2.jar"
LOG="${1:-/c/craton/CratonVM-ecgc/ecprobe/catch.log}"
STRIDE="${2:-1}"
echo "=== catch run $(date) stride=$STRIDE ===" > "$LOG"
CRATONVM_DISABLE_JIT=1 CRATONVM_DBG_ECWATCH=1 CRATONVM_DBG_ECWATCH_NATIVE=1 \
  CRATONVM_DBG_YOUNGSCAN=1 CRATONVM_YOUNGSCAN_STRIDE="$STRIDE" \
  "$RUN" --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 -Xmx256m -cp "$CP" \
  junit.textui.TestRunner org.bouncycastle.math.ec.test.FixedPointTest >> "$LOG" 2>&1
echo "rc=$?" >> "$LOG"
echo DONE
