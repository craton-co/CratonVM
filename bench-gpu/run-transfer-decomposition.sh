#!/usr/bin/env bash
# Decompose the ray tracer's per-frame GPU cost into a fixed per-call part
# and a per-pixel part, at every resolution given.
#
# Why this and not just the wall clock: `run-raytracer-interleaved.sh` says
# how CratonVM compares to TornadoVM at one size, and the margin moves with
# size. Whether that is the fixed cost amortising away or the per-pixel
# rates diverging is not answerable from one number per size -- it needs the
# two components separated, which is what `GpuTransferFloor` is for. It runs
# the same launch shape and the same output bytes with essentially no
# arithmetic, so
#
#   floor(n)   ~ fixed + transfer_per_px * n
#   tracer(n)  ~ fixed + transfer_per_px * n + compute_per_px * n
#
# and the difference is compute. A least-squares line through the floor
# points recovers `fixed` directly, which is the number section 7 of the ray
# tracer record fitted at three sizes below 2.8M pixels and never re-checked
# past it.
#
# Usage: run-transfer-decomposition.sh <cratonvm.exe> [W H]...
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
HERE_W="$(cd "$HERE" && pwd -W)"
CV="${1:?usage: run-transfer-decomposition.sh <cratonvm.exe> [W H]...}"
shift

JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
GPU_JAR="${GPU_JAR:?set GPU_JAR to the craton-gpu annotations jar}"
ITERS="${ITERS:-20}"
CP="$HERE_W;$GPU_JAR"

"$JDK/bin/javac" -cp "$GPU_JAR" -d "$HERE" \
  "$HERE/RayTracerKernel.java" "$HERE/GpuTransferFloor.java" || exit 1

best() { sed -n 's/.*[[:space:]]best_ms=\([^[:space:]]*\).*/\1/p'; }

if [ "$#" -eq 0 ]; then
  set -- 1920 1440 3840 2160 7680 4320
fi

tmp="$(mktemp)"
printf '%-14s %10s %12s %12s %12s\n' resolution pixels floor_ms tracer_ms compute_ms
while [ "$#" -ge 2 ]; do
  W="$1"; H="$2"; shift 2
  N=$((W * H))
  # Interleaved within the size, floor first then tracer then floor again,
  # so a drift across the pair shows up as disagreement between the two
  # floor readings rather than as compute.
  f1=$("$CV" --java-home "$JDK" --gpu --gpu-min-work 1 --Xmx 6g -cp "$CP" \
        GpuTransferFloor "$N" "$ITERS" 2>/dev/null | best)
  t=$("$CV" --java-home "$JDK" --gpu --gpu-min-work 1 --Xmx 6g -cp "$CP" \
        RayTracerKernel "$W" "$H" "$ITERS" 2>/dev/null | best)
  f2=$("$CV" --java-home "$JDK" --gpu --gpu-min-work 1 --Xmx 6g -cp "$CP" \
        GpuTransferFloor "$N" "$ITERS" 2>/dev/null | best)
  f=$(awk -v a="$f1" -v b="$f2" 'BEGIN { printf "%.4f", (a+0 < b+0 ? a : b) }')
  c=$(awk -v t="$t" -v f="$f" 'BEGIN { printf "%.4f", t - f }')
  printf '%-14s %10s %12s %12s %12s\n' "${W}x${H}" "$N" "$f" "$t" "$c"
  echo "$N $f $t" >> "$tmp"
done

echo
awk '{ n[NR]=$1; f[NR]=$2; t[NR]=$3; c++ }
     END {
       if (c < 2) { print "need at least two sizes to fit a line"; exit }
       # Least squares on the FLOOR points: floor = fixed + per_px * n.
       for (i = 1; i <= c; i++) { sx += n[i]; sy += f[i]; sxx += n[i]*n[i]; sxy += n[i]*f[i] }
       slope = (c*sxy - sx*sy) / (c*sxx - sx*sx)
       icpt  = (sy - slope*sx) / c
       printf "floor fit:   fixed = %.4f ms,  transfer = %.4f ns/px\n", icpt, slope*1e6
       sx=sy=sxx=sxy=0
       for (i = 1; i <= c; i++) { sx += n[i]; sy += t[i]; sxx += n[i]*n[i]; sxy += n[i]*t[i] }
       slope = (c*sxy - sx*sy) / (c*sxx - sx*sx)
       icpt  = (sy - slope*sx) / c
       printf "tracer fit:  fixed = %.4f ms,  total    = %.4f ns/px\n", icpt, slope*1e6
     }' "$tmp"
rm -f "$tmp"
