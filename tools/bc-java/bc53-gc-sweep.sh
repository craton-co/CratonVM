#!/bin/bash
# All 53 bc-java AllTests classes, ONE shard, once per collector.
#
# The collector is a runtime selector (-XX:+Use*GC), not a build feature, so all
# three arms run the SAME binary — which is what makes this a comparison rather
# than three unrelated runs.
#
# CLASS_TIMEOUT is 1800s to match the existing 53-class baseline in
# docs/known-issues/bc-java/. Two pqc classes exceed it in every arm and are
# recorded as HANG there too; they are known to be slow rather than stuck
# (bug-bcjava-pqc-53class-20260818.md).
set -u

BIN="${BIN:-/data/vm-gcsweep-20260819.bin}"
STAMP="$(date -Is)"
echo "sweep start $STAMP   binary=$BIN   md5=$(md5sum "$BIN" | cut -d' ' -f1)"

for gc in zgc g1 generational; do
  case "$gc" in
    zgc)          FLAG="-XX:+UseZGC" ;;
    g1)           FLAG="-XX:+UseG1GC" ;;
    generational) FLAG="-XX:+UseGenerationalGC" ;;
  esac

  OUT="/data/bc53-gc-$gc"
  rm -rf "$OUT"
  mkdir -p "$OUT"

  echo "=== ARM $gc ($FLAG) start $(date -Is) ==="
  SHARD=0 SHARDS=1 MODE=cratonvm CRATONVM_BIN="$BIN" \
    CVM_JIT_FLAG="$FLAG" CLASS_TIMEOUT=1800 XMX=1g OUTDIR="$OUT" \
    /data/bc53-shard.sh
  echo "=== ARM $gc end $(date -Is) ==="

  awk -F'\t' '$1!~/^SHARD_/ {c[$2]++} END {printf "  %s: PASS=%d FAIL=%d HANG=%d\n", "'"$gc"'", c["PASS"], c["FAIL"], c["HANG"]}' \
    "$OUT/results-0.tsv"
done

echo "sweep end $(date -Is)"
