#!/usr/bin/env bash
# Ray-tracer kernel across four execution paths and a resolution sweep.
#
#   HotSpot CPU  |  CratonVM CPU  |  CratonVM --gpu  |  TornadoVM PTX
#
# Produces one row per (path, resolution) with mean/best wall time and the
# frame checksum, plus a FrameDiff verdict of every path against the
# HotSpot reference frame at each resolution. The sweep is the point: a
# fixed per-launch cost shows up as a resolution-dependent speedup, so a
# single resolution cannot tell "our kernel is faster" from "our launch
# overhead is lower".
#
# Two footguns this script exists to remove:
#
#   * TornadoVM silently falls back to a SEQUENTIAL CPU run when its
#     compiler bails out, and still prints a normal-looking result line.
#     The `-g` on its javac is load-bearing (without it the kernel does
#     not compile to PTX at all), and the run is checked for `[Bailout]`.
#   * The `tornado` launcher execs `python3`. A Windows box with only
#     `python.exe` needs a `python3` on PATH; set PYTHON3_DIR to a
#     directory containing one.
#
# Usage:  bench-gpu/run-raytracer-comparison.sh [output.md]
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# Windows-style twins. `java.exe` does not understand an MSYS `/c/...`
# path in -cp: it silently produces no output, which looks exactly like a
# run that legitimately printed no result line.
HERE_W="$(cd "$HERE" && pwd -W)"

JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
GPU_JAR="${GPU_JAR:-C:/craton/gpu-java/target/craton-gpu-0.2.0.jar}"
CV="${CV:-$ROOT/target-gpuray/release/cratonvm-gpuray.exe}"
TORNADO_SETVARS="${TORNADO_SETVARS:-/c/craton/tornadovm/setvars.sh}"
PYTHON3_DIR="${PYTHON3_DIR:-}"
ITERS="${ITERS:-20}"
CPU_ITERS="${CPU_ITERS:-5}"   # the CratonVM CPU path is ~10x HotSpot; fewer reps
RESOLUTIONS="${RESOLUTIONS:-160x120 320x240 640x480 1280x960 1920x1440}"

DUMPS="$HERE/results/dumps"
DUMPS_W="$HERE_W/results/dumps"
mkdir -p "$DUMPS"
CP="$HERE_W;$GPU_JAR"

say() { echo "$@" >&2; }

# Extract `key=value` from a RAYTRACER_RESULT line.
field() { sed -n "s/.*[[:space:]]$2=\\([^[:space:]]*\\).*/\\1/p" <<<"$1"; }

say "== building fixtures =="
"$JDK/bin/javac" -cp "$GPU_JAR" -d "$HERE" "$HERE/RayTracerKernel.java" || exit 1
"$JDK/bin/javac" -d "$HERE" "$HERE/FrameDiff.java" || exit 1

HAVE_TORNADO=0
if [ -f "$TORNADO_SETVARS" ]; then
  # shellcheck disable=SC1090
  source "$TORNADO_SETVARS" >/dev/null 2>&1
  [ -n "$PYTHON3_DIR" ] && export PATH="$PYTHON3_DIR:$PATH"
  if command -v python3 >/dev/null 2>&1; then
    # -g is REQUIRED. Without it javac drops the dead initialising stores
    # for the kernel's `final float` constants, and TornadoVM's compiler
    # then fails to build the task and silently runs it sequentially.
    if "$JAVA_HOME/bin/javac" -g -cp "$TORNADOVM_HOME/share/java/tornado/*" \
         -d "$ROOT/bench-tornado" "$ROOT/bench-tornado/RayTracerTornado.java"; then
      HAVE_TORNADO=1
    else
      say "!! TornadoVM twin failed to compile; skipping that arm"
    fi
  else
    say "!! no python3 on PATH (the tornado launcher needs one); skipping that arm"
    say "   set PYTHON3_DIR=<dir containing python3.exe>"
  fi
