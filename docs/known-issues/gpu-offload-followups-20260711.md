# GPU offload — open follow-ups after first real-hardware validation (2026-07-11)

**Context.** First systematic validation of the GPU offload stack on real hardware
(RTX 2060, sm_75, CUDA driver 591.86, `--features gpu-driver` build of dev). The
launch path works end-to-end: interpreter hook → `offload::try_dispatch` →
`dispatch_method_from_native` → `dispatch_async` → `cuda-bridge` →
cudarc `cuLaunchKernel`, with checksums matching HotSpot bit-for-bit on every
kernel tested. Two bugs found that day were fixed in-tree (invoke-cache
promotion killing repeat offloads; failure-flag drained after array writebacks).
The items below remain OPEN.

## 1. Reduction kernels never dispatch (void-return gate)

`try_dispatch` only launches kernels for methods whose descriptor ends in `)V`.
A non-void kernel (e.g. `static long dotReduce(int[], int[])`) is analyzed as an
eligible reduction (`is_reduction: true`), lowered to PTX with an atomic-add
epilogue — and then falls through to the CPU because the transparent-dispatch
path has no way to push the scalar result onto the interpreter's operand stack
after the D2H copy. The whole analyzer/lowering half is done and tested; only
the dispatch-side "read back the 1-element result buffer and push it" is
missing. See the void gate in `vm/src/runtime/offload.rs` (`try_dispatch`,
"VOID return only" comment) and `dispatch_async`'s "Limitation: only `Void`
return" doc.

## 2. JIT-compiled callers bypass the offload hook

The offload hook lives in the interpreter's `execute_invokestatic` slow path.
The 2026-07-11 fix keeps offload-eligible *call sites* out of the invoke cache,
so interpreted callers now re-enter the hook on every call. But if the **caller
method itself** gets JIT-compiled (OSR of a hot loop that contains the
`invokestatic`), dispatch moves into JIT-emitted code and the hook is never
consulted again — offload silently stops, exactly like the pre-fix cache bug.
Not yet observed in practice (an offloaded kernel keeps its own CPU hotness
counter at zero, and benchmark reps are usually below OSR thresholds for the
caller), but structurally present. Options: teach the JIT's invokestatic
lowering to emit a slow-path call for offload-eligible targets (the
`jit-api/gpu-lowering` feature is a stub extension point for exactly this), or
deopt/ban JIT compilation of methods containing eligible call sites while
`--gpu` is on.

## 3. `dispatch_async` is synchronous under the hood

The Java-side async API (`GpuExecutor.submit` → `GpuFuture`) works, but the
Rust side records a completion event and the *finalize* path synchronizes on
it; there is no stream-callback-driven completion, so a `GpuFuture` never
completes without someone calling `get()`/`isDone()`. Real overlap of host work
and kernel execution beyond one queued submission per stream is future work
(Phase 5 follow-up note in `offload.rs`).

## 4. Small-array thread over-launch (min 2²⁰ threads per launch)

`dispatch_async` computes `work = max(runtime_work, signature.estimated_work)`
where `estimated_work` is a fixed `1 << 20` placeholder for every counted-loop
kernel. Any kernel over a smaller array (min-work default is 4096) still
launches 1,048,576 threads — up to 256× more than needed; the extras exit at
the loop guard. Harmless but wasteful (measured double-digit µs per launch of
pure guard-exit overhead). Fix: use `runtime_work` when it is non-zero, keep
`estimated_work` only for the scalar-only sentinel-0 case.

## 5. Occupancy-tuned block size is dead code

`DeviceModule::elementwise_for_kernel` (queries
`cuOccupancyMaxPotentialBlockSize`, falls back to 256) exists and is tested,
but `dispatch_async` always uses `LaunchConfig::elementwise` with the fixed
256-thread block. One-line switch, needs an A/B on real kernels.

## 6. Analyzer/lowering coverage gaps (each rejects otherwise-fine kernels)

- `ldc`/`ldc_w`/`ldc2_w` rejected → any int constant outside sipush range
  (|c| > 32767), any float/double/long literal constant, kills eligibility.
- `frem`/`drem` (IEEE remainder), `lcmp`/`fcmp*`/`dcmp*` comparisons.
- Only the canonical `for (i = 0; i < bound; i++)` loop: non-zero start,
  non-unit stride, `!=`/`<=` exits, and nested/2-D loops all reject.
- `invokestatic` intrinsics (e.g. `Math.sqrt`) accepted by the analyzer under
  `AllowIntrinsicCalls` but not resolved/lowered (PHASE1-GUESS marker).

## 7. No hardware CI

Everything real-GPU is `#[ignore]`d or `gpu-it`-gated; CI only checks the stub
backend compiles (`cuda-bridge.yml` runs `cargo check --features cuda`). A
weekly self-hosted job on a CUDA box running the `bench-gpu/` suite
(checksum-verified, per `bench-gpu/run-gpu-comparison.sh`) would have caught
the invoke-cache regression the day it landed.

## Benchmark snapshot backing this doc

See `bench-gpu/results/` (2026-07-11 files) and the README "GPU offload"
section for the CratonVM-vs-TornadoVM-vs-HotSpot numbers, including the
div-chain kernel where the GPU beats even auto-vectorized HotSpot C2 by ~70×.
Box caveat: measurements taken on a machine with ~25-30% background CPU load
(known infection, see session notes); GPU-side timings are largely unaffected
(GPU idle), CPU baselines are pessimistic by roughly that margin.
