#!/usr/bin/env bash
# Correct-output comparison across configs (the HONEST metric: DONE + right answer).
set -u
CV="C:/craton/CratonVM-sbreg/cratonvm-sbreg.exe"
JDK="C:/Program Files/Java/jdk-25"
cd C:/craton/CratonVM/apps/spring-boot/buildSrc || exit 1
CP="runner;$(cat test-classpath.txt)"
ITERS="${1:-200000}"; N="${2:-3}"

cmp() {
  local lbl="$1"; local cliflag="$2"; local envset="$3"
  local ok=0 done=0 seg=0 hang=0
  for i in $(seq 1 "$N"); do
    timeout 150 env $envset CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
      "$CV" $cliflag --java-home "$JDK" -cp "$CP" MinRegexProbe code "$ITERS" > /tmp/cc.log 2>&1
    local rc=$?
    grep -q 'a `X` b' /tmp/cc.log && ok=$((ok+1))
    grep -q "DONE $ITERS" /tmp/cc.log && done=$((done+1))
    [ "$rc" = 124 ] && hang=$((hang+1))
    [ "$rc" -ge 139 ] 2>/dev/null && seg=$((seg+1))
  done
  printf "%-16s correct=%d/%d DONE=%d/%d segv=%d hang=%d\n" "$lbl" "$ok" "$N" "$done" "$N" "$seg" "$hang"
}

echo "== correct-output comparison (code $ITERS, natural GC, N=$N) =="
cmp "nojit"           "--nojit"  ""
cmp "default(off)"    ""         ""
cmp "spill"           ""         "CRATONVM_JIT_SAFEPOINT_REG_SPILL=1"
cmp "shadow"          ""         "CRATONVM_SHADOW_STACK=1"
cmp "disable_inline"  ""         "CRATONVM_JIT_DISABLE_INLINE_NEW=1"
echo done
