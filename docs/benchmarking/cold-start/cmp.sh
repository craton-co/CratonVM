#!/bin/bash
# usage: cmp.sh <cvm-binary> <outdir> [probes...]  -- stdout+rc per probe vs HotSpot 25, both CVM modes
CVM=$1; OUT=$2; shift 2
HS=/usr/lib/jvm/java-25-openjdk-amd64/bin/java
CP=/root/p5kit/probes/cls
mkdir -p $OUT
PROBES=${@:-"Hello P5Zip P5Math P5Exc P5Regex P5Nio P5Misc"}
for p in $PROBES; do
  [ -f $OUT/$p.hs.out ] || { timeout 60 $HS -cp $CP $p > $OUT/$p.hs.out 2>$OUT/$p.hs.err; echo "rc=$?" >> $OUT/$p.hs.out; }
  for m in jdk-only fast-boot; do
    timeout 120 $CVM --$m -cp $CP $p > $OUT/$p.$m.out 2>$OUT/$p.$m.err; echo "rc=$?" >> $OUT/$p.$m.out
    if cmp -s $OUT/$p.hs.out $OUT/$p.$m.out; then echo "SAME $p $m"; else echo "DIFF $p $m"; diff $OUT/$p.hs.out $OUT/$p.$m.out | head -8; fi
  done
done
