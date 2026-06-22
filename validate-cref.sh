#!/usr/bin/env bash
# Validate the compact reference-field layout (CRATONVM_COMPACT_REF_FIELDS).
# Compares CratonVM checksums (flag OFF and ON) against the HotSpot-verified
# golden values for object-binarytrees at several depths, plus a GC_STRESS run.
set -u
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"
VM="./cvmcref.exe"
REPO="docs/known-issues/repros/gc-stress-bintrees-main-args"
WORK="$(pwd)/.cref-bench"
mkdir -p "$WORK"
cp "$REPO/binarytrees.java" "$WORK/binarytrees.java"
"$JDK/bin/javac.exe" -d "$WORK" "$WORK/binarytrees.java" || { echo "javac failed"; exit 1; }

declare -A GOLD=( [10]=135854 [14]=3222190 [16]=14985902 [18]=68332206 )

run() {  # $1=depth  $2=flagval(0/1)  $3=extra-env
  local depth="$1" flag="$2" extra="$3"
  CRATONVM_COMPACT_REF_FIELDS="$flag" env $extra \
    "$VM" --java-home "$JDK" -cp "$WORK" binarytrees "$depth" 2>/dev/null | tr -d '[:space:]'
}

echo "=== binarytrees checksum A/B (compact OFF vs ON) ==="
fail=0
for d in 10 14 16 18; do
  off=$(run "$d" "" "")
  on=$(CRATONVM_COMPACT_REF_FIELDS=1 "$VM" --java-home "$JDK" -cp "$WORK" binarytrees "$d" 2>/dev/null | tr -d '[:space:]')
  g="${GOLD[$d]}"
  status="OK"
  [ "$off" != "$g" ] && { status="OFF-MISMATCH"; fail=1; }
  [ "$on"  != "$g" ] && { status="ON-MISMATCH"; fail=1; }
  printf "bt%-2s gold=%-9s off=%-9s on=%-9s  %s\n" "$d" "$g" "$off" "$on" "$status"
done

echo "=== GC_STRESS bt16 (compact ON) ==="
gs=$(CRATONVM_COMPACT_REF_FIELDS=1 CRATONVM_GC_STRESS=1 "$VM" --java-home "$JDK" -cp "$WORK" binarytrees 16 2>/dev/null | tr -d '[:space:]')
[ "$gs" = "14985902" ] && echo "bt16 GC_STRESS on=$gs OK" || { echo "bt16 GC_STRESS on=$gs MISMATCH(exp 14985902)"; fail=1; }

echo "=== node size probe (compact ON, alloc trace if available) ==="
echo "(node = HEADER(40) + 2*8 = 56 bytes compact vs 40 + 2*16 = 72 legacy)"

[ "$fail" = 0 ] && echo "ALL PASS" || echo "FAILURES PRESENT"
exit $fail
