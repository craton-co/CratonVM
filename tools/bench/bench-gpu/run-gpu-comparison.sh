#!/usr/bin/env bash
# GPU offload head-to-head: CratonVM CPU, CratonVM GPU, HotSpot CPU, TornadoVM GPU.
# Benchmarks: GpuCompute.heavy (compute-heavy) and GpuProbe.vaddMap (memory-bound).
# Sizes: 2^20 (1M), 2^22 (4M), 2^24 (16M), 2^26 (64M), 2^28 (256M) — 2^28 was crashing before.
# Usage: bash run-gpu-comparison.sh [output.md]
#
# Optional feature-gated benches (off by default so this suite keeps passing
# before the underlying CratonVM features land — see
# gpu-offload-followups-20260711.md):
#   BENCH_DOT=1  — also run GpuDotBench.dotReduce (bench-gpu/GpuDotBench.java)
#                  vs its TornadoVM @Reduce twin (bench-tornado/TornadoDotBench.java).
#                  Exercises the reduction-dispatch feature (item 1 in the doc
#                  above); until the dispatch-side result-readback lands,
#                  --gpu just re-measures the CPU fallback for this one.
#   BENCH_LDC=1  — GpuLdcBench (ldc-constants feature) is wired into
#                  run-gpu-warm.sh, not this script; set it there instead.
#   Usage: BENCH_DOT=1 bash run-gpu-comparison.sh [output.md]
set +e
set +o pipefail
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

# ── paths ──────────────────────────────────────────────────────────────────────
ROOT="${ROOT:-C:/craton/CratonVM}"
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
CV_GPU="${CV_GPU:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
HS="$JDK/bin/java.exe"
TVBASE="C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx"
TVM="C:/craton/tornadovm/jdk-25.0.3/bin/java.exe"
ARGFILE="$TVBASE/tornado-argfile"
TVJARS="$TVBASE/share/java/tornado"
TVCP="$TVJARS/tornado-api-4.0.1-jdk25.jar"
GO="${GO:-$ROOT/bench-gpu}"
TVSRC="${TVSRC:-$ROOT/bench-tornado}"
OUT="${1:-$ROOT/bench-gpu/results/gpu-comparison-$(date +%Y%m%d-%H%M%S).md}"

[ -x "$CV" ]     || { echo "ERROR: missing CV=$CV";     exit 1; }
[ -x "$CV_GPU" ] || { echo "ERROR: missing CV_GPU=$CV_GPU"; exit 1; }
[ -f "$ARGFILE" ] || { echo "ERROR: missing $ARGFILE"; exit 1; }

SIZES=( 1048576 4194304 16777216 67108864 268435456 )   # 2^20 2^22 2^24 2^26 2^28
declare -A EXPOF=( [1048576]=20 [4194304]=22 [16777216]=24 [67108864]=26 [268435456]=28 )
TIMEOUT_S=180

# ── compile TornadoVM variants ─────────────────────────────────────────────────
echo "[build] compiling TornadoGpuCompute + TornadoVadd ..."
"$TVM" -version 2>&1 | head -1
# -g required: TornadoVM's PTX compiler reads LocalVariableTable for param names
"C:/craton/tornadovm/jdk-25.0.3/bin/javac.exe" -g \
  --module-path "$TVJARS" \
  --add-modules tornado.annotation,tornado.api \
  --patch-module tornado.examples="$GO" \
  -d "$GO" \
  "$TVSRC/TornadoGpuCompute.java" "$TVSRC/TornadoVadd.java" 2>&1 \
  && echo "[build] OK" || { echo "[build] FAILED — TornadoVM variants unavailable"; TORNADO_OK=0; }
TORNADO_OK="${TORNADO_OK:-1}"

