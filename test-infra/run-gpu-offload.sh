#!/usr/bin/env bash
# ============================================================================
# GPU-offload regression test — an EXPLICIT check of whether GPU offload works.
#
# Tracked harness. The Java fixtures live (untracked, like the other suites)
# under apps/_test-suites/gpu-offload/; this script materializes them from the
# embedded sources below if they are missing, so a fresh checkout still runs.
# If you edit the fixtures, update BOTH the standalone .java files and the
# heredocs below (the standalone files win when present).
#
# Unlike a plain correctness test (which passes whether the kernel runs on the
# GPU or silently falls back to the CPU), this makes the GPU pipeline observable
# in three layers, plus a correctness gate:
#
#   1. DEVICE    — is a CUDA device present and is a context acquired?
#   2. ANALYZER  — does the offload analyzer classify each kernel correctly?
#                  vaddMap (map) -> Eligible/is_reduction:false;
#                  dotReduce (reduction) -> Eligible/is_reduction:true;
#                  withCall (has invokestatic) -> Rejected.
#                  (Locks in the map-vs-reduction fix in jit-cuda/analyzer.rs.)
#   3. EXECUTION — does the kernel ACTUALLY run on the device? Measured by
#                  timing the map kernel CPU vs GPU; equal time => CPU fallback
#                  (today's try_dispatch launch-glue stub).
#
# Exit codes: 0 = PASS, 1 = FAIL, 2 = SKIP (no gpu-driver binary / no CUDA device).
#
# Usage: bash test-infra/run-gpu-offload.sh
#   Env: ROOT, JDK, CV_CPU, CV_GPU override the auto-detected paths.
# ============================================================================
set +e

ROOT="${ROOT:-C:/craton/CratonVM}"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
CV_CPU="${CV_CPU:-$ROOT/target/release/cratonvm.exe}"
CV_GPU="${CV_GPU:-$ROOT/target-gpu/release/cratonvm.exe}"
HOTSPOT="$JDK/bin/java.exe"
JAVAC="$JDK/bin/javac.exe"
DIR="$ROOT/apps/_test-suites/gpu-offload"
OFFLOG="cratonvm_vm::runtime::offload=info"

say()  { printf '%s\n' "$*"; }
skip() { say "SKIP: $*"; exit 2; }
pass=0; fail=0
check() { if [ "$2" -eq 0 ]; then pass=$((pass+1)); say "  PASS  $1"; else fail=$((fail+1)); say "  FAIL  $1"; fi; }

# ── 0. Prerequisites ───────────────────────────────────────────────────────
[ -f "$CV_GPU" ] || skip "no gpu-driver binary at $CV_GPU — build with: CARGO_TARGET_DIR=target-gpu cargo build --release -p cratonvm-cli --features gpu-driver"
[ -f "$HOTSPOT" ] || skip "no HotSpot java at $HOTSPOT"
mkdir -p "$DIR"

# Materialize the fixtures from the embedded sources if absent (fresh clone).
if [ ! -f "$DIR/GpuProbe.java" ]; then
cat > "$DIR/GpuProbe.java" <<'EOF_PROBE'
// GPU offload probe — exercises the three analyzer outcomes plus a correctness
// check. Each kernel is a static method invoked from main(), so the
// interpreter's invokestatic offload hook analyzes (and, under --gpu, attempts
// to offload) it. Results are deterministic so the runner can diff against a
// HotSpot reference.  Usage: java GpuProbe [n]   (default n = 1<<20)
public class GpuProbe {
    // Eligible MAP: writes out[i]; NOT a reduction -> is_reduction:false.
    static void vaddMap(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }
    // Eligible REDUCTION: accumulates into a scalar return, no array store
    // -> is_reduction:true.
    static long dotReduce(int[] a, int[] b) {
        long sum = 0;
        int n = a.length;
        for (int i = 0; i < n; i++) {
            sum += (long) a[i] * b[i];
        }
        return sum;
    }
    // INELIGIBLE: contains an invokestatic (Math.max) -> Rejected(Invoke).
    static int withCall(int[] a) {
        int m = 0;
        for (int i = 0; i < a.length; i++) {
            m = Math.max(m, a[i]);
        }
        return m;
    }
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 20);
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
            b[i] = (i * 7) % 1000;
        }
        vaddMap(a, b, out);
        long dot = dotReduce(a, b);
        int mx = withCall(a);
        long mapChecksum = 0;
        for (int i = 0; i < n; i++) {
            mapChecksum += out[i];
        }
        System.out.println("n=" + n);
        System.out.println("MAP_CHECKSUM=" + mapChecksum);
        System.out.println("DOT_CHECKSUM=" + dot);
        System.out.println("MAX=" + mx);
        System.out.println("OUT0=" + out[0] + " OUTN=" + out[n - 1]);
    }
}
EOF_PROBE
fi

