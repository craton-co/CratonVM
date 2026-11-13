#!/usr/bin/env bash
# Warm-timing GPU comparison: CratonVM-GPU vs TornadoVM-GPU vs CPU baselines.
# All GPU timings are WARM (PTX compiled + buffers allocated in a prior call)
# and include the full per-call H2D + kernel + D2H round-trip.
# Kernel: GpuWarm.heavy — 96 int multiply-adds per element (same chain as
# GpuCompute.heavy / TornadoGpuCompute.heavy).
# Usage: bash run-gpu-warm.sh [output.md]
#
# Optional feature-gated benches (off by default so this suite keeps passing
# before the underlying CratonVM features land — see
# gpu-offload-followups-20260711.md):
#   BENCH_LDC=1  — also run GpuLdcBench (bench-gpu/GpuLdcBench.java), the same
#                  96-MAD chain as GpuWarm.heavy but with multiplier/addend
#                  constants outside sipush range (forces `ldc`). Exercises
#                  the ldc-constants feature (item 6 in the doc above); until
#                  ldc support lands, the analyzer rejects it and --gpu just
#                  re-measures the CPU fallback. No TornadoVM twin (CPU-only
#                  columns).
#   BENCH_DOT=1  — GpuDotBench/TornadoDotBench (reduction-dispatch feature) is
#                  wired into run-gpu-comparison.sh, not this script; set it
#                  there instead.
#   Usage: BENCH_LDC=1 bash run-gpu-warm.sh [output.md]
set +e
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

ROOT="${ROOT:-C:/craton/CratonVM}"
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
CV_GPU="${CV_GPU:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
HS="$JDK/bin/java.exe"
TVBASE="C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx"
TVM="C:/craton/tornadovm/jdk-25.0.3/bin/java.exe"
ARGFILE="$TVBASE/tornado-argfile"
GO="${GO:-$ROOT/bench-gpu}"
OUT="${1:-$ROOT/bench-gpu/results/warm-comparison-$(date +%Y%m%d-%H%M%S).md}"

SIZES=( 4194304 16777216 67108864 )   # 2^22 2^24 2^26
declare -A EXPOF=( [4194304]=22 [16777216]=24 [67108864]=26 )
REPS=5
TIMEOUT_S=300
BENCH_LDC="${BENCH_LDC:-0}"

extract() { echo "$2" | grep -oE "${1}=[^[:space:]]+" | head -1 | sed "s/${1}=//"; }

declare -A R S
for n in "${SIZES[@]}"; do
  echo "== n=$n =="
  # CratonVM CPU (plain build, no --gpu), warm best-of
  out=$(timeout $TIMEOUT_S "$CV" --java-home "$JDK" --Xmx 8g -cp "$GO" GpuWarm f "$n" $REPS 2>/dev/null)
  R[cvcpu,$n]=$(extract warm_ms "$out"); S[cvcpu,$n]=$(extract SAMPLE "$out")
  echo "  cv-cpu   warm_ms=${R[cvcpu,$n]} sample=${S[cvcpu,$n]}"
  # CratonVM GPU (gpu-driver build, --gpu), warm best-of
  out=$(timeout $TIMEOUT_S "$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" GpuWarm f "$n" $REPS 2>/dev/null)
  R[cvgpu,$n]=$(extract warm_ms "$out"); S[cvgpu,$n]=$(extract SAMPLE "$out")
  echo "  cv-gpu   warm_ms=${R[cvgpu,$n]} sample=${S[cvgpu,$n]}"
  # HotSpot CPU (same class), warm best-of
  out=$(timeout $TIMEOUT_S "$HS" -Xmx8g -cp "$GO" GpuWarm f "$n" $REPS 2>/dev/null)
  R[hs,$n]=$(extract warm_ms "$out"); S[hs,$n]=$(extract SAMPLE "$out")
  echo "  hotspot  warm_ms=${R[hs,$n]} sample=${S[hs,$n]}"
  # TornadoVM GPU (warm single-shot, full H2D+kernel+D2H)
  out=$(timeout $TIMEOUT_S "$TVM" "@$ARGFILE" -Xmx8g \
        --patch-module tornado.examples="$GO" \
        -m tornado.examples/uk.ac.manchester.tornado.examples.TornadoGpuCompute "$n" 2>/dev/null)
  R[tvm,$n]=$(extract heavy_ms "$out"); S[tvm,$n]=$(extract COMPUTE_CHECKSUM "$out")
  echo "  tornado  heavy_ms=${R[tvm,$n]} checksum=${S[tvm,$n]}"
done

