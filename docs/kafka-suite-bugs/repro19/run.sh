#!/usr/bin/env bash
# Run the three bug-19 primitive repros under CratonVM across the doc's config
# matrix. Each run has an external 30s timeout; rc=124 = hang.
set -u
VM="/c/craton/CratonVM/target/release/cratonvm.exe"
CP="/c/craton/CratonVM/docs/kafka-suite-bugs/repro19"
TIMEOUT=30

run() {
  local label="$1"; shift
  local env="$1"; shift
  local cls="$1"; shift
  echo "======== $label :: $cls ========"
  if [ -n "$env" ]; then export $env; fi
  timeout $TIMEOUT "$VM" "$@" -cp "$CP" "$cls"
  local rc=$?
  if [ -n "$env" ]; then unset "${env%%=*}"; fi
  echo "[rc=$rc]$([ $rc -eq 124 ] && echo '  <<< HANG (timed out)')"
  echo
}

for cls in R2WaitNotify R3Park R1Condition; do
  run "default-JIT"  ""                   "$cls"
  run "nojit"        ""                   "$cls" --nojit
  run "REAL_AQS"     "CRATONVM_REAL_AQS=1" "$cls"
done
