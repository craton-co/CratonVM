#!/usr/bin/env bash
# 4-way vector-add benchmark across:
#   1. HotSpot C2          — Oracle JDK 25 (CPU JIT'd reference)
#   2. CratonVM CPU         — cratonvm, no --gpu (JIT enabled when stable, off when not)
#   3. CratonVM GPU         — cratonvm --gpu (transparent GPU offload)
#   4. TornadoVM            — TornadoVM @Parallel + TaskGraph API
#
# Each platform runs vector-add `out[i] = a[i] + b[i]` for n=1<<20 elements,
# warmup=2 + iters=5. Reports best_ns / mean_ns + correctness.

set +e

ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
RJVM="$ROOT/target/release/cratonvm.exe"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
HOTSPOT="$JDK/bin/java.exe"
TORNADO_SDK_SETUP="${TORNADO_SDK_SETUP:-C:/craton/tornadovm/setvars.sh}"
CRATON_GPU=$(ls -d "$ROOT/target/release/build/craton-gpu-"*/out/classes 2>/dev/null | head -1)
BENCH_CLASSES="$ROOT/apps/gpu-bench/classes"
TORNADO_DIR="$ROOT/bench-tornado"
LOG="$ROOT/applogs/bench-4way-$(date +%H%M%S)"
mkdir -p "$LOG"

N=${1:-1048576}
ITERS=${2:-5}
WARMUP=${3:-2}

echo "=== 4-way bench: n=$N iters=$ITERS warmup=$WARMUP ==="
echo "log dir: $LOG"

CSV="$LOG/results.csv"
echo "platform,best_ns,mean_ns,correctness" > "$CSV"

extract() {
    local log="$1"
    local best mean ok
    best=$(grep -oE 'best_ns=[0-9]+' "$log" 2>/dev/null | head -1 | grep -oE '[0-9]+$')
    mean=$(grep -oE 'mean_ns=[0-9]+' "$log" 2>/dev/null | head -1 | grep -oE '[0-9]+$')
    if grep -q "correctness=OK" "$log" 2>/dev/null; then ok=OK; else ok=FAIL; fi
    echo "${best:-?},${mean:-?},$ok"
}

# ----- 1. HotSpot C2 -----
echo "----- HotSpot C2 -----"
"$HOTSPOT" -XX:+TieredCompilation \
    -cp "$BENCH_CLASSES" CpuOnlyBench "$N" "$ITERS" "$WARMUP" \
    > "$LOG/hotspot.out" 2> "$LOG/hotspot.err"
echo "hotspot rc=$?"
echo "hotspot_c2,$(extract "$LOG/hotspot.out")" >> "$CSV"

# ----- 2. CratonVM CPU (JIT on) -----
echo "----- CratonVM CPU JIT-on -----"
CRATONVM_DISABLE_JIT=0 timeout 300 "$RJVM" --java-home "$JDK" \
    -c "$BENCH_CLASSES" CpuOnlyBench "$N" "$ITERS" "$WARMUP" \
    > "$LOG/cratoncpu.out" 2> "$LOG/cratoncpu.err"
echo "cratoncpu rc=$?"
echo "cratonvm_cpu_jit_on,$(extract "$LOG/cratoncpu.out")" >> "$CSV"

# ----- 3. CratonVM CPU (JIT off — workaround for int[] loop regression) -----
echo "----- CratonVM CPU JIT-off -----"
CRATONVM_DISABLE_JIT=1 timeout 300 "$RJVM" --java-home "$JDK" \
    -c "$BENCH_CLASSES" CpuOnlyBench "$N" "$ITERS" "$WARMUP" \
    > "$LOG/cratoncpu_nojit.out" 2> "$LOG/cratoncpu_nojit.err"
echo "cratoncpu_nojit rc=$?"
echo "cratonvm_cpu_jit_off,$(extract "$LOG/cratoncpu_nojit.out")" >> "$CSV"

# ----- 4. CratonVM GPU (--gpu) — uses GpuBench (with craton.gpu.*) -----
echo "----- CratonVM GPU -----"
CRATONVM_DISABLE_JIT=1 timeout 300 "$RJVM" --java-home "$JDK" \
    --gpu --print-gpu-decisions \
    -c "$BENCH_CLASSES;$CRATON_GPU" GpuBench "$N" "$ITERS" "$WARMUP" \
    > "$LOG/cratongpu.out" 2> "$LOG/cratongpu.err"
echo "cratongpu rc=$?"
# GpuBench prints "GPU best_ns=... mean_ns=..." not the CpuOnlyBench format
GPU_BEST=$(grep -oE 'GPU best_ns=[0-9]+' "$LOG/cratongpu.out" 2>/dev/null | grep -oE '[0-9]+$')
GPU_MEAN=$(grep -oE 'GPU.*mean_ns=[0-9]+' "$LOG/cratongpu.out" 2>/dev/null | head -1 | grep -oE '[0-9]+$')
GPU_OK=$(grep -q "correctness=OK" "$LOG/cratongpu.out" && echo OK || echo FAIL)
echo "cratonvm_gpu,${GPU_BEST:-?},${GPU_MEAN:-?},$GPU_OK" >> "$CSV"

# ----- 5. TornadoVM -----
echo "----- TornadoVM -----"
if [ -x "$TORNADO_DIR/run.sh" ]; then
    bash "$TORNADO_DIR/run.sh" "$N" "$ITERS" > "$LOG/tornado.out" 2> "$LOG/tornado.err"
    echo "tornado rc=$?"
    echo "tornadovm,$(extract "$LOG/tornado.out")" >> "$CSV"
else
    echo "TornadoVM not installed; skipping (expected $TORNADO_DIR/run.sh)"
    echo "tornadovm,not_installed,not_installed,SKIP" >> "$CSV"
fi

echo ""
echo "=== Results (n=$N, $ITERS iters + $WARMUP warmup) ==="
column -t -s, "$CSV"
