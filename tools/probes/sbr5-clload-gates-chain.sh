#!/bin/bash
# usage: gates-chain.sh <bin>  -> probes through a manifest-only booter jar (Class-Path chain): diffs vs HotSpot, new bin and cvm-sbr5-base
cd /data/sbr5/clload; JH=/data/jdkimages/jdk25-linux/jdk-25.0.4+7; NEW=$1; BASE=/data/sbr5/bin/cvm-sbr5-base
BOOT=/data/sbr5/clload/chain/booter.jar
clean() { grep -a -v "^\[cratonvm\]\|^CRATONVM\|^    java\|^    at \|^Exception in thread" ; }
run() { local name=$1 dirs=$2; shift 2; local cp="$dirs:$BOOT"
  $JH/bin/java -cp "$cp" "$@" 2>&1 | clean > gc-hs.txt
  for mode in "" "--compatible"; do for b in $NEW $BASE; do
    JAVA_HOME=$JH $b $mode -cp "$cp" "$@" 2>&1 | clean > gc-vm.txt
    printf "%-16s %-13s %-22s diffs=%s\n" "$name" "${mode:---jdk-only}" "$(basename $b)" "$(diff gc-hs.txt gc-vm.txt | grep -c '^[<>]')"
  done; done; }
run Variants "var/probe:var/dir" Sbr5ClloadVariants
run SealViolation so Sbr5ClloadSealViolation
run Identity so Sbr5ClloadIdentity
run Sealed so Sbr3RefmiscSealed
run Class so Sbr3RefmiscClass
run SplitAB sp Sbr5ClloadSplit AB
run SplitBA sp Sbr5ClloadSplit BA
run Section sect Sbr5ClloadSection
run Missing ms Sbr5ClloadMissing
echo CHAIN-GATES-DONE
