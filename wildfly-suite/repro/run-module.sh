#!/usr/bin/env bash
# Batched module sweep for regression verification. Runs every *TestCase/*Test
# class in one module through KRun, BATCH per cratonvm JVM, with controllable
# JIT knobs. Scans the captured output for VM-specific bug signatures
# (the JIT exception-handler family) vs the benign Arquillian no-container
# failures. NOT crash-recovering — a batch JVM crash just loses that batch
# (reported as a gap); good enough for a clean/dirty signal.
#
# Usage: MOD=integration/clustering THRESH=2 TIERUP=1 BATCH=15 ./run-module.sh
set -u
ROOT="/c/craton/cratonvm/apps/wildfly/testsuite"
HARNESS="/c/craton/CratonVM-wildfly/wildfly-suite"
VM="${VM:-/c/craton/cratonvm/target/release/cratonvm.exe}"
JDK25="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
MOD="${MOD:-integration/clustering}"
THRESH="${THRESH:-2}"; TIERUP="${TIERUP:-1}"; BATCH="${BATCH:-15}"; BATCH_TO="${BATCH_TO:-1200}"
OUT="${OUT:-/tmp/runmod_$(echo "$MOD" | tr '/' '_').out}"
MODDIR="$ROOT/$MOD"; TC="$MODDIR/target/test-classes"

HARNESS_W=$(cygpath -m "$HARNESS"); HCP="$HARNESS_W"
while IFS= read -r l || [ -n "$l" ]; do l="${l%$'\r'}"; [ -n "$l" ] && HCP="$HCP;$l"; done < "$HARNESS/harness-cp.txt"
MCP="$HCP;$(cygpath -m "$TC");$(cygpath -m "$MODDIR/target/classes")"
if [ -f "$MODDIR/target/cratonvm-testcp.txt" ]; then
  DEPS=$(tr '\\' '/' < "$MODDIR/target/cratonvm-testcp.txt" | tr -d '\r' | sed 's:/$::')
  [ -n "$DEPS" ] && MCP="$MCP;$DEPS"
fi
{ echo "-cp"; echo "$MCP"; } > /tmp/.af_runmod.txt
AF=$(cygpath -m /tmp/.af_runmod.txt)

mapfile -t ALL < <(cd "$TC" && find . \( -name '*TestCase.class' -o -name '*Test.class' \) ! -name '*\$*' \
    | sed 's|^\./||; s|\.class$||; s|/|.|g' | sort)
echo "### MOD=$MOD classes=${#ALL[@]} BATCH=$BATCH THRESH=$THRESH TIERUP=$TIERUP" | tee "$OUT"

export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
export CRATONVM_JIT_THRESHOLD="$THRESH"
export CRATONVM_JIT_VIRTUAL_TIERUP="$TIERUP"

total=${#ALL[@]}
for ((b=0; b<total; b+=BATCH)); do
  batch=("${ALL[@]:b:BATCH}")
  raw=$(timeout "$BATCH_TO" "$VM" --java-home "$JDK25" "@$AF" KRun "${batch[@]}" 2>&1); rc=$?
  printf '%s\n' "$raw" >> "$OUT"
  got=$(printf '%s\n' "$raw" | grep -c '^RESULT ')
  echo "### batch $((b+${#batch[@]}))/$total rc=$rc results=$got" | tee -a "$OUT"
done
echo "### SWEEP COMPLETE" | tee -a "$OUT"
echo "=== VM-bug signature scan ===" | tee -a "$OUT"
grep -cE "Cannot read field 'throwable'|annotationType must not be null|CompactValue|exceeds 47-bit|AbstractMethodError|cannot be cast to org/junit|InternalError: JIT|Cannot invoke .* on null" "$OUT" | xargs echo "VM-bug lines:" | tee -a "$OUT"