# ── optional: compile TornadoDotBench (BENCH_DOT=1 only) ───────────────────────
BENCH_DOT="${BENCH_DOT:-0}"
DOT_TORNADO_OK=0
if [ "$BENCH_DOT" = "1" ] && [ "$TORNADO_OK" = "1" ]; then
  echo "[build] compiling TornadoDotBench (BENCH_DOT=1) ..."
  "C:/craton/tornadovm/jdk-25.0.3/bin/javac.exe" -g \
    --module-path "$TVJARS" \
    --add-modules tornado.annotation,tornado.api \
    --patch-module tornado.examples="$GO" \
    -d "$GO" \
    "$TVSRC/TornadoDotBench.java" 2>&1 \
    && { echo "[build] TornadoDotBench OK"; DOT_TORNADO_OK=1; } \
    || echo "[build] TornadoDotBench FAILED — dot bench will skip the tornadovm column"
fi

# ── runner helpers ─────────────────────────────────────────────────────────────
run_cv_cpu() {   # $1=class $2=n
  timeout "$TIMEOUT_S" "$CV" --java-home "$JDK" --Xmx 8g -cp "$GO" "$1" "$2" 2>/dev/null
}
run_cv_gpu() {
  timeout "$TIMEOUT_S" "$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" "$1" "$2" 2>/dev/null
}
run_hs() {
  timeout "$TIMEOUT_S" "$HS" -Xmx8g -cp "$GO" "$1" "$2" 2>/dev/null
}
run_tvm() {   # $1=class $2=n  (note: $1 is "Tornado${class}" from bench_run)
  [ "$TORNADO_OK" = "1" ] || { echo "SKIP"; return; }
  # Classes are in the tornado.examples named module (patch-module); must use -m, not -cp
  local pkg="uk.ac.manchester.tornado.examples"
  timeout "$TIMEOUT_S" "$TVM" "@$ARGFILE" -Xmx8g \
    --patch-module tornado.examples="$GO" \
    -m tornado.examples/${pkg}."$1" "$2" 2>/dev/null
}

extract() {   # $1=key $2=output
  echo "$2" | grep -oE "${1}=[^[:space:]]+" | head -1 | sed "s/${1}=//"
}

# ── run one benchmark variant ──────────────────────────────────────────────────
# Columns: n | cv-cpu | cv-gpu | hotspot | tornadovm | checksum-match
declare -A RES   # RES[bench,n,variant]=time_ms
declare -A SUMS  # SUMS[bench,n,variant]=checksum

bench_run() {
  local bench="$1" class="$2" time_key="$3" sum_key="$4" n="$5"
  echo -n "  ${bench} n=$(numfmt --to=si --suffix='' $n 2>/dev/null || echo $n)"
  for vm in cv_cpu cv_gpu hotspot tornadovm; do
    local out
    case $vm in
      cv_cpu)   out=$(run_cv_cpu  "$class" "$n") ;;
      cv_gpu)   out=$(run_cv_gpu  "$class" "$n") ;;
      hotspot)  out=$(run_hs      "$class" "$n") ;;
      tornadovm) out=$(run_tvm    "Tornado${class}" "$n") ;;
    esac
    local ms=$(extract "$time_key" "$out")
    local cs=$(extract "$sum_key"  "$out")
    ms="${ms:-FAIL}"
    cs="${cs:--}"
    RES["$bench,$n,$vm"]="$ms"
    SUMS["$bench,$n,$vm"]="$cs"
    echo -n "  $vm=${ms}ms"
  done
  # checksum agreement
  local ref="${SUMS[$bench,$n,hotspot]}"
  local agree="✓"
  for vm in cv_cpu cv_gpu tornadovm; do
    local s="${SUMS[$bench,$n,$vm]}"
    [ "$s" != "$ref" ] && [ "$s" != "-" ] && { agree="✗"; break; }
  done
  RES["$bench,$n,agree"]="$agree"
  echo "  $agree"
}

