#!/bin/bash
# usage: cmp-img.sh <cvm-binary> <outdir> [probes...]
# Per probe and mode: HotSpot 25, cold CVM, capture run (auto miss), restored run.
# Compares stdout+rc of each CVM arm against HotSpot, and stderr of restored vs cold.
# A third, verbose restored run proves the image was actually used.
CVM=$1; OUT=$2; shift 2
HS=/usr/lib/jvm/java-25-openjdk-amd64/bin/java
CP=/root/p5kit/probes/cls
mkdir -p $OUT
PROBES=${@:-"Hello P5Zip P5Math P5Exc P5Regex P5Nio P5Misc"}
for p in $PROBES; do
  [ -f $OUT/$p.hs.out ] || { timeout 60 $HS -cp $CP $p > $OUT/$p.hs.out 2>$OUT/$p.hs.err; echo "rc=$?" >> $OUT/$p.hs.out; }
  for m in jdk-only compatible; do
    D=$OUT/img-$p-$m; rm -rf $D; mkdir -p $D
    timeout 120 $CVM --$m -cp $CP $p > $OUT/$p.$m.cold.out 2>$OUT/$p.$m.cold.err; echo "rc=$?" >> $OUT/$p.$m.cold.out
    CRATONVM_BOOT_IMAGE=auto${CAPX} CRATONVM_BOOT_IMAGE_DIR=$D timeout 120 $CVM --$m -cp $CP $p > $OUT/$p.$m.cap.out 2>$OUT/$p.$m.cap.err; echo "rc=$?" >> $OUT/$p.$m.cap.out
    CRATONVM_BOOT_IMAGE=auto CRATONVM_BOOT_IMAGE_DIR=$D timeout 120 $CVM --$m -cp $CP $p > $OUT/$p.$m.rst.out 2>$OUT/$p.$m.rst.err; echo "rc=$?" >> $OUT/$p.$m.rst.out
    CRATONVM_BOOT_IMAGE=auto,verbose CRATONVM_BOOT_IMAGE_DIR=$D timeout 120 $CVM --$m -cp $CP $p > /dev/null 2>$OUT/$p.$m.rstv.err
    used=$(grep -c "boot image: restored /" $OUT/$p.$m.rstv.err)
    for arm in cold cap rst; do
      if cmp -s $OUT/$p.hs.out $OUT/$p.$m.$arm.out; then r=SAME; else r=DIFF; fi
      echo "$r $p $m $arm"
      [ $r = DIFF ] && diff $OUT/$p.hs.out $OUT/$p.$m.$arm.out | head -6
    done
    if cmp -s $OUT/$p.$m.cold.err $OUT/$p.$m.rst.err; then echo "SAME-ERR $p $m rst-vs-cold"; else echo "DIFF-ERR $p $m rst-vs-cold"; diff $OUT/$p.$m.cold.err $OUT/$p.$m.rst.err | head -6; fi
    if cmp -s $OUT/$p.$m.cold.err $OUT/$p.$m.cap.err; then echo "SAME-ERR $p $m cap-vs-cold"; else echo "DIFF-ERR $p $m cap-vs-cold"; diff $OUT/$p.$m.cold.err $OUT/$p.$m.cap.err | head -6; fi
    echo "IMAGE-USED=$used $p $m :: $(ls $D)"
  done
done
