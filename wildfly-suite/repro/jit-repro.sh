#!/usr/bin/env bash
# Focused JIT-bug reproduction harness for the WildFly suite.
# Runs ONE batch of test classes from a single module through KRun in a single
# cratonvm JVM, with full control over the JIT knobs and KRUN_STACK traces.
#
# Usage:
#   MOD=integration/basic THRESH=2 TIERUP=1 ./jit-repro.sh Class1 Class2 ...
#
# Env:
#   MOD      module path under apps/wildfly/testsuite (default integration/basic)
#   THRESH   CRATONVM_JIT_THRESHOLD (default 2 — compile almost immediately)
#   TIERUP   CRATONVM_JIT_VIRTUAL_TIERUP (default 1; set 0 to disable layer B)
#   TO       external timeout seconds (default 300)
#   STACK    KRUN_STACK (default 1 — print full stack traces)
#   EXTRA    extra CRATONVM_* env assignments, space separated (e.g. "CRATONVM_DBG_JIT_DISPATCH=1")
set -u
ROOT="/c/craton/cratonvm/apps/wildfly/testsuite"
HARNESS="/c/craton/CratonVM-wildfly/wildfly-suite"  # has compiled KRun.class
VM="${VM:-/c/craton/cratonvm/target/release/cratonvm.exe}"
JDK25="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
MOD="${MOD:-integration/basic}"
THRESH="${THRESH:-2}"; TIERUP="${TIERUP:-1}"; TO="${TO:-300}"; STACK="${STACK:-1}"
MODDIR="$ROOT/$MOD"
TC="$MODDIR/target/test-classes"

HARNESS_W=$(cygpath -m "$HARNESS")
HCP="$HARNESS_W"
while IFS= read -r l || [ -n "$l" ]; do l="${l%$'\r'}"; [ -n "$l" ] && HCP="$HCP;$l"; done < "$HARNESS/harness-cp.txt"
MCP="$HCP;$(cygpath -m "$TC");$(cygpath -m "$MODDIR/target/classes")"
if [ -f "$MODDIR/target/cratonvm-testcp.txt" ]; then
  DEPS=$(tr '\\' '/' < "$MODDIR/target/cratonvm-testcp.txt" | tr -d '\r')
  [ -n "$DEPS" ] && MCP="$MCP;$DEPS"
fi

AF="/tmp/.af_repro.txt"; { echo "-cp"; echo "$MCP"; } > "$AF"

export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
export CRATONVM_JIT_THRESHOLD="$THRESH"
export CRATONVM_JIT_VIRTUAL_TIERUP="$TIERUP"
[ "$STACK" = "1" ] && export KRUN_STACK=1
for kv in ${EXTRA:-}; do export "$kv"; done

echo "### MOD=$MOD THRESH=$THRESH TIERUP=$TIERUP TO=$TO classes=$#"
timeout "$TO" "$VM" --java-home "$JDK25" "@$(cygpath -m "$AF")" KRun "$@"
echo "### rc=$?"
