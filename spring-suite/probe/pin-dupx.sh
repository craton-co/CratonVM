#!/usr/bin/env bash
# spring-bug-11: identify the crashing dup_x1 method.
#  (1) CRATONVM_DBG_JIT_NAMES=1 -> crash report names each JIT frame.
#  (2) CRATONVM_DBG_DUPX_METHODS=1 -> lists every compiled method with dup_x1.
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
VM="${VM:-C:/craton/CratonVM-springbugs/target/release/cratonvm.exe}"
JDK25="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
KRUN="C:\\craton\\CratonVM-spring\\spring-suite"
CPF="C:/craton/cratonvm/apps/spring-framework/spring-context/build/cratonvm-testcp.txt"
CP="$KRUN;$(tr -d '\r' < "$CPF")"
CLASS="${CLASS:-org.springframework.scripting.groovy.GroovyScriptEvaluatorTests}"

echo "===== RUN: JIT_NAMES + DUPX_METHODS ====="
CRATONVM_DBG_JIT_NAMES=1 CRATONVM_DBG_DUPX_METHODS=1 timeout 120 "$VM" \
  --java-home "$JDK25" --stack-dump-on-timeout 0 -cp "$CP" KRun "$CLASS" \
  > /tmp/pin.out 2>/tmp/pin.err; echo "rc=$?"

echo "===== Native frames (JIT-named) ====="
grep -aA14 'Native frames (most recent call first)' /tmp/pin.out /tmp/pin.err 2>/dev/null | grep -aE 'Native frames|jit:|external/jit|exe\+' | head -20

echo "===== faulting registers ====="
grep -aE 'rax=|rip=' /tmp/pin.out /tmp/pin.err 2>/dev/null | head -4

echo "===== dup_x1 methods that JIT-compiled (last 40) ====="
grep -aE '^\[DUPX1\]' /tmp/pin.err /tmp/pin.out 2>/dev/null | tail -40
