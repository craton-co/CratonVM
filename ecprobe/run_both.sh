#!/usr/bin/env bash
# Cross-check: run FixedPointTest with BOTH ec_watch (GC-EXIT detect) and
# seedhunt (phase-bisect) on. If [ecwatch-GCEXIT] fires in the SAME GC that
# [seedhunt] shows a count jump -> real corruption, phase localized. If
# ec_watch fires but seedhunt stays 0 -> ec_watch GC-EXIT is a remap
# false-positive (fact 4 misleading).
set -u
BIN=/c/craton/CratonVM-ecgc/target/release/cratonvm.exe
RUN=/c/craton/CratonVM-ecgc/target/release/cratonvm_seedhunt.exe
cp -f "$BIN" "$RUN"
cd /c/craton/CratonVM/apps/_test-suites/bc-java
CP="core/build/classes/java/main;core/build/classes/java/test;core/build/resources/main;core/build/resources/test;$TEMP/junit-3.8.2.jar"
LOG="${1:-/c/craton/CratonVM-ecgc/ecprobe/both.log}"
echo "=== both run $(date) ===" > "$LOG"
CRATONVM_DISABLE_JIT=1 CRATONVM_DBG_SEEDHUNT=1 CRATONVM_DBG_ECWATCH=1 \
  "$RUN" --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 -Xmx256m -cp "$CP" \
  junit.textui.TestRunner org.bouncycastle.math.ec.test.FixedPointTest >> "$LOG" 2>&1
echo "rc=$?" | tee -a "$LOG"
