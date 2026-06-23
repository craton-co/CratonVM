#!/usr/bin/env bash
# BUG-03 validation: run the multithreaded JIT/GC reproducer many times in
# both modes and summarize outcomes.
#   $1 = mode: "off" (baseline) | "on" (fix) | "strict" (fix + STRICT)
#   $2 = run count (default 10)
#   GC_STRESS overridable (default 2048)
set +e
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CV="${CV:-C:/craton/CratonVM-xtjitroots/target/release/cratonvm-xtjit.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot}"
MODE="${1:-off}"
RUNS="${2:-10}"
GCS="${GC_STRESS:-2048}"
export MSYS_NO_PATHCONV=1
# The reproducer is intentionally GC-heavy; disable the VM's default
# stack-dump watchdog so a slow-but-correct run completes instead of being
# aborted (rc=127) mid-flight.
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

EXTRA=""
case "$MODE" in
  off)       unset CRATONVM_XT_JIT_ROOT_SCAN; unset CRATONVM_STRICT_JIT_ROOTS ;;
  offstrict) unset CRATONVM_XT_JIT_ROOT_SCAN; export CRATONVM_STRICT_JIT_ROOTS=1 ;;
  on)        export CRATONVM_XT_JIT_ROOT_SCAN=1; unset CRATONVM_STRICT_JIT_ROOTS ;;
  strict)    export CRATONVM_XT_JIT_ROOT_SCAN=1; export CRATONVM_STRICT_JIT_ROOTS=1 ;;
  *) echo "bad mode $MODE"; exit 2 ;;
esac

echo "== BUG-03 validate mode=$MODE runs=$RUNS GC_STRESS=$GCS CV=$CV =="
ok=0; corrupt=0; crash=0; other=0; gap_total=0
for i in $(seq 1 "$RUNS"); do
  out="$(CRATONVM_DBG_GC_STRESS="$GCS" "$CV" --java-home "$JDK" -cp "$HERE" XtJitRepro 2>&1)"
  rc=$?
  gaps="$(printf '%s' "$out" | grep -c 'cross_thread_jit_gap')"
  gap_total=$((gap_total + gaps))
  if printf '%s' "$out" | grep -q 'CORRUPTION DETECTED\|CORRUPT id='; then
    corrupt=$((corrupt+1)); tag="CORRUPT"
  elif [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q 'RESULT: OK'; then
    ok=$((ok+1)); tag="OK"
  elif [ "$rc" -ge 128 ] || [ "$rc" -eq 139 ] || [ "$rc" -eq 134 ]; then
    crash=$((crash+1)); tag="CRASH(rc=$rc)"
  else
    other=$((other+1)); tag="OTHER(rc=$rc)"
  fi
  echo "  run $i: $tag  gaps=$gaps"
  # On a panic (strict), show the tail.
  if printf '%s' "$out" | grep -q 'STRICT_JIT_ROOTS\|panicked'; then
    printf '%s\n' "$out" | grep -i 'panic\|STRICT_JIT' | head -3 | sed 's/^/      /'
  fi
done
echo "== summary mode=$MODE: ok=$ok corrupt=$corrupt crash=$crash other=$other gap_hits_total=$gap_total =="
