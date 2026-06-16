#!/usr/bin/env bash
# spring-bug-10: does SHADOW_PIN (pinned shadow marking) close the race without
# the movable-path SIGSEGV and without OOMing bt18?
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
VM="${VM:-C:/craton/CratonVM-springbugs/target/release/cratonvm.exe}"
JDK25="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
BENCH="C:/craton/CratonVM-springbugs/bench"
KRUN="C:\\craton\\CratonVM-spring\\spring-suite"
CPF="C:/craton/cratonvm/apps/spring-framework/spring-context/build/cratonvm-testcp.txt"
CP="$KRUN;$(tr -d '\r' < "$CPF")"
DIR="C:/craton/cratonvm/apps/spring-framework/spring-context/build/classes/java/test"

bt(){ # label  ENV...
  local label="$1"; shift
  local out rc
  out=$(env "$@" timeout 180 "$VM" --java-home "$JDK25" --stack-dump-on-timeout 0 -cp "$BENCH" BinTreesOnly 2>&1); rc=$?
  local v="?"
  if printf '%s' "$out" | grep -qaiE 'ACCESS_VIOLATION|SIGSEGV|Native frames'; then v="CRASH(rc=$rc)";
  elif printf '%s' "$out" | grep -qaiE 'OutOfMemory|OOM|heap (space|exhausted)|cannot allocate'; then v="OOM";
  elif [ "$rc" = 124 ]; then v="timeout";
  else v=$(printf '%s' "$out" | grep -aoE '\[[0-9]+\]' | head -1); v="${v:-rc=$rc} $(printf '%s' "$out" | grep -aoE '[0-9]+ ms' | head -1)"; fi
  printf 'bt18 %-22s -> %s\n' "$label" "$v"
}

spring(){ # label  ENV...
  local label="$1"; shift
  mapfile -t AJ < <(cd "$DIR" && find ./org/springframework/aop/aspectj -name '*Tests.class' ! -name '*$*' | sed 's|^\./||; s|\.class$||; s|/|.|g' | head -14)
  local err; err=$(mktemp)
  local out rc
  out=$(env KRUN_STACK=1 "$@" timeout 280 "$VM" --java-home "$JDK25" --stack-dump-on-timeout 0 -cp "$CP" KRun "${AJ[@]}" 2>"$err"); rc=$?
  local stale; stale=$(grep -ca 'Stale pointer detected' "$err")
  local res; res=$(printf '%s' "$out" | grep -ca '^RESULT')
  local crash="no"; grep -qaiE 'ACCESS_VIOLATION|SIGSEGV|Native frames' "$err" && crash="CRASH"
  printf 'spring %-20s -> stale=%s results=%s crash=%s rc=%s\n' "$label" "$stale" "$res" "$crash" "$rc"
  rm -f "$err"
}

echo "=== bt18 (HotSpot golden = 68332206) ==="
bt "default"
bt "SHADOW(movable)"   CRATONVM_SHADOW_STACK=1
bt "SHADOW+PIN"        CRATONVM_SHADOW_STACK=1 CRATONVM_SHADOW_PIN=1
bt "SHADOW+PIN Xmx512m" CRATONVM_SHADOW_STACK=1 CRATONVM_SHADOW_PIN=1
echo "=== spring aspectj batch (race = stale>0) ==="
spring "default"
spring "SHADOW(movable)" CRATONVM_SHADOW_STACK=1
spring "SHADOW+PIN"    CRATONVM_SHADOW_STACK=1 CRATONVM_SHADOW_PIN=1