# ── GpuLdcBench (optional, BENCH_LDC=1) ─────────────────────────────────────────
# ldc-constants bench: same 96-MAD shape as GpuWarm.heavy, but constants
# outside sipush range make it analyzer-ineligible until ldc/ldc_w support
# lands (gpu-offload-followups-20260711.md item 6), so
# gated off by default — --gpu just re-measures the CPU fallback until then.
# No TornadoVM twin for this one (CPU-only columns).
if [ "$BENCH_LDC" = "1" ]; then
  for n in "${SIZES[@]}"; do
    echo "== ldc n=$n (BENCH_LDC=1) =="
    out=$(timeout $TIMEOUT_S "$CV" --java-home "$JDK" --Xmx 8g -cp "$GO" GpuLdcBench "$n" $REPS 2>/dev/null)
    R[ldc_cvcpu,$n]=$(extract warm_ms "$out"); S[ldc_cvcpu,$n]=$(extract SAMPLE "$out")
    echo "  cv-cpu   warm_ms=${R[ldc_cvcpu,$n]} sample=${S[ldc_cvcpu,$n]}"
    out=$(timeout $TIMEOUT_S "$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" GpuLdcBench "$n" $REPS 2>/dev/null)
    R[ldc_cvgpu,$n]=$(extract warm_ms "$out"); S[ldc_cvgpu,$n]=$(extract SAMPLE "$out")
    echo "  cv-gpu   warm_ms=${R[ldc_cvgpu,$n]} sample=${S[ldc_cvgpu,$n]}"
    out=$(timeout $TIMEOUT_S "$HS" -Xmx8g -cp "$GO" GpuLdcBench "$n" $REPS 2>/dev/null)
    R[ldc_hs,$n]=$(extract warm_ms "$out"); S[ldc_hs,$n]=$(extract SAMPLE "$out")
    echo "  hotspot  warm_ms=${R[ldc_hs,$n]} sample=${S[ldc_hs,$n]}"
  done
fi

mkdir -p "$(dirname "$OUT")"
{
  echo "# Warm GPU comparison — GpuWarm.heavy (96 int MADs/element)"
  echo
  echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
  echo "| N | CratonVM CPU | HotSpot CPU | CratonVM GPU | TornadoVM GPU | CV-GPU vs CV-CPU | CV-GPU vs HotSpot |"
  echo "|---|---|---|---|---|---|---|"
  for n in "${SIZES[@]}"; do
    a=${R[cvcpu,$n]}; h=${R[hs,$n]}; g=${R[cvgpu,$n]}; t=${R[tvm,$n]}
    s1="-"; s2="-"
    [[ "$a" =~ ^[0-9]+$ && "$g" =~ ^[0-9]+$ && "$g" -gt 0 ]] && s1=$(awk "BEGIN{printf \"%.0f×\", $a/$g}")
    [[ "$h" =~ ^[0-9]+$ && "$g" =~ ^[0-9]+$ && "$g" -gt 0 ]] && s2=$(awk "BEGIN{printf \"%.1f×\", $h/$g}")
    echo "| 2^${EXPOF[$n]} | ${a:--}ms | ${h:--}ms | ${g:--}ms | ${t:--}ms | $s1 | $s2 |"
  done
  echo
  echo "Samples (CratonVM/HotSpot must match; Tornado prints full checksum):"
  for n in "${SIZES[@]}"; do
    echo "- n=$n: cv-cpu=${S[cvcpu,$n]:--} cv-gpu=${S[cvgpu,$n]:--} hotspot=${S[hs,$n]:--}"
  done
  if [ "$BENCH_LDC" = "1" ]; then
    echo
    echo "## GpuLdcBench — ldc-constants (BENCH_LDC=1)"
    echo
    echo "| N | CratonVM CPU | HotSpot CPU | CratonVM GPU | CV-GPU vs CV-CPU | CV-GPU vs HotSpot |"
    echo "|---|---|---|---|---|---|"
    for n in "${SIZES[@]}"; do
      a=${R[ldc_cvcpu,$n]}; h=${R[ldc_hs,$n]}; g=${R[ldc_cvgpu,$n]}
      s1="-"; s2="-"
      [[ "$a" =~ ^[0-9]+$ && "$g" =~ ^[0-9]+$ && "$g" -gt 0 ]] && s1=$(awk "BEGIN{printf \"%.0f×\", $a/$g}")
      [[ "$h" =~ ^[0-9]+$ && "$g" =~ ^[0-9]+$ && "$g" -gt 0 ]] && s2=$(awk "BEGIN{printf \"%.1f×\", $h/$g}")
      echo "| 2^${EXPOF[$n]} | ${a:--}ms | ${h:--}ms | ${g:--}ms | $s1 | $s2 |"
    done
    echo
    echo "Samples (CratonVM/HotSpot must match):"
    for n in "${SIZES[@]}"; do
      echo "- n=$n: cv-cpu=${S[ldc_cvcpu,$n]:--} cv-gpu=${S[ldc_cvgpu,$n]:--} hotspot=${S[ldc_hs,$n]:--}"
    done
  fi
} > "$OUT"
echo "Written: $OUT"
