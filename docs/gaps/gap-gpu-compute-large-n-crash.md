# Gap: GPU compute crashes at N=2²⁸ (GpuCompute.heavy)

**Discovered:** 2026-06-09 (cross-VM comparison run with GPU binary from CratonVM-run worktree)  
**Severity:** Medium — GPU offload works correctly at N≤2²⁴ (verified); crashes silently at N=2²⁸  
**Binary:** `CratonVM-run/target-gpu/release/cratonvm.exe` (dev `f98fbca8`, `--features gpu-driver`)  
**Status:** ✅ **FIXED** — dev `85c80dfd` (2026-06-10), `target-gpu/release/cratonvm.exe` built 22:16

**Fix verification:** `GpuCompute.heavy` at N=2^28 completes in **1090ms** with correct checksum. All 5 N values now pass. Full results in `apps/demo/gpu-comparison-20260610-222135.md`.

---

## Symptom

```bash
CV_GPU="C:/craton/CratonVM-run/target-gpu/release/cratonvm.exe"
"$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" GpuCompute 268435456
# Output: (nothing — silent exit)
# rc=1
```

No output whatsoever — no checksum, no error, no stack trace. Process exits with rc=1 in ~32 seconds.

At smaller sizes GPU mode works correctly with a large speedup:

| N | CPU heavy_ms | GPU heavy_ms | Speedup | Correct? |
|---|---|---|---|---|
| 2²⁰ (1,048,576) | 148 ms | 10 ms | **14.8×** | ✓ (checksum match) |
| 2²⁴ (16,777,216) | — | 78 ms | — | ✓ |
| 2²⁸ (268,435,456) | 40,194 ms | ~32s then crash | — | ✗ (no output) |

---

## Reproduction

```bash
CV_GPU="C:/craton/CratonVM-run/target-gpu/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
GO="C:/craton/CratonVM/apps/_test-suites/gpu-offload"

# Works at 2^24:
"$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" GpuCompute 16777216
# n=16777216  heavy_ms=78  COMPUTE_CHECKSUM=...  correctness=OK

# Crashes at 2^28:
"$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" GpuCompute 268435456
# (no output)  rc=1
```

`GpuCompute.heavy` has the signature `static void heavy(int[], int[])` and runs a compute-heavy loop that CratonVM's GPU offload compiles to a CUDA/PTX kernel.

---

## Analysis

Three candidate root causes:

1. **GPU buffer allocation limit** — At 2^28 ints, the GPU buffer for a single array is 1 GB (`268435456 × 4 bytes`). With two arrays (input + output), that's 2 GB. The RTX 2060 has 6 GB VRAM but the allocator or CratonVM's off-heap arena may hit a limit at that size. The `DirectByteBuffer cap=-1` fix (`dbb_allocate_direct0 fixed-slot clobber`) would have addressed one such limit, but a 2^36-threshold check in `NativeContext::copy_from/to_native_memory` may not handle 1GB+ buffers.

2. **PTX kernel launch limit** — CUDA thread/block limits (max 1024 threads/block, max 2^31-1 blocks) impose a total max of ~2^41 threads per launch, well above 2^28. But the grid configuration may be coded for smaller sizes.

3. **CUDA memory copy timeout** — The D2H copy for 1 GB may exceed a Windows TDR (Timeout Detection and Recovery) threshold, causing the GPU driver to reset and `ExitProcess`.

---

## Fix direction

1. Add `CRATONVM_DBG_GPU_VERBOSE=1` logging around the GPU buffer allocation and kernel dispatch in `vm/src/gpu/` to identify where the silent rc=1 exit fires.

2. Test at N=2²⁶ and N=2²⁷ to narrow the threshold.

3. If it's a TDR issue, the CUDA dispatch can be split into chunks (e.g., 4× 2^26 launches).

4. If it's a buffer allocator limit, fix the size check in the off-heap arena path.

---

## GPU performance context

For reference, at 2^28:
- TornadoVM GPU (RTX 2060, VectorAddInt): ~461ms total (12ms kernel + 302ms H2D + 146ms D2H)
- CratonVM CPU (BenchSuite vadd2_28): 1,609ms (JIT)
- CratonVM GPU (BenchSuite vadd2_28): 1,616ms (GPU offload not triggering for the simple counted loop shape; same as CPU)
- CratonVM GPU (GpuCompute.heavy, 2^28): CRASH
