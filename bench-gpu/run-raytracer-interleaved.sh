#!/usr/bin/env bash
# Interleaved CratonVM-GPU vs TornadoVM A/B at one resolution, with a
# per-round HotSpot control.
#
# Why this exists rather than just reading run-raytracer-comparison.sh's
# table: this box carries a variable background CPU load, and both GPU
# paths spend real host time marshalling and launching. A sweep that runs
# all of arm A and then all of arm B attributes any drift between them to
# the arms. Two runs of the same pair 40 minutes apart on this host
# disagreed about which arm was faster at 1920x1440.
#
# So: alternate the arms within a round, and run a HotSpot CPU render each
# round as an absolute contention detector. HotSpot's best-of on an idle
# box is known (CONTROL_BASELINE_MS); a round whose control exceeds
# CONTROL_TOLERANCE x that is measuring the load, not the arms, and is
# reported as CONTENDED so it can be discarded rather than averaged in.
#
# Usage: bench-gpu/run-raytracer-interleaved.sh [width] [height] [rounds]
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
HERE_W="$(cd "$HERE" && pwd -W)"

JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
GPU_JAR="${GPU_JAR:-C:/craton/gpu-java/target/craton-gpu-0.2.0.jar}"
CV="${CV:-$ROOT/target-gpuray/release/cratonvm-gpuray.exe}"
TORNADO_SETVARS="${TORNADO_SETVARS:-/c/craton/tornadovm/setvars.sh}"
PYTHON3_DIR="${PYTHON3_DIR:-}"

W="${1:-1920}"
H="${2:-1440}"
ROUNDS="${3:-5}"
ITERS="${ITERS:-30}"
# Past 8K the frame itself stops fitting in a default heap: 11520x6480 is
# 74.6M pixels, an `int[]` of 298 MB, and all three arms allocate one. Left
# empty below 8K so every previously published row is byte-for-byte the same
# command that produced it.
XMX="${XMX:-}"
CV_HEAP=(); HS_HEAP=(); TORNADO_HEAP=()
if [ -n "$XMX" ]; then
  CV_HEAP=(--Xmx "$XMX"); HS_HEAP=("-Xmx$XMX"); TORNADO_HEAP=("--jvm" "-Xmx$XMX")
fi
# HotSpot best-of at 640x480 on this box with nothing else running. Scaled
# by pixel count for other resolutions; the kernel is very close to linear
# in n on the CPU, so this is a good enough contention yardstick.
CONTROL_BASELINE_MS="${CONTROL_BASELINE_MS:-8.1}"
CONTROL_TOLERANCE="${CONTROL_TOLERANCE:-1.25}"

CP="$HERE_W;$GPU_JAR"
best() { sed -n 's/.*[[:space:]]best_ms=\([^[:space:]]*\).*/\1/p'; }

"$JDK/bin/javac" -cp "$GPU_JAR" -d "$HERE" "$HERE/RayTracerKernel.java" || exit 1

# shellcheck disable=SC1090
source "$TORNADO_SETVARS" >/dev/null 2>&1
[ -n "$PYTHON3_DIR" ] && export PATH="$PYTHON3_DIR:$PATH"
"$JAVA_HOME/bin/javac" -g -cp "$TORNADOVM_HOME/share/java/tornado/*" \
  -d "$ROOT/bench-tornado" "$ROOT/bench-tornado/RayTracerTornado.java" || exit 1

limit=$(awk -v b="$CONTROL_BASELINE_MS" -v t="$CONTROL_TOLERANCE" \
            -v n="$((W * H))" 'BEGIN { printf "%.3f", b * t * n / 307200 }')
echo "resolution ${W}x${H}, $ROUNDS rounds of $ITERS iterations"
echo "control limit: HotSpot best above ${limit} ms means the host CPU is loaded"
echo
tmp="$(mktemp)"
printf '%-6s %12s %12s %12s   %s\n' round control craton-gpu tornado verdict

for r in $(seq 1 "$ROUNDS"); do
  ctl=$("$JDK/bin/java" "${HS_HEAP[@]}" -cp "$CP" RayTracerKernel "$W" "$H" 5 2>/dev/null | best)
  # Alternate which arm goes first so a systematic warm/cool drift within
  # a round cannot favour one of them.
  if [ $((r % 2)) -eq 1 ]; then
    a=$("$CV" --java-home "$JDK" --gpu --gpu-min-work 1 "${CV_HEAP[@]}" -cp "$CP" \
          RayTracerKernel "$W" "$H" "$ITERS" 2>/dev/null | best)
    b=$(cd "$ROOT" && tornado "${TORNADO_HEAP[@]}" --classpath bench-tornado RayTracerTornado \
          "$W" "$H" "$ITERS" 2>/dev/null | best)
  else
    b=$(cd "$ROOT" && tornado "${TORNADO_HEAP[@]}" --classpath bench-tornado RayTracerTornado \
          "$W" "$H" "$ITERS" 2>/dev/null | best)
    a=$("$CV" --java-home "$JDK" --gpu --gpu-min-work 1 "${CV_HEAP[@]}" -cp "$CP" \
          RayTracerKernel "$W" "$H" "$ITERS" 2>/dev/null | best)
  fi
  verdict=$(awk -v c="$ctl" -v l="$limit" -v x="$a" -v y="$b" 'BEGIN {
      if (x+0 < y+0) printf "craton %.3fx", y/x; else printf "tornado %.3fx", x/y
      if (c+0 > l+0) printf "   [host CPU loaded]"
      print ""
  }')
  printf '%-6s %12s %12s %12s   %s\n' "$r" "$ctl" "$a" "$b" "$verdict"
  echo "$a $b" >> "$tmp"
done

echo
awk '{ ca += $1; to += $2; n++; if ($1 < $2) wins++ }
     END {
       printf "paired mean over %d rounds: craton %.4f ms, tornado %.4f ms", n, ca/n, to/n
       printf " -- craton faster in %d/%d rounds, ", wins+0, n
       if (ca < to) printf "by %.1f%% on the mean\n", 100*(to-ca)/to
       else printf "tornado ahead by %.1f%% on the mean\n", 100*(ca-to)/ca
     }' "$tmp"
echo
echo "A '[host CPU loaded]' round is still a valid PAIR: both arms ran under the"
echo "same conditions, alternating order, and both are GPU-bound. The control is a"
echo "CPU yardstick, so it flags load the GPU arms are largely insensitive to --"
echo "read it as 'do not compare this number to a run from another day', not as"
echo "'discard the comparison within this round'."
rm -f "$tmp"
