#!/bin/bash
# usage: gates.sh <bin>  -> per probe, per mode: differing lines vs HotSpot, and the same for cvm-sbr5-base
cd /data/sbr5/clload; JH=/data/jdkimages/jdk25-linux/jdk-25.0.4+7; NEW=$1; BASE=/data/sbr5/bin/cvm-sbr5-base
clean() { grep -a -v "^\[cratonvm\]\|^CRATONVM\|^    java\|^    at \|^Exception in thread" ; }
run() { # name classpath main args...
  local name=$1 cp=$2; shift 2
  $JH/bin/java -cp "$cp" "$@" 2>&1 | clean > g-hs.txt
  for mode in "" "--compatible"; do
    for b in $NEW $BASE; do
      JAVA_HOME=$JH $b $mode -cp "$cp" "$@" 2>&1 | clean > g-vm.txt
      printf "%-34s %-13s %-22s diffs=%s\n" "$name" "${mode:---jdk-only}" "$(basename $b)" "$(diff g-hs.txt g-vm.txt | grep -c '^[<>]')"
    done
  done
}
V=var; run Variants "$V/probe:$V/mr.jar:$V/signed.jar:$V/sealsec.jar:$V/dir" Sbr5ClloadVariants
CP2=so:pkg.jar:other.jar:$(cat cp.txt)
run SealViolation "$CP2" Sbr5ClloadSealViolation
run Identity "$CP2" Sbr5ClloadIdentity
run Sealed "$CP2" Sbr3RefmiscSealed
run Loader "so:pkg.jar:$(cat cp.txt)" Sbr3RefmiscLoader
run Class "so:pkg.jar:$(cat cp.txt)" Sbr3RefmiscClass
run SplitAB "sp:sp/s1.jar:sp/s2.jar" Sbr5ClloadSplit AB
run SplitBA "sp:sp/s1.jar:sp/s2.jar" Sbr5ClloadSplit BA
run Missing "ms:ms/msjar.jar" Sbr5ClloadMissing
run Section "sect:sect/sect.jar" Sbr5ClloadSection
run Concurrent "so:$(cat cp.txt)" Sbr5ClloadConcurrent
echo GATES-DONE