else
  say "!! $TORNADO_SETVARS not found; skipping the TornadoVM arm"
fi

OUT="${1:-/dev/stdout}"
{
  echo "| resolution | n | path | mean ms | best ms | checksum | vs HotSpot |"
  echo "|---|---:|---|---:|---:|---:|---|"
} > "$OUT"

for res in $RESOLUTIONS; do
  W="${res%x*}"; H="${res#*x}"; N=$((W * H))
  say "== ${W}x${H} (n=$N) =="

  ref_line=$("$JDK/bin/java" -cp "$CP" RayTracerKernel "$W" "$H" "$ITERS" \
              "$DUMPS_W/hotspot-$res.bin" 2>/dev/null | grep RAYTRACER_RESULT)
  ref_sum=$(field "$ref_line" checksum)
  if [ -z "$ref_sum" ]; then
    say "!! the HotSpot reference run produced no result line at $res — every"
    say "   FrameDiff verdict below would silently be empty. Aborting."
    exit 1
  fi
  printf '| %sx%s | %s | HotSpot CPU | %s | %s | %s | 1x (reference) |\n' \
    "$W" "$H" "$N" "$(field "$ref_line" mean_ms)" "$(field "$ref_line" best_ms)" "$ref_sum" >> "$OUT"

  cpu_line=$("$CV" --java-home "$JDK" -cp "$CP" RayTracerKernel "$W" "$H" "$CPU_ITERS" \
              "$DUMPS_W/craton-cpu-$res.bin" 2>/dev/null | grep RAYTRACER_RESULT)
  gpu_line=$("$CV" --java-home "$JDK" --gpu --gpu-min-work 1 -cp "$CP" RayTracerKernel \
              "$W" "$H" "$ITERS" "$DUMPS_W/craton-gpu-$res.bin" 2>/dev/null | grep RAYTRACER_RESULT)

  for arm in "CratonVM CPU:$cpu_line:craton-cpu" "CratonVM --gpu:$gpu_line:craton-gpu"; do
    label="${arm%%:*}"; rest="${arm#*:}"; line="${rest%:*}"; tag="${rest##*:}"
    verdict=$("$JDK/bin/java" -cp "$HERE_W" FrameDiff "$W" "$H" \
                "$DUMPS_W/hotspot-$res.bin" "$DUMPS_W/$tag-$res.bin" 2>/dev/null \
              | sed -n 's/.*verdict=//p')
    printf '| %sx%s | %s | %s | %s | %s | %s | %s |\n' \
      "$W" "$H" "$N" "$label" "$(field "$line" mean_ms)" "$(field "$line" best_ms)" \
      "$(field "$line" checksum)" "$verdict" >> "$OUT"
  done

  if [ "$HAVE_TORNADO" = 1 ]; then
    t_raw=$(cd "$ROOT" && tornado --classpath bench-tornado RayTracerTornado \
              "$W" "$H" "$ITERS" "$DUMPS_W/tornado-gpu-$res.bin" 2>&1)
    t_line=$(grep RAYTRACER_RESULT <<<"$t_raw")
    if grep -q "Bailout" <<<"$t_raw"; then
      say "!! TornadoVM BAILED OUT at $res: that row ran on the CPU, not the GPU"
      note="BAILED-OUT-TO-CPU"
    else
      note=$("$JDK/bin/java" -cp "$HERE_W" FrameDiff "$W" "$H" \
               "$DUMPS_W/hotspot-$res.bin" "$DUMPS_W/tornado-gpu-$res.bin" 2>/dev/null \
             | sed -n 's/.*verdict=//p')
    fi
    printf '| %sx%s | %s | TornadoVM PTX | %s | %s | %s | %s |\n' \
      "$W" "$H" "$N" "$(field "$t_line" mean_ms)" "$(field "$t_line" best_ms)" \
      "$(field "$t_line" checksum)" "$note" >> "$OUT"
  fi
done

say "== done; wrote $OUT =="
