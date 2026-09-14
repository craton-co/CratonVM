#!/usr/bin/env bash
# SB-CRASH-04 register-invisibility A/B harness (runs against MAIN checkout buildSrc).
set -u
CV="C:/craton/CratonVM-sbreg/cratonvm-sbreg.exe"
JDK="C:/Program Files/Java/jdk-25"
cd C:/craton/CratonVM/apps/spring-boot/buildSrc || exit 1
CP="runner;$(cat test-classpath.txt)"
N="${1:-8}"; WHICH="${2:-code}"; ITERS="${3:-20000}"

run_cfg() {
  local label="$1" envset="$2"
  local ok=0 wrong=0 crash=0 corrupt=0 totih=0
  for i in $(seq 1 "$N"); do
    timeout 120 env $envset CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_GC_STRESS=524288 \
      "$CV" --java-home "$JDK" -cp "$CP" MinRegexProbe "$WHICH" "$ITERS" > /tmp/rs.log 2>&1
    local ih; ih=$(grep -c 'inconsistent header' /tmp/rs.log); totih=$((totih+ih))
    [ "$ih" -gt 0 ] && corrupt=$((corrupt+1))
    if grep -q 'a `X` b' /tmp/rs.log; then ok=$((ok+1))
    elif grep -q 'DONE 20000' /tmp/rs.log; then wrong=$((wrong+1))
    else crash=$((crash+1)); fi
  done
  printf "%-9s correct=%2d wrong=%2d crash=%2d  corrupt-runs=%2d/%d  total-ih=%d\n" \
    "$label" "$ok" "$wrong" "$crash" "$corrupt" "$N" "$totih"
}

echo "== SB-CRASH-04 reg-spill A/B (N=$N, $WHICH $ITERS, GC_STRESS) =="
run_cfg "off"     ""
run_cfg "spill"   "CRATONVM_JIT_SAFEPOINT_REG_SPILL=1"
run_cfg "nostore" "CRATONVM_JIT_SAFEPOINT_REG_SPILL=nostore"
echo "done"
