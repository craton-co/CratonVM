#!/bin/bash
# zerocell-ab.sh <reps> — interleaved JIT vs --nojit on JitZeroCellProbe.
#
# The first pass of this A/B read the DEFAULT (compact-layout) JIT arm as
# `Object(None)` once and `Int(0)` later, which would have gone into the write-up
# as "compact layout is the discriminator". It is not: the discriminator is
# which allocation arm served the LAST allocation, and that is a compile-timing
# coin toss. Repeat it, interleaved, and quote the split rather than one run.
set -u
REPS="${1:-4}"
EXE=${ZC_EXE:-/data/bin/cratonvm-nres2-20260824}
JDK=/data/toolchain/jdk-25
D=/data/nres/zerocell; mkdir -p "$D"
: > "$D/OUT.txt"
one() { # $1=arm $2=idx $3..=extra vm args
  local arm="$1" i="$2"; shift 2
  local L="$D/$arm-$i.log"
  timeout -k 10 140 "$EXE" --java-home "$JDK" --Xmx 512m --stack-dump-on-timeout=30 \
      -cp /data/nres/probes -XX:+UseG1GC "$@" JitZeroCellProbe > "$L" 2>&1
  local cell nullness
  cell=$(grep -ao "result=Some([A-Za-z()0-9]*)" "$L" | head -1)
  nullness=$(grep -ao "result==null is [a-z]*" "$L" | head -1)
  printf "%-6s %-3s %-28s %s\n" "$arm" "$i" "${cell:--}" "${nullness:--}" >> "$D/OUT.txt"
}
for i in $(seq 1 "$REPS"); do
  one JIT "$i"
  one NOJIT "$i" --nojit
done
echo "ZCDONE" >> "$D/OUT.txt"