# ── GpuCompute.heavy ───────────────────────────────────────────────────────────
echo; echo "=== GpuCompute.heavy (96×multiply-add per element) ==="
for n in "${SIZES[@]}"; do
  bench_run heavy GpuCompute heavy_ms COMPUTE_CHECKSUM "$n"
done

# ── GpuProbe.vaddMap ──────────────────────────────────────────────────────────
# For vaddMap, we need a standalone class. GpuProbe runs all three kernels at once.
# Use TornadoVadd for TornadoVM. For others, we extract MAP_CHECKSUM from GpuProbe.
# But GpuProbe doesn't print vadd_ms separately — reuse GpuCompute-style approach
# with a dedicated VaddOnly class (or use MAP_CHECKSUM from GpuProbe output).
echo; echo "=== GpuProbe.vaddMap (out[i]=a[i]+b[i], memory-bound) ==="
bench_vadd() {
  local n="$1"
  echo -n "  vadd n=$(numfmt --to=si --suffix='' $n 2>/dev/null || echo $n)"
  # CratonVM: run GpuProbe and time it end-to-end (includes all 3 kernels)
  # Use the map_checksum as the correctness signal; ms is total wall time
  for vm in cv_cpu cv_gpu hotspot; do
    local out
    case $vm in
      cv_cpu)  out=$(run_cv_cpu  GpuProbe "$n") ;;
      cv_gpu)  out=$(run_cv_gpu  GpuProbe "$n") ;;
      hotspot) out=$(run_hs      GpuProbe "$n") ;;
    esac
    local cs=$(extract MAP_CHECKSUM "$out")
    RES["vadd,$n,$vm"]="${cs:--}"
    SUMS["vadd,$n,$vm"]="${cs:--}"
    echo -n "  $vm=CS:${cs:-FAIL}"
  done
  # TornadoVM: run TornadoVadd which does ONLY vaddMap
  if [ "$TORNADO_OK" = "1" ]; then
    local out=$(run_tvm TornadoVadd "$n")
    local ms=$(extract vadd_ms "$out")
    local cs=$(extract MAP_CHECKSUM "$out")
    RES["vadd,$n,tornadovm"]="${ms:-FAIL}ms"
    SUMS["vadd,$n,tornadovm"]="${cs:--}"
    echo -n "  tornadovm=${ms:-FAIL}ms CS:${cs:-FAIL}"
  fi
  # checksum agreement (compare all vs hotspot)
  local ref="${SUMS[vadd,$n,hotspot]}"
  local agree="✓"
  for vm in cv_cpu cv_gpu tornadovm; do
    local s="${SUMS[vadd,$n,$vm]}"
    [ "$s" != "$ref" ] && [ "$s" != "-" ] && { agree="✗"; break; }
  done
  echo "  $agree"
}
for n in "${SIZES[@]}"; do
  bench_vadd "$n"
done

