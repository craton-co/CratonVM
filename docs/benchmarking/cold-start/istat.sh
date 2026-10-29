#!/bin/bash
# usage: istat.sh <bin> <mode> <class> [n] -- median instructions:u, task-clock ms, wall ms over n runs
B=$1; M=$2; C=$3; N=${4:-5}; CP=/root/p5kit/probes/cls
for i in $(seq $N); do
  if [ "$B" = hs ]; then perf stat -x, -e instructions:u,task-clock /usr/lib/jvm/java-25-openjdk-amd64/bin/java -cp $CP $C 2>&1 >/dev/null | tr '\n' ' '
  else perf stat -x, -e instructions:u,task-clock $B $M -cp $CP $C 2>&1 >/dev/null | grep -E "instructions|task-clock" | tr '\n' ' '; fi; echo
done | awk -F, '{for(i=1;i<=NF;i++){if($i~/instructions/)ins=$(i-2); if($i~/task-clock/)tc=$(i-2)} print ins/1e6, tc}' | sort -n | awk '{a[NR]=$1; t[NR]=$2} END {m=int((NR+1)/2); printf "instr_med=%.1fM (min %.1fM) task_ms_med=%.1f\n", a[m], a[1], t[m]}'
