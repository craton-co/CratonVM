# CratonVM GPU offload — device launch glue is a stub (H2D=0, silent CPU fallback)

> **RESOLVED (validated 2026-07-11).** The launch glue was completed after this
> handoff was written: `try_dispatch` now marshals, launches on the RTX 2060,
> and writes back, with checksums matching HotSpot. This doc describes the
> pre-June-2026 state only. Open residuals live in
> `gpu-offload-followups-20260711.md`.

## Symptom
`bash test-infra/run-gpu-offload.sh` (gpu-driver binary, RTX 2060, sm_75):

```
1. DEVICE     PASS  CUDA context acquired under --gpu
2. ANALYZER   PASS  vaddMap Eligible / is_reduction:false
              PASS  dotReduce Eligible / is_reduction:true
              PASS  withCall Rejected
3. CORRECTNESS PASS MAP_CHECKSUM / DOT_CHECKSUM / MAX all match HotSpot
4. EXECUTION  FAIL  heavy kernel executed on GPU device (H2D=0 bytes uploaded)
              FAIL  heavy GPU result matches HotSpot ()    (no result)
              kernel time: CPU=? ms  GPU=? ms
SUMMARY: PASS=9 FAIL=2  RESULT: FAIL
```

The first three layers (device acquire, offload analyzer, numerical correctness)
all pass. The **EXECUTION layer fails**: for the compute-heavy `GpuCompute.heavy`
kernel (n=600M, 96 ops/elem) the H2D (host→device) byte counter is **0** — no
data is uploaded, so the kernel does not run on the device. It silently falls
back to the CPU.

## What this means for "GPU mode"
CratonVM's `--gpu` path is **correctness-complete but execution-incomplete**:
- It detects the CUDA device and acquires a context. ✓
- Its offload analyzer classifies kernels (map vs reduction vs rejected). ✓
- It produces numerically correct results. ✓ (because it computes on the CPU)
- It does **NOT** actually launch the kernel on the GPU — the `try_dispatch`
  launch glue (H2D copy + module load + kernel launch + D2H copy) is a stub, so
  every kernel transparently runs on the CPU.

### Corroboration from the vector-add 4-way
`test-infra/vadd-4way.sh` (n=2^28):
| variant | best | note |
|---|---|---|
| HotSpot C2 | 201 ms | CPU |
| CratonVM CPU (JIT) | 231 ms | CPU |
| CratonVM GPU (`--gpu`) | 175 ms | **"context acquired" but no H2D in the log → CPU fallback; 175 vs 231 is JIT/warmup variance, not device speedup** |
| TornadoVM (`@Parallel` PTX) | 180 ms | **genuinely on the RTX 2060 PTX device** |

And TornadoVM's compute-bound `PolyEvalTornado` (8.4M × 64 FMA) runs in **5.56 ms
on the PTX device** — a real on-device kernel. CratonVM has no equivalent
on-device execution today.

## Impact
"GPU mode" currently provides **no actual GPU acceleration** — it is a
transparent CPU fallback with a working analyzer. This is fine for *correctness*
(answers match HotSpot) but means the GPU build's benchmark numbers equal the
CPU build's (modulo noise), as observed throughout the comparison.

## What an agent should try next
1. Wire `try_dispatch` end-to-end: H2D upload (the `copy_from/to_native_memory`
   off-heap routing already exists — see `reference_server_socket_gap` arena
   note), CUDA module load of the lowered PTX, kernel launch with the computed
   grid/block, D2H copy back. The analyzer/lowering already produce the kernel;
   the missing piece is the launch glue.
2. Use `run-gpu-offload.sh` section 4 as the gate: it asserts `H2D > 0` AND a
   GPU result matching HotSpot AND GPU time < CPU time. Drive that to PASS.
3. Start with the proven-eligible `vaddMap` (map, is_reduction:false) before the
   reduction path.

## Reproduce
```
bash test-infra/run-gpu-offload.sh        # PASS=9 FAIL=2, RESULT: FAIL on §4
```

## Note
This is NOT a regression from the EC fix; it is the pre-existing state of the
GPU launch path. The gpu-driver binary itself builds and runs correctly.
