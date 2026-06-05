#!/usr/bin/env bash
# bc-math-ec 0x4 seed-phase bisect repro.
# Runs FixedPointTest (JIT off -> moving Cheney) with CRATONVM_DBG_SEEDHUNT on.
# Output: which phase (post-cheney vs post-major) first introduces a 0x4 slot,
# plus the [gcfwd] log if the major-GC update_refs path writes the small fwd.
set -u
BIN=/c/craton/CratonVM-ecgc/target/release/cratonvm.exe
RUN=/c/craton/CratonVM-ecgc/target/release/cratonvm_seedhunt.exe
cp -f "$BIN" "$RUN"
cd /c/craton/CratonVM/apps/_test-suites/bc-java
CP="core/build/classes/java/main;core/build/classes/java/test;core/build/resources/main;core/build/resources/test;$TEMP/junit-3.8.2.jar"
LOG="${1:-/c/craton/CratonVM-ecgc/ecprobe/seedhunt.log}"
echo "=== seedhunt run $(date) ===" > "$LOG"
CRATONVM_DISABLE_JIT=1 CRATONVM_DBG_SEEDHUNT=1 \
  "$RUN" --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 -Xmx256m -cp "$CP" \
  junit.textui.TestRunner org.bouncycastle.math.ec.test.FixedPointTest >> "$LOG" 2>&1
echo "rc=$?" | tee -a "$LOG"
