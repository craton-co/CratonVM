#!/usr/bin/env bash
# 4-way polynomial-eval benchmark — heavy per-element work (64 FMAs).
# Sized so HotSpot C2 takes ~1s at n=2^23 (8,388,608 elements).
#
# Runs:
#   1. HotSpot C2          — CpuPolyBench (pure Java, plain JVM)
#   2. CratonVM CPU JIT-on — CpuPolyBench under cratonvm (no --gpu)
#   3. CratonVM CPU JIT-off — same with CRATONVM_DISABLE_JIT=1
#   4. CratonVM GPU         — GpuPolyBench under cratonvm --gpu (explicit submit)
#   5. TornadoVM            — PolyEvalTornado.java via tornado launcher
set +e

ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
RJVM="$ROOT/target/release/cratonvm.exe"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
HOTSPOT="$JDK/bin/java.exe"
TORNADO_DIR="$ROOT/bench-tornado"
CRATON_GPU=$(ls -d "$ROOT/target-gpu/release/cratonvm.exe"*/out/classes 2>/dev/null | head -1)
BENCH_CLASSES="$ROOT/apps/gpu-bench/classes"
LOG="$ROOT/applogs/bench-poly-4way-$(date +%H%M%S)"
mkdir -p "$LOG"

N=${1:-8388608}
ITERS=${2:-5}
WARMUP=${3:-2}

echo "=== 4-way polynomial eval bench: n=$N iters=$ITERS warmup=$WARMUP ==="
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

# 1. HotSpot C2
echo "----- HotSpot C2 -----"
"$HOTSPOT" -Xmx2g -XX:+TieredCompilation \
    -cp "$BENCH_CLASSES" CpuPolyBench "$N" "$ITERS" "$WARMUP" \
    > "$LOG/hotspot.out" 2> "$LOG/hotspot.err"
echo "hotspot rc=$?"
echo "hotspot_c2,$(extract "$LOG/hotspot.out")" >> "$CSV"

# 2. CratonVM CPU JIT-on
echo "----- CratonVM CPU JIT-on -----"
CRATONVM_DISABLE_JIT=0 timeout 300 "$RJVM" --java-home "$JDK" \
    --Xmx 2g -c "$BENCH_CLASSES" CpuPolyBench "$N" "$ITERS" "$WARMUP" \
    > "$LOG/cratoncpu_jit_on.out" 2> "$LOG/cratoncpu_jit_on.err"
echo "cratoncpu_jit_on rc=$?"
echo "cratonvm_cpu_jit_on,$(extract "$LOG/cratoncpu_jit_on.out")" >> "$CSV"

# 3. CratonVM CPU JIT-off
echo "----- CratonVM CPU JIT-off -----"
CRATONVM_DISABLE_JIT=1 timeout 300 "$RJVM" --java-home "$JDK" \
    --Xmx 2g -c "$BENCH_CLASSES" CpuPolyBench "$N" "$ITERS" "$WARMUP" \
    > "$LOG/cratoncpu_jit_off.out" 2> "$LOG/cratoncpu_jit_off.err"
echo "cratoncpu_jit_off rc=$?"
echo "cratonvm_cpu_jit_off,$(extract "$LOG/cratoncpu_jit_off.out")" >> "$CSV"

# 4. CratonVM GPU
echo "----- CratonVM GPU -----"
CRATONVM_DISABLE_JIT=1 timeout 300 "$RJVM" --java-home "$JDK" \
    --Xmx 2g --gpu --print-gpu-decisions \
    -c "$BENCH_CLASSES;$CRATON_GPU" GpuPolyBench "$N" "$ITERS" "$WARMUP" \
    > "$LOG/cratongpu.out" 2> "$LOG/cratongpu.err"
echo "cratongpu rc=$?"
echo "cratonvm_gpu,$(extract "$LOG/cratongpu.out")" >> "$CSV"

# 5. TornadoVM
echo "----- TornadoVM -----"
if [ -d "$TORNADO_DIR" ] && [ -f "$TORNADO_DIR/PolyEvalTornado.java" ]; then
    (
        source /c/craton/tornadovm/setvars.sh >/dev/null
        cd "$TORNADO_DIR"
        API_JAR="C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/share/java/tornado/tornado-api-4.0.1-jdk25.jar"
        ANNO_JAR="C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/share/java/tornado/tornado-annotation-4.0.1-jdk25.jar"
        if [ ! -f PolyEvalTornado.class ] || [ PolyEvalTornado.java -nt PolyEvalTornado.class ]; then
            echo "[bench-poly] compiling PolyEvalTornado.java (with -g)..." >&2
            javac -g --enable-preview --release 25 -cp "$API_JAR;$ANNO_JAR" PolyEvalTornado.java
        fi
        tornado --classpath . PolyEvalTornado "$N" "$ITERS"
    ) > "$LOG/tornado.out" 2> "$LOG/tornado.err"
    echo "tornado rc=$?"
    echo "tornadovm,$(extract "$LOG/tornado.out")" >> "$CSV"
else
    echo "TornadoVM PolyEval bench not set up; skipping"
    echo "tornadovm,not_installed,not_installed,SKIP" >> "$CSV"
fi

echo ""
echo "=== Results (poly64-eval, n=$N, $ITERS iters + $WARMUP warmup) ==="
column -t -s, "$CSV"
