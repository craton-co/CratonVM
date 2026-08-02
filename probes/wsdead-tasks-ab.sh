#!/usr/bin/env bash
# usage: tasks-ab.sh <n> <exeA> <tagA> <exeB> <tagB>
set -u
P=/data/data/wsdead-probes
N=$1; A=$2; TA=$3; B=$4; TB=$5
one() { # tag exe round
  local out=$P/tk-$1-$3.log
  PROBE_COMBOS=0 bash $P/probe-run.sh "$2" "$out" 0 300 >/dev/null 2>&1
  local tasks=$(grep -o "tasks=[0-9]*" "$out" | tail -1 | cut -d= -f2)
  local sends=$(grep -o "serverSends=[0-9]*" "$out" | tail -1 | cut -d= -f2)
  local pool=$(grep -o "pool=[0-9]*" "$out" | cut -d= -f2 | sort -n | tail -1)
  local delay=$(grep -o "closeDelay=[0-9.]*" "$out" | tail -1 | cut -d= -f2)
  local rej=$(grep -c "Executor rejected socket" "$out")
  local res=PASS; grep -q "Close delay was" "$out" && res=CLOSE_DELAY
  grep -q "^OK (" "$out" || [ "$res" = CLOSE_DELAY ] || res=OTHER
  echo "$1 r$3 $res tasks=$tasks sends=$sends ratio=$(awk -v t="${tasks:-0}" -v s="${sends:-1}" "BEGIN{printf \"%.2f\", t/(s==0?1:s)}") maxpool=$pool delay=$delay rejects=$rej"
}
for r in $(seq 1 $N); do
  if [ $((r % 2)) -eq 1 ]; then one $TA "$A" $r; one $TB "$B" $r; else one $TB "$B" $r; one $TA "$A" $r; fi
done