# GpuCompute: a COMPUTE-bound eligible kernel — a single counted loop whose
# body is a long, data-dependent integer multiply-add chain (constants in
# sipush range so no `ldc`). Heavy arithmetic, tiny memory traffic, so the
# GPU's parallelism crushes the CPU's serial execution (seconds vs ~100s).
# Generated with the chain unrolled OPS times.
COMPUTE_OPS="${COMPUTE_OPS:-96}"
if [ ! -f "$DIR/GpuCompute.java" ]; then
  {
    cat <<'EOF_CHEAD'
public class GpuCompute {
    static void heavy(int[] a, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            int x = a[i];
EOF_CHEAD
    i=0; while [ "$i" -lt "$COMPUTE_OPS" ]; do echo "            x = x * 1103 + 12345;"; i=$((i+1)); done
    cat <<'EOF_CTAIL'
            out[i] = x;
        }
    }
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 20);
        int[] a = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
        }
        long t0 = System.nanoTime();
        heavy(a, out);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        long checksum = 0;
        for (int i = 0; i < n; i++) {
            checksum += out[i];
        }
        System.out.println("n=" + n);
        System.out.println("heavy_ms=" + ms);
        System.out.println("COMPUTE_CHECKSUM=" + checksum);
        System.out.println("OUT0=" + out[0] + " OUTN=" + out[n - 1]);
    }
}
EOF_CTAIL
  } > "$DIR/GpuCompute.java"
fi

# Compile fixtures (recompile if missing or stale).
for cls in GpuProbe GpuCompute; do
  if [ ! -f "$DIR/$cls.class" ] || [ "$DIR/$cls.java" -nt "$DIR/$cls.class" ]; then
    "$JAVAC" -d "$DIR" "$DIR/$cls.java" || { say "javac failed for $cls"; exit 1; }
  fi
done

# Device probe: skip cleanly on a host with no CUDA device.
GPUINFO=$("$CV_GPU" --gpu-info 2>&1)
say "device: $(printf '%s' "$GPUINFO" | grep -aiE 'device 0|NVIDIA|sm_' | head -1)"
printf '%s' "$GPUINFO" | grep -aqiE "device 0|NVIDIA|sm_[0-9]" || skip "no CUDA device reported by --gpu-info"

# ── 1. Reference result from HotSpot ───────────────────────────────────────
N=1048576
REF=$("$HOTSPOT" -cp "$DIR" GpuProbe "$N" 2>/dev/null)
ref_map=$(printf '%s' "$REF" | grep -oE 'MAP_CHECKSUM=[0-9-]+' | cut -d= -f2)
ref_dot=$(printf '%s' "$REF" | grep -oE 'DOT_CHECKSUM=[0-9-]+' | cut -d= -f2)
ref_max=$(printf '%s' "$REF" | grep -oE 'MAX=[0-9-]+' | cut -d= -f2)
say "hotspot reference: MAP=$ref_map DOT=$ref_dot MAX=$ref_max"

# ── 2. CratonVM --gpu run with analyzer decision log ───────────────────────
GOUT=$(mktemp); GERR=$(mktemp)
RUST_LOG="$OFFLOG" "$CV_GPU" --gpu --print-gpu-decisions --gpu-min-work 256 \
  --java-home "$JDK" --Xmx 2g -cp "$DIR" GpuProbe "$N" >"$GOUT" 2>"$GERR"

say ""
say "── 1. DEVICE ──────────────────────────────────────────────"
grep -aq "context acquired" "$GERR"; check "CUDA context acquired under --gpu" $?

say "── 2. ANALYZER decisions (locks map-vs-reduction fix) ─────"
grep -aE "GpuProbe\.vaddMap" "$GERR" | grep -aq "Eligible"; check "vaddMap (map) is Eligible" $?
grep -aE "GpuProbe\.vaddMap" "$GERR" | head -1 | grep -aq "is_reduction: false"; check "vaddMap is_reduction:false (map, not reduction)" $?
grep -aE "GpuProbe\.dotReduce" "$GERR" | grep -aq "Eligible"; check "dotReduce (reduction) is Eligible" $?
grep -aE "GpuProbe\.dotReduce" "$GERR" | head -1 | grep -aq "is_reduction: true"; check "dotReduce is_reduction:true (genuine reduction)" $?
grep -aE "GpuProbe\.withCall" "$GERR" | grep -aq "Rejected"; check "withCall (has invokestatic) is Rejected" $?

