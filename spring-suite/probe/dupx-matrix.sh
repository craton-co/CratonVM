#!/usr/bin/env bash
# spring-bug-11 isolation matrix: run the crashing Groovy test under several
# JIT dup-family flag variants and classify each as CRASH vs no-crash.
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
VM="${VM:-C:/craton/CratonVM-springbugs/target/release/cratonvm.exe}"
JDK25="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
KRUN="C:\\craton\\CratonVM-spring\\spring-suite"
CPF="C:/craton/cratonvm/apps/spring-framework/spring-context/build/cratonvm-testcp.txt"
CP="$KRUN;$(tr -d '\r' < "$CPF")"
CLASS="${CLASS:-org.springframework.scripting.groovy.GroovyScriptEvaluatorTests}"
TO="${TO:-110}"

run() {  # name  ENV...
  local name="$1"; shift
  local out rc
  out=$(env "$@" timeout "$TO" "$VM" --java-home "$JDK25" --stack-dump-on-timeout 0 -cp "$CP" KRun "$CLASS" 2>&1); rc=$?
  local verdict
  if printf '%s' "$out" | grep -qaiE 'EXCEPTION_ACCESS_VIOLATION|SIGSEGV|Native frames'; then
    verdict="CRASH(rc=$rc)"
  elif [ "$rc" = "124" ]; then
    verdict="timeout/no-crash"
  elif printf '%s' "$out" | grep -qa '^RESULT'; then
    verdict="RESULT/no-crash: $(printf '%s' "$out" | grep -m1 '^RESULT' | sed 's/RESULT [^ ]* //')"
  else
    verdict="other(rc=$rc)"
  fi
  printf '%-24s -> %s\n' "$name" "$verdict"
}

echo "CLASS=$CLASS  TO=${TO}s  VM=$VM"
run "baseline"
run "NO_DUPX"         CRATONVM_JIT_NO_DUPX=1
run "NO_DUP_X1"       CRATONVM_JIT_NO_DUP_X1=1
run "NO_DUP_X2"       CRATONVM_JIT_NO_DUP_X2=1
run "EAGER_CANON"     CRATONVM_JIT_DUPX_EAGER_CANON=1
