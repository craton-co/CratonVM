#!/usr/bin/env bash
# Interleaved A/B of the direct-call arg fix (5fa4cdb6fb) on the WebSocket
# close-delay failure. Both arms are origin/dev@492ddedd2; the "pre" arm has
# ONLY jit/src/x64.rs reverted to its pre-fix state.
set -u
P=/data/data/wsdead-probes
N="${1:-20}"
run() { # tag exe round
  local out=$P/e2e-$1-$3.log
  PROBE_COMBOS=0 bash $P/probe-run.sh "$2" "$out" 0 300 >/dev/null 2>&1
  local res=PASS
  grep -q 'Close delay was' "$out" && res=CLOSE_DELAY
  grep -q '^OK (' "$out" || [ "$res" = CLOSE_DELAY ] || res=OTHER
  local tasks
  tasks=$(grep -o 'tasks=[0-9]*' "$out" | tail -1 | cut -d= -f2)
  local pool
  pool=$(grep -o 'pool=[0-9]*' "$out" | cut -d= -f2 | sort -n | tail -1)
  local rej
  rej=$(grep -c 'Executor rejected socket' "$out")
  echo "$1 r$3 $res rejects=$rej tasks=$tasks maxpool=$pool load=$(cut -d' ' -f1 /proc/loadavg)"
}
for r in $(seq 1 "$N"); do
  if [ $((r % 2)) -eq 1 ]; then
    run pre  $P/cvm-wsdead-pre-20260801  "$r"
    run post $P/cvm-wsdead-post-20260801 "$r"
  else
    run post $P/cvm-wsdead-post-20260801 "$r"
    run pre  $P/cvm-wsdead-pre-20260801  "$r"
  fi
done
echo "--- totals ---"
for t in pre post; do
  cd_n=$(grep -l 'Close delay was' $P/e2e-$t-*.log 2>/dev/null | wc -l)
  tot=$(ls $P/e2e-$t-*.log 2>/dev/null | wc -l)
  rj=$(grep -l 'Executor rejected socket' $P/e2e-$t-*.log 2>/dev/null | wc -l)
  echo "$t: CLOSE_DELAY $cd_n/$tot  runs_with_rejects=$rj"
done