say "── 3. CORRECTNESS (CratonVM --gpu vs HotSpot) ─────────────"
g_map=$(grep -oE 'MAP_CHECKSUM=[0-9-]+' "$GOUT" | cut -d= -f2)
g_dot=$(grep -oE 'DOT_CHECKSUM=[0-9-]+' "$GOUT" | cut -d= -f2)
g_max=$(grep -oE 'MAX=[0-9-]+' "$GOUT" | cut -d= -f2)
[ -n "$g_map" ] && [ "$g_map" = "$ref_map" ]; check "MAP_CHECKSUM matches HotSpot ($g_map)" $?
[ -n "$g_dot" ] && [ "$g_dot" = "$ref_dot" ]; check "DOT_CHECKSUM matches HotSpot ($g_dot)" $?
[ -n "$g_max" ] && [ "$g_max" = "$ref_max" ]; check "MAX matches HotSpot ($g_max)" $?

rm -f "$GOUT" "$GERR"

# ── 4. EXECUTION + COMPUTE-HEAVY DEMO ──────────────────────────────────────
# A compute-bound eligible kernel (long arithmetic chain per element, tiny
# memory traffic). The GPU runs every element in parallel while the CPU is
# serial, so the speedup is large and unmistakable — unlike the memory-bound
# vector-add where transfers make GPU≈CPU. Three hard checks:
#   * GPU executed on the device  — H2D byte trace (authoritative; proves the
#                                   transparent launch glue fired, not CPU)
#   * GPU result == HotSpot       — correctness of the device computation
#   * GPU is many× faster than CPU — the whole point of offloading
# COMPUTE_N is sized so GPU runs in a few seconds and CratonVM CPU in ~100s.
# Override COMPUTE_N (and COMPUTE_OPS) for a faster/slower run.
CN="${COMPUTE_N:-600000000}"
say ""
say "── 4. EXECUTION + compute-heavy demo (n=$CN, ${COMPUTE_OPS} ops/elem) ──"
# --stack-dump-on-timeout 0 disables CratonVM's 120s watchdog: the CPU run of
# this deliberately-heavy kernel legitimately takes ~100s and must not be killed.
cref=$("$HOTSPOT" -Xmx16g -cp "$DIR" GpuCompute "$CN" 2>/dev/null | grep -oE 'COMPUTE_CHECKSUM=-?[0-9]+' | cut -d= -f2)
GX_OUT=$(mktemp); GX_ERR=$(mktemp)
CRATONVM_GPU_TRACE_BYTES=1 RUST_LOG="$OFFLOG" "$CV_GPU" --gpu --gpu-min-work 256 \
  --stack-dump-on-timeout 0 --java-home "$JDK" --Xmx 16g -cp "$DIR" GpuCompute "$CN" >"$GX_OUT" 2>"$GX_ERR"
gpu_ms=$(grep -oE 'heavy_ms=[0-9]+' "$GX_OUT" | cut -d= -f2)
gcsum=$(grep -oE 'COMPUTE_CHECKSUM=-?[0-9]+' "$GX_OUT" | cut -d= -f2)
h2d=$(grep -aoE "submit H2D=[0-9]+ bytes \(GpuCompute\.heavy" "$GX_ERR" | grep -oE 'H2D=[0-9]+' | cut -d= -f2 | head -1)
cpu_ms=$("$CV_CPU" --stack-dump-on-timeout 0 --java-home "$JDK" --Xmx 16g -cp "$DIR" GpuCompute "$CN" 2>/dev/null | grep -oE 'heavy_ms=[0-9]+' | cut -d= -f2)
rm -f "$GX_OUT" "$GX_ERR"

[ -n "$h2d" ] && [ "$h2d" -gt 0 ]; check "heavy kernel executed on GPU device (H2D=${h2d:-0} bytes uploaded)" $?
[ -n "$gcsum" ] && [ "$gcsum" = "$cref" ]; check "heavy GPU result matches HotSpot ($gcsum)" $?
say "  kernel time:  CPU=${cpu_ms:-?} ms   GPU=${gpu_ms:-?} ms"
if [ -n "$cpu_ms" ] && [ -n "$gpu_ms" ] && [ "$gpu_ms" -gt 0 ]; then
  spd=$(( cpu_ms / gpu_ms ))
  say "  GPU speedup over CPU: ${spd}x"
  [ "$gpu_ms" -lt "$(( cpu_ms / 5 ))" ]; check "GPU >=5x faster than CPU on compute-bound kernel (${spd}x)" $?
fi
say ""
say "── SUMMARY ────────────────────────────────────────────────"
say "  gpu-offload: PASS=$pass FAIL=$fail"
[ "$fail" -eq 0 ] && { say "  RESULT: PASS"; exit 0; } || { say "  RESULT: FAIL"; exit 1; }
