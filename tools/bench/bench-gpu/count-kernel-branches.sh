#!/usr/bin/env bash
# What a kernel's control flow costs on the device, both arms of the
# if-conversion switch.
#
# The ray tracer record measured 18% of this kernel's SASS as branch
# machinery -- 67 `BRA` plus 32 `BSSY`/`BSYNC`/`BMOV` triples -- coming from
# short-circuit `&&`s and from ternaries the lowerer turned back into
# branches. `BSSY`/`BSYNC` are the reconvergence pair `ptxas` wraps around a
# divergent region; a `selp` needs neither. This counts both, with
# `CRATONVM_GPU_IF_CONVERT` as a same-binary A/B, so the claim is an
# instruction census rather than a wall clock on a host that cannot hold
# still.
#
# Usage: count-kernel-branches.sh <cratonvm.exe> [width height]
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERE_W="$(cd "$HERE" && pwd -W)"
CV="${1:?usage: count-kernel-branches.sh <cratonvm.exe> [width height]}"
W="${2:-640}"
H="${3:-480}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
GPU_JAR="${GPU_JAR:?set GPU_JAR to the craton-gpu annotations jar}"
CUDA_BIN="${CUDA_BIN:-/c/Program Files/NVIDIA GPU Computing Toolkit/CUDA/v13.3/bin}"
CP="$HERE_W;$GPU_JAR"

"$JDK/bin/javac" -cp "$GPU_JAR" -d "$HERE" "$HERE/RayTracerKernel.java" || exit 1

# `$2` is the weighted arm-pair budget: 0 is the shipped default (the
# transform off), 8 is what `CRATONVM_GPU_IF_CONVERT=1` selects, and a large
# number converts every diamond the shape and purity tests admit.
report() {
  local label="$1" budget="$2"
  local dir; dir="$(mktemp -d)"
  local dir_w; dir_w="$(cd "$dir" && pwd -W)"
  CRATONVM_GPU_IF_CONVERT_MAX_OPS="$budget" CRATONVM_GPU_DUMP_PTX="$dir_w" \
    "$CV" --java-home "$JDK" --gpu --gpu-min-work 1 -cp "$CP" \
    RayTracerKernel "$W" "$H" 2 >/dev/null 2>&1
  local ptx; ptx="$(ls "$dir"/*render*.ptx "$dir"/*.ptx 2>/dev/null | head -1)"
  if [ -z "$ptx" ]; then
    echo "$label: no PTX dumped into $dir"
    return
  fi
  "$CUDA_BIN/ptxas" -arch=sm_75 -O3 -o "$dir/k.cubin" "$ptx" || return
  local sass; sass="$("$CUDA_BIN/nvdisasm" -c "$dir/k.cubin" 2>/dev/null)"
  awk -v label="$label" -v ptx="$(cat "$ptx")" -v sass="$sass" 'BEGIN {
      n = split(ptx, pl, "\n"); ptx_ins = 0; selp = 0; bra = 0
      for (i = 1; i <= n; i++) {
        t = pl[i]; gsub(/^[ \t]+|[ \t]+$/, "", t)
        if (t == "" || t ~ /^\/\// || t ~ /^\./ || t ~ /:$/ || t ~ /^[{}]/) continue
        ptx_ins++
        if (t ~ /selp\./) selp++
        if (t ~ /bra /) bra++
      }
      m = split(sass, sl, "\n"); sass_ins = 0; sbra = 0; sync = 0
      for (i = 1; i <= m; i++) {
        t = sl[i]
        if (t !~ /\/\*[0-9a-f]+\*\//) continue
        sass_ins++
        if (t ~ /[ @]BRA/) sbra++
        if (t ~ /BSSY|BSYNC|BMOV/) sync++
      }
      printf "%-12s PTX: %5d instr, %3d selp, %3d bra   SASS: %5d instr, %3d BRA, %3d BSSY/BSYNC/BMOV (%.1f%% branch machinery)\n",
             label, ptx_ins, selp, bra, sass_ins, sbra, sync,
             sass_ins ? 100.0 * (sbra + sync) / sass_ins : 0
  }'
  rm -rf "$dir"
}

echo "RayTracerKernel.render at ${W}x${H}"
report "budget=0 (default)" 0
report "budget=8" 8
report "budget=unbounded" 10000
