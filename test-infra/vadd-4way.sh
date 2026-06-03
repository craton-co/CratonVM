#!/usr/bin/env bash
# Vector-add n=2^28, warmup=2 iters=5, real GPU harness:
#   HotSpot          CpuOnlyBench (pure Java)
#   CratonVM CPU     CpuOnlyBench (JIT-on, no --gpu)
#   CratonVM GPU     CpuOnlyBench --gpu  (transparent offload of EligibleVectorAdd.vectorAdd)
#   TornadoVM        VectorAddTornado (@Parallel + TaskGraph, PTX backend) via tornado launcher
set +e
ROOT="C:/craton/CratonVM"
CV_CPU="$ROOT/target/release/cratonvm.exe"
CV_GPU="$ROOT/target-gpu/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
HOTSPOT="$JDK/bin/java.exe"
BENCH="$ROOT/apps/gpu-bench/classes"
N=${1:-268435456}; ITERS=${2:-5}; WARMUP=${3:-2}
LOG="$ROOT/test-infra/suite-results/vadd-$(date +%H%M%S)"
mkdir -p "$LOG"
echo "n=$N iters=$ITERS warmup=$WARMUP"

echo "===== 1. HotSpot C2 ====="
"$HOTSPOT" -Xmx12g -cp "$BENCH" CpuOnlyBench "$N" "$ITERS" "$WARMUP" 2>"$LOG/hs.err" | tee "$LOG/hs.out"

echo "===== 2. CratonVM CPU (JIT-on) ====="
"$CV_CPU" --java-home "$JDK" --Xmx 16g -cp "$BENCH" CpuOnlyBench "$N" "$ITERS" "$WARMUP" 2>"$LOG/cpu.err" | grep -aE "best_ns|correctness|n=" | tee "$LOG/cpu.out"

echo "===== 3. CratonVM GPU (transparent --gpu) ====="
RUST_LOG="cratonvm_vm::runtime::offload=info" "$CV_GPU" --gpu --print-gpu-decisions --gpu-min-work 1024 \
    --java-home "$JDK" --Xmx 16g -cp "$BENCH" CpuOnlyBench "$N" "$ITERS" "$WARMUP" 2>"$LOG/gpu.err" | grep -aE "best_ns|correctness|n=" | tee "$LOG/gpu.out"
echo "--- GPU offload decision for vectorAdd + launch/fallthrough evidence ---"
grep -aiE "context acquired|EligibleVectorAdd|PTX load failed|lowering failed|launch|deopt|fell? ?through" "$LOG/gpu.err" | head

echo "===== 4. TornadoVM (@Parallel PTX) ====="
source /c/craton/tornadovm/setvars.sh >/dev/null 2>&1
( cd "$ROOT/bench-tornado" && tornado --threadInfo -cp . VectorAddTornado "$N" "$ITERS" ) 2>"$LOG/torn.err" | grep -aE "best_ns|mean_ns|correctness|avg|OK" | tee "$LOG/torn.out"

echo
echo "Logs: $LOG"
