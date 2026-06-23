# GPU Offload Comparison — CratonVM vs TornadoVM

**Date:** 2026-06-10  
**Hardware:** NVIDIA GeForce RTX 2060 (CUDA/PTX)  
**CratonVM:** `target/release/cratonvm.exe` (dev `f98fbca8`)  
**CratonVM-GPU:** `target-gpu/release/cratonvm.exe` (`--features gpu-driver --gpu`)  
**TornadoVM:** 4.0.1-jdk25, PTX backend, JDK 25.0.3  
**HotSpot:** JDK 25  

---

## GpuCompute.heavy — 96× multiply-add per element (compute-bound)

Each element: 96 iterations of `x = x * 1103 + 12345`.  
All checksums verified against HotSpot (✓ = all agree).  
CratonVM GPU timing: first call (includes PTX compilation + H2D + kernel + D2H).  
TornadoVM GPU timing: warm call (warmup call before measurement).

| N | CratonVM CPU | CratonVM GPU | HotSpot CPU | TornadoVM GPU | CV-GPU vs CV-CPU | TVM vs HotSpot |
|---|---|---|---|---|---|---|
| 2²⁰ (1M)  | 155ms  | 19ms  | 21ms  | 2ms  | **8.2×** ✓ | **10.5×** ✓ |
| 2²² (4M)  | 618ms  | 25ms  | 21ms  | 6ms  | **24.7×** ✓ | **3.5×** ✓ |
| 2²⁴ (16M) | 2404ms | 83ms  | 27ms  | 19ms | **29.0×** ✓ | **1.4×** ✓ |
| 2²⁶ (64M) | 9954ms | 286ms† | 55ms  | 77ms | **34.8×** ✓ | **0.7×** ✓ |

† 2^26 CV-GPU crashes intermittently (non-deterministic; also crashes at 2^28 per docs/gaps/gap-gpu-compute-large-n-crash.md).

**Observations:**
- CratonVM GPU delivers consistent 8–35× speedup over CratonVM CPU across all sizes (scales well with N)
- TornadoVM peaks at 10.5× over HotSpot at N=2^20 but degrades at larger N as H2D/D2H transfer dominates
- At N=2^26 HotSpot CPU (55ms) is faster than TornadoVM GPU (77ms) — memory bandwidth wall
- CratonVM GPU at N=2^24 (83ms) ≈ TornadoVM GPU (19ms) × 4.4; CratonVM overhead = PTX compile on first call + less-optimized transfer path

---

## GpuProbe.vaddMap — out[i]=a[i]+b[i] (memory-bound)

Pure memory bandwidth: `out[i] = a[i] + b[i]`, `a[i]=i`, `b[i]=(i*7)%1000`.  
CratonVM and HotSpot run as plain CPU (no GPU path for vadd in CratonVM unless `--gpu` is passed and the kernel is eligible).  
TornadoVM GPU: `@Parallel` annotation, warm timing.

| N | CratonVM CPU (checksum) | HotSpot CPU (checksum) | TornadoVM GPU |
|---|---|---|---|
| 2²⁰ (1M)  | CS:550279050800        | CS:550279050800        | 3ms  CS:550279050800        |
| 2²² (4M)  | CS:8798185971448       | CS:8798185971448       | 7ms  CS:8798185971448       |
| 2²⁴ (16M) | CS:140745860167760     | CS:140745860167760     | 33ms CS:140745860167760     |
| 2²⁶ (64M) | CS:2251833301005528    | CS:2251833301005528    | 90ms CS:2251833301005528    |

All checksums agree across CratonVM, HotSpot, and TornadoVM.

---

## API Comparison

| Aspect | CratonVM `--gpu` | TornadoVM |
|---|---|---|
| Annotation | None — automatic | `@Parallel` on loop variable |
| Data types | Plain `int[]` | `IntArray` (custom type, not `int[]`) |
| Task setup | Implicit | `TaskGraph` + `ImmutableTaskGraph` + `TornadoExecutionPlan` |
| Kernel compilation | First call (cold) | Warmup call required for warm timing |
| Source compatibility | Standard Java | Requires TornadoVM-specific types |
| Module requirement | None | Classes must be compiled with `-g` (LocalVariableTable required by Graal PTX compiler) |
| Crash boundary | 2^26+ unstable, 2^28 always | Handles 2^28 (461ms) — no crash boundary |

---

## Notes

- TornadoVM requires a named module or `-g` debug flag — classpath classes without `LocalVariableTable` fail with NPE in `PTXNodeLIRBuilder.emitPrologue()` (root cause, not a module-path issue)
- CratonVM automatic GPU offload requires no code changes; TornadoVM requires port to `IntArray`/`@Parallel`/`TaskGraph` API
- At N=2^28, CratonVM GPU crashes (documented gap); TornadoVM handles it in 461ms
- Benchmark source: `apps/_test-suites/gpu-offload/`; TornadoVM variants: `TornadoGpuCompute.java`, `TornadoVadd.java`
- Comparison script: `apps/_test-suites/gpu-offload/run-gpu-comparison.sh`
