# GPU-offload first results

This file is the acceptance gate for the GPU offload work. The dev
box this implementation was written on has no NVIDIA GPU, so the
tables below are intentionally placeholders. Anyone running on a
CUDA-equipped machine should paste real numbers into the row(s)
below — do not synthesise.

## What's being measured (after Phase 1–8)

Two independent dispatch paths:

| Path | Where the work lives | Exercised by |
|---|---|---|
| **Transparent** | `interpreter.rs::execute_invokestatic` hook → `OffloadCache::try_dispatch` (Phase 1 Part E) | `Benchmark.java` — plain `EligibleVectorAdd.vectorAdd(a,b,out)` call |
| **Explicit submit** | `Native.submitMethod` → `dispatch_method_from_native` → cudarc launch (Phase 5 + 3.5) + deferred finalize (Phase 7 #1) | `BenchmarkExplicit.java` — `executor.submit("EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out)` |

Both paths reach the same compiled kernel — they only differ in
the host-side entry point. The transparent path's overhead is one
hashmap lookup per invokestatic; the explicit path adds the lambda/
method-name resolution + the Java-side `GpuFuture` wrapping.

## Procedure

On a CUDA-12.x Windows x64 box with an NVIDIA GPU + JDK 21:

```powershell
# Build with the real driver bindings.
cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver

# Locate the craton-gpu annotations jar (built by craton-gpu/build.rs).
$annotJar = (Get-ChildItem -Recurse -Filter "craton-gpu-annotations.jar" target/release | Select-Object -First 1).FullName

# Compile the two benchmark fixtures + the eligible kernel.
javac -d test_classes/gpu `
    -cp $annotJar `
    test_classes/gpu/Benchmark.java `
    test_classes/gpu/BenchmarkExplicit.java `
    test_classes/gpu/EligibleVectorAdd.java

$N = 16777216
$ITERS = 5

# 1. CPU baseline (no --gpu).
./target/release/cratonvm.exe --classpath "test_classes/gpu" Benchmark $N $ITERS

# 2. Transparent GPU path.
./target/release/cratonvm.exe --gpu --classpath "test_classes/gpu" Benchmark $N $ITERS

# 3. Explicit submit path (Phase 5–7).
./target/release/cratonvm.exe --gpu --classpath "test_classes/gpu;$annotJar" BenchmarkExplicit $N $ITERS
```

Each run emits one `iter=...` line per iteration plus a single
`summary n=... iterations=... warmup=... min_ns=... mean_ns=... max_ns=...`
line at the end.

## Acceptance criteria

1. **Correctness (non-negotiable):** every iteration's `out[0]` and
   `out[n-1]` are identical, AND match across all three modes (CPU,
   transparent, explicit). The deterministic LCG in `Benchmark.main`
   produces the same input bytes for every run.
2. **Transparent speedup:** at `n = 1<<24` (16 Mi elements) the
   transparent GPU run's `mean_ns` is strictly less than the CPU run's.
   Target ≥ 2×; smaller speedups are a follow-up topic, not a fail
   (file an issue about marshalling overhead).
3. **Explicit speedup:** the explicit submit path's `mean_ns` is
   within 2× of the transparent path. Higher overhead is acceptable
   (lambda resolution + GpuFuture wrapping); >2× difference suggests
   something is wrong with the deferred-finalize path (Phase 7 #1).
4. **No regression:** `cargo test --workspace` (no features) still
   passes on the GPU box.

## Results table

| Date | Machine | n | iters | CPU mean_ns | Transparent mean_ns | Explicit mean_ns | CPU vs Transparent | Explicit vs Transparent | out[0] match | Notes |
| ---- | ------- | -- | ----- | ----------- | ------------------- | ---------------- | ------------------ | ----------------------- | ------------ | ----- |
| _pending_ | _GPU box, see procedure_ | 16,777,216 | 5 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | First measurement after Phase 1–8 land. |

## Sub-tables for the smaller sweeps

If you have spare time on the GPU box, populate these too — they
help diagnose where the per-mode overhead lives.

### Element-count sweep (transparent path)

| n | mean_ns | ns / element |
| - | ------- | ------------ |
| 1<<20 | _TBD_ | _TBD_ |
| 1<<22 | _TBD_ | _TBD_ |
| 1<<24 | _TBD_ | _TBD_ |
| 1<<26 | _TBD_ | _TBD_ |

### Per-iteration variance (n = 1<<24)

CPU vs Transparent vs Explicit, 20 iterations each (raise `iters`
to 20 in the run commands above). Paste the raw `iter=` lines into
the appropriate sub-section below.

## What "pending" actually means

After the Phase 1–8 commits all the host-side code paths are in
place. The cudarc backend (Phase 3.5) compiles cleanly against
`cudarc-0.13.9` but has never been linked against `libcuda.so` /
`nvcuda.dll` on this dev machine. On a GPU box:

- The transparent path's `try_dispatch` may surface a real cudarc
  error from the first kernel launch — that's the riskiest
  untested integration.
- The explicit path adds the `dispatch_method_from_native`
  marshaller layer + the deferred finalize. Both are blind-wired
  but their unit-level pieces all build clean.

If the GPU run hangs or panics: pin the exact panic line + the
last few `tracing::debug!` lines (run with
`CRATONVM_LOG=gpu.offload=debug,gpu=debug`) into an issue. The
likely-to-bite places are:

1. `cuda_bridge::backend_cuda::launch_on_raw_stream`'s
   `Vec<*mut c_void>` argument-pointer assembly (Phase 3.5 wrote
   this against cudarc 0.13.9 source without runtime validation).
2. `MarshalWriteback` lifetime — the `Arc<DeviceBuffer<T>>` is
   captured by both the kernel-args push closure (via raw ptr)
   and the writeback record; if those drop in the wrong order the
   raw pointer dangles.
3. Phase 7 #1's deferred finalize on the writebacks running on a
   different thread than dispatch — the GcCriticalGuard should
   keep GC paused but the actual `gpu_marshal::write_back_*`
   calls were written assuming a SafepointToken stay-alive
   contract.

If correctness fails (`out[0]` mismatch between CPU and GPU runs):
that's the kernel-emission bug. Diff the PTX (set
`CRATONVM_PTX_DUMP=1`) and compare against `EligibleVectorAdd`'s
expected pattern.
