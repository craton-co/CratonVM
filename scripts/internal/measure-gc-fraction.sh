#!/usr/bin/env bash
# Measure the GC-sweep fraction of the bt18 throughput gap on the current build.
# The design's mandated "MEASURE FIRST" prerequisite for default-moving-young-gen.
set -u
CV="C:/craton/CratonVM-movingyoung/cvmove.exe"
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"
CP="/tmp/btbench"
HEAP="${HEAP:--Xmx8g}"

run() { # label  envset  depth
  local lbl="$1" env="$2" d="$3"
  local s e out
  s=$(date +%s%3N)
  out=$(env $env "$CV" $HEAP --java-home "$JDK" -cp "$CP" binarytrees "$d" 2>/tmp/cv.err)
  e=$(date +%s%3N)
  local sweeps; sweeps=$(grep -c 'sp-stats' /tmp/cv.err 2>/dev/null || echo 0)
  printf "%-22s d=%s  checksum=%-9s  %6dms  sweeps=%s\n" "$lbl" "$d" "${out:-FAIL}" "$((e-s))" "$sweeps"
}

echo "== bt18 GC-fraction measurement (heap=$HEAP) =="
echo "-- HotSpot reference (same load) --"
for d in 16 18; do s=$(date +%s%3N); out=$("$JDK/bin/java" -cp "$CP" binarytrees "$d"); e=$(date +%s%3N); printf "%-22s d=%s  checksum=%-9s  %6dms\n" "hotspot" "$d" "$out" "$((e-s))"; done
echo "-- CratonVM (JIT on, default = non-moving sweep under JIT) --"
run "craton-default"      "CRATONVM_SP_STATS=1" 16
run "craton-default"      "CRATONVM_SP_STATS=1" 18
echo "-- CratonVM NO_GC (isolates GC cost: time delta = GC share) --"
run "craton-nogc"         "CRATONVM_NO_GC=1"    16
run "craton-nogc"         "CRATONVM_NO_GC=1"    18
echo "-- CratonVM forced-moving (under-counts; shadow off) --"
run "craton-forcemoving"  "CRATONVM_DBG_FORCE_MOVING=1 CRATONVM_SP_STATS=1" 18
echo "-- CratonVM forced-moving + shadow (the incomplete Route-A path) --"
run "craton-moving+shadow" "CRATONVM_DBG_FORCE_MOVING=1 CRATONVM_SHADOW_STACK=1 CRATONVM_SP_STATS=1" 18
echo done
