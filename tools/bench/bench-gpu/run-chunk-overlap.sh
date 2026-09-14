#!/usr/bin/env bash
# What the chunked writeback overlap is worth, as a same-binary A/B.
#
# `CRATONVM_GPU_CHUNKS=1` turns chunking off inside one binary, so the ratio
# below is the overlap and nothing else -- no rebuild, no cross-binary
# comparison, no dependence on this host holding a clock still between two
# builds. The ray tracer record measured 1.51x here where a raw prototype of
# the same shape reached 1.67x, and attributed the gap to the bridge's
# per-launch `last_write` event bookkeeping, which a hand-written prototype
# does not do. Re-run this after touching that bookkeeping.
#
# Rounds alternate which arm runs first, for the same reason every other
# harness here does.
#
# Usage: run-chunk-overlap.sh <cratonvm.exe> [width height rounds]
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERE_W="$(cd "$HERE" && pwd -W)"
CV="${1:?usage: run-chunk-overlap.sh <cratonvm.exe> [width height rounds]}"
W="${2:-1920}"
H="${3:-1440}"
ROUNDS="${4:-5}"
ITERS="${ITERS:-30}"
# Past 8K the frame stops fitting a default heap: 11520x6480 is an `int[]`
# of 298 MB.
XMX="${XMX:-6g}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
GPU_JAR="${GPU_JAR:?set GPU_JAR to the craton-gpu annotations jar}"
CP="$HERE_W;$GPU_JAR"

"$JDK/bin/javac" -cp "$GPU_JAR" -d "$HERE" "$HERE/RayTracerKernel.java" || exit 1

run() {
  CRATONVM_GPU_CHUNKS="$1" "$CV" --java-home "$JDK" --gpu --gpu-min-work 1 --Xmx "$XMX" \
    -cp "$CP" RayTracerKernel "$W" "$H" "$ITERS" 2>/dev/null \
    | sed -n 's/.*[[:space:]]best_ms=\([^[:space:]]*\).*/\1/p'
}

echo "RayTracerKernel ${W}x${H}, $ROUNDS rounds of $ITERS iterations"
printf '%-6s %12s %12s %10s   %s\n' round chunks=1 chunks=8 speedup order
tmp="$(mktemp)"
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 1 ]; then
    off=$(run 1); on=$(run 8); order="off-then-on"
  else
    on=$(run 8); off=$(run 1); order="on-then-off"
  fi
  sp=$(awk -v a="$off" -v b="$on" 'BEGIN { if (b+0 > 0) printf "%.3fx", a/b; else print "n/a" }')
  printf '%-6s %12s %12s %10s   %s\n' "$r" "$off" "$on" "$sp" "$order"
  echo "$off $on" >> "$tmp"
done
echo
awk '{ so += $1; sn += $2; n++ } END {
       printf "paired mean over %d rounds: off %.4f ms, on %.4f ms -- %.3fx from the overlap\n",
              n, so/n, sn/n, so/sn }' "$tmp"
rm -f "$tmp"