# ── GpuDotBench.dotReduce (optional, BENCH_DOT=1) ───────────────────────────────
# Reduction-dispatch bench: dotReduce is analyzer-eligible (is_reduction:true)
# but --gpu transparent dispatch currently falls through to CPU for non-void
# kernels (gpu-offload-followups-20260711.md item 1), so
# gated off by default — running it before that lands just re-measures the CPU
# fallback, which is harmless but not informative for a default suite run.
bench_dot() {
  local n="$1"
  echo -n "  dot n=$(numfmt --to=si --suffix='' $n 2>/dev/null || echo $n)"
  for vm in cv_cpu cv_gpu hotspot; do
    local out
    case $vm in
      cv_cpu)  out=$(run_cv_cpu  GpuDotBench "$n") ;;
      cv_gpu)  out=$(run_cv_gpu  GpuDotBench "$n") ;;
      hotspot) out=$(run_hs      GpuDotBench "$n") ;;
    esac
    local ms=$(extract dot_ms "$out")
    local cs=$(extract DOT_CHECKSUM "$out")
    RES["dot,$n,$vm"]="${ms:-FAIL}"
    SUMS["dot,$n,$vm"]="${cs:--}"
    echo -n "  $vm=${ms:-FAIL}ms CS:${cs:-FAIL}"
  done
  if [ "$DOT_TORNADO_OK" = "1" ]; then
    local out=$(run_tvm TornadoDotBench "$n")
    local ms=$(extract dot_ms "$out")
    local cs=$(extract DOT_CHECKSUM "$out")
    RES["dot,$n,tornadovm"]="${ms:-FAIL}"
    SUMS["dot,$n,tornadovm"]="${cs:--}"
    echo -n "  tornadovm=${ms:-FAIL}ms CS:${cs:-FAIL}"
  fi
  local ref="${SUMS[dot,$n,hotspot]}"
  local agree="✓"
  for vm in cv_cpu cv_gpu tornadovm; do
    local s="${SUMS[dot,$n,$vm]}"
    [ "$s" != "$ref" ] && [ "$s" != "-" ] && { agree="✗"; break; }
  done
  RES["dot,$n,agree"]="$agree"
  echo "  $agree"
}
if [ "$BENCH_DOT" = "1" ]; then
  echo; echo "=== GpuDotBench.dotReduce (reduction dispatch, BENCH_DOT=1) ==="
  for n in "${SIZES[@]}"; do
    bench_dot "$n"
  done
fi

# ── print table ───────────────────────────────────────────────────────────────
echo
echo "=== RESULTS TABLE ==="
printf "%-20s %-8s %-14s %-14s %-14s %-14s %-6s\n" \
  "benchmark" "n" "CratonVM-CPU" "CratonVM-GPU" "HotSpot-CPU" "TornadoVM-GPU" "OK"
printf "%-20s %-8s %-14s %-14s %-14s %-14s %-6s\n" \
  "--------------------" "--------" "--------------" "--------------" "--------------" "--------------" "------"

for n in "${SIZES[@]}"; do
  for bench in heavy; do
    nfmt=$(numfmt --to=si --suffix='' "$n" 2>/dev/null || echo "$n")
    printf "%-20s %-8s %-14s %-14s %-14s %-14s %-6s\n" \
      "$bench" "${nfmt}" \
      "${RES[$bench,$n,cv_cpu]:--}ms" \
      "${RES[$bench,$n,cv_gpu]:--}ms" \
      "${RES[$bench,$n,hotspot]:--}ms" \
      "${RES[$bench,$n,tornadovm]:--}ms" \
      "${RES[$bench,$n,agree]:--}"
  done
done

for n in "${SIZES[@]}"; do
  nfmt=$(numfmt --to=si --suffix='' "$n" 2>/dev/null || echo "$n")
  cv_cs="${SUMS[vadd,$n,cv_cpu]}"
  printf "%-20s %-8s %-14s %-14s %-14s %-14s %-6s\n" \
    "vadd(checksum)" "${nfmt}" \
    "${cv_cs:--}" \
    "${SUMS[vadd,$n,cv_gpu]:--}" \
    "${SUMS[vadd,$n,hotspot]:--}" \
    "${SUMS[vadd,$n,tornadovm]:--}" \
    "${RES[vadd,$n,agree]:--}"
done

