#!/usr/bin/env bash
# `RayTracerCpuAblation` on both VMs, alternating, minimum per construct.
#
# One reading of each arm is not enough on this box. Two passes taken forty
# minutes apart disagreed by 3x on HotSpot's cheapest constructs -- `copy`
# read 1.399 ns/element under load and 0.479 quiet -- while CratonVM's
# barely moved, which turns a 3.3x ratio into a 10.9x one without either VM
# changing. Alternate the arms and keep the minimum: contention only ever
# makes a run slower, so the minimum is the estimator that survives it.
#
# Usage: run-cpu-ablation.sh <cratonvm.exe> [n] [rounds]
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERE_W="$(cd "$HERE" && pwd -W)"
CV="${1:?usage: run-cpu-ablation.sh <cratonvm.exe> [n] [rounds]}"
N="${2:-2764800}"
ROUNDS="${3:-3}"
ITERS="${ITERS:-12}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"

"$JDK/bin/javac" -d "$HERE" "$HERE/RayTracerCpuAblation.java" || exit 1

# `<construct> <ns_per_elem>` per line.
hs()  { "$JDK/bin/java" -Xmx4g -cp "$HERE_W" RayTracerCpuAblation "$N" "$ITERS" 2>/dev/null; }
cv()  { "$CV" --java-home "$JDK" --Xmx 4g -cp "$HERE_W" RayTracerCpuAblation "$N" "$ITERS" 2>/dev/null; }
pairs() { sed -n 's/^\([a-z]*\) *best_ms=.*ns_per_elem= *\([0-9,.]*\).*/\1 \2/p' | tr ',' '.'; }

tmp="$(mktemp)"
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 1 ]; then
    hs | pairs | sed 's/^/hs /' >> "$tmp"; cv | pairs | sed 's/^/cv /' >> "$tmp"
  else
    cv | pairs | sed 's/^/cv /' >> "$tmp"; hs | pairs | sed 's/^/hs /' >> "$tmp"
  fi
done

echo "RayTracerCpuAblation n=$N, $ROUNDS alternating rounds of best-of-$ITERS"
awk '{ k = $1 "|" $2; if (!(k in m) || $3+0 < m[k]) m[k] = $3+0; if (!($2 in seen)) { order[++o] = $2; seen[$2] = 1 } }
     END {
       printf "%-10s %12s %12s %8s\n", "construct", "HotSpot", "CratonVM", "ratio"
       for (i = 1; i <= o; i++) {
         c = order[i]
         printf "%-10s %12.3f %12.3f %7.2fx\n", c, m["hs|" c], m["cv|" c], m["cv|" c] / m["hs|" c]
       }
     }' "$tmp"
rm -f "$tmp"