# ── write markdown ────────────────────────────────────────────────────────────
mkdir -p "$(dirname "$OUT")"
{
  echo "# GPU Offload Comparison — CratonVM vs TornadoVM"
  echo; echo "**Date:** $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "**CratonVM:** \`$CV\`"
  echo "**CratonVM-GPU:** \`$CV_GPU\`"
  echo "**TornadoVM:** \`$TVM\` (PTX/RTX 2060)"
  echo "**HotSpot:** \`$HS\`"
  echo
  echo "## GpuCompute.heavy — 96× multiply-add per element (compute-bound)"
  echo
  echo "| N | CratonVM CPU | CratonVM GPU | HotSpot CPU | TornadoVM GPU | CV-GPU speedup | TVM speedup |"
  echo "|---|---|---|---|---|---|---|"
  for n in "${SIZES[@]}"; do
    nfmt=$(numfmt --to=si --suffix='' "$n" 2>/dev/null || printf "%d" "$n")
    cv_cpu="${RES[heavy,$n,cv_cpu]}"
    cv_gpu="${RES[heavy,$n,cv_gpu]}"
    hs="${RES[heavy,$n,hotspot]}"
    tvm="${RES[heavy,$n,tornadovm]}"
    agree="${RES[heavy,$n,agree]}"
    # speedup vs hotspot
    spd_cv_gpu="-"
    spd_tvm="-"
    if [[ "$cv_gpu" =~ ^[0-9]+$ ]] && [[ "$hs" =~ ^[0-9]+$ ]] && [ "$hs" -gt 0 ]; then
      spd_cv_gpu=$(awk "BEGIN{printf \"%.1f×\", $hs/$cv_gpu}")
    fi
    if [[ "$tvm" =~ ^[0-9]+$ ]] && [[ "$hs" =~ ^[0-9]+$ ]] && [ "$hs" -gt 0 ]; then
      spd_tvm=$(awk "BEGIN{printf \"%.1f×\", $hs/$tvm}")
    fi
    echo "| 2^${EXPOF[$n]} ($nfmt) | ${cv_cpu:--}ms | ${cv_gpu:--}ms | ${hs:--}ms | ${tvm:--}ms | $spd_cv_gpu | $spd_tvm | $agree |"
  done
  echo
  echo "## GpuProbe.vaddMap — out\[i\]=a\[i\]+b\[i\] (memory-bound, checksums)"
  echo
  echo "| N | CratonVM (checksum) | HotSpot (checksum) | TornadoVM GPU |"
  echo "|---|---|---|---|"
  for n in "${SIZES[@]}"; do
    nfmt=$(numfmt --to=si --suffix='' "$n" 2>/dev/null || printf "%d" "$n")
    # RES[vadd,n,tornadovm] already contains "${ms}ms" from bench_vadd
    echo "| 2^${EXPOF[$n]} ($nfmt) | CS:${SUMS[vadd,$n,cv_cpu]:--} | CS:${SUMS[vadd,$n,hotspot]:--} | ${RES[vadd,$n,tornadovm]:--} CS:${SUMS[vadd,$n,tornadovm]:--} |"
  done
  if [ "$BENCH_DOT" = "1" ]; then
    echo
    echo "## GpuDotBench.dotReduce — reduction dispatch (BENCH_DOT=1)"
    echo
    echo "| N | CratonVM CPU | CratonVM GPU | HotSpot CPU | TornadoVM GPU | OK |"
    echo "|---|---|---|---|---|---|"
    for n in "${SIZES[@]}"; do
      nfmt=$(numfmt --to=si --suffix='' "$n" 2>/dev/null || printf "%d" "$n")
      echo "| 2^${EXPOF[$n]} ($nfmt) | ${RES[dot,$n,cv_cpu]:--}ms | ${RES[dot,$n,cv_gpu]:--}ms | ${RES[dot,$n,hotspot]:--}ms | ${RES[dot,$n,tornadovm]:--}ms | ${RES[dot,$n,agree]:--} |"
    done
  fi
  echo
  echo "## Notes"
  echo
  echo "- CratonVM GPU: automatic offload via \`--gpu\` flag (analyzes bytecode at first call)"
  echo "- TornadoVM GPU: explicit \`@Parallel\` annotation + TaskGraph API (warmup call before measurement)"
  echo "- All timings include H2D + kernel + D2H (full round-trip)"
  echo "- TornadoVM heavy timing: warm (after PTX compilation in warmup call)"
  echo "- CratonVM heavy timing: first call (includes PTX compilation + buffer alloc)"
  echo "- N=2^28 included to verify if crash is fixed in current build"
} > "$OUT"

echo; echo "Written: $OUT"
