# Phase 8 summary — eviction, benchmark scaffold, multi-arg SAMs

Phase 7's backlog had six items. Phase 8 lands three concrete
deliverables and documents why the other three are deferred to a
future round.

## Landed

### Phase 8 #1 — device-cache eviction on `Native.releaseArray` (`ec8daf5`)

Closes the Phase 7 #2 PHASE8-FOLLOWUP memory-leak hazard. New
`NativeContext::gpu_release_array_cache(handle)` escape hatch
(default no-op; VM override calls `device_cache::release`). The
`builtin_release_array` native shim now calls it after dropping
the synthetic host-side resident entry, so the host bytes and
the cached device buffer drop atomically.

A long-running Java program that churns through many GpuArrays
no longer accumulates device memory.

### Phase 8 #3 — benchmark scaffold updated for Phase 5–8 paths (`2fe9205`)

The original Part-J `Benchmark.java` single-iteration scaffold
became a multi-iteration averaging harness with per-iteration
correctness checks. Added a parallel `BenchmarkExplicit.java`
that exercises the Phase 5 `executor.submit(class, method,
descriptor, args)` path so the explicit and transparent paths
can both be measured against each other on a GPU box.

`docs/gpu/first-results.md` is rewritten with:
- A three-mode acceptance table (CPU vs transparent vs explicit).
- A six-step PowerShell run procedure.
- Sweep sub-tables (element-count scaling + per-iteration variance).
- A "likely-to-bite-first" diagnostic guide naming the three
  specific places real-GPU validation might first surface bugs.

Side effect of getting the new fixture to compile: the
`GpuFuture.thenApplyGpu` interface signature now uses the
wildcards form `<U> GpuFuture<U> thenApplyGpu(GpuFunction<? super
T, ? extends U> fn)` matching what the impl classes already used.
This was a long-standing erasure mismatch that the build script's
javac was silently catching as a warning.

Also: `jit-cuda/build.rs::compile_top_level_fixtures` now adds
the craton-gpu annotations classpath to its javac invocation, so
new fixtures may freely import `craton.gpu.*`.

### Phase 8 #6 — `Native.submitWithArgs` for multi-arg SAMs (`ab1480a`)

Generalised companion to Phase 7 #3's `submitWithArg`. Same
resolve-then-append pattern, except `samArgs` is an `Object[]`
of arbitrary arity. Covers `BiFunction`, `TriFunction`, and any
user-defined multi-parameter `@FunctionalInterface`. Empty array
collapses to the Phase 6 #5 zero-arg case.

Java callers:

```java
BiFunction<int[], int[], int[]> add = Pipeline::add;
GpuFuture<int[]> f = (GpuFuture<int[]>) Native.submitWithArgs(
    0L, add, new Object[]{ a, b });
```

No new `GpuBiFunction`-style shadow interfaces in `craton.gpu` —
`java.util.function` already covers everything users need.

## Deferred

### Phase 8 #5 — skip mid-pipeline D→H writeback

Today every Resident-arg writeback downloads the device buffer
back to host bytes, even when the next access is another GPU
dispatch on the same GpuArray. The optimization: defer the
download until `Native.arrayToHost(handle)` is called.

Implementation requires:
- A `dirty: bool` flag per cached DeviceBuffer.
- Resident writebacks set the flag without copying.
- `Native.arrayToHost` consults a new
  `NativeContext::gpu_array_download_if_dirty(handle)` escape
  hatch to refresh host bytes before returning the Java array.

The perf win is real (saves one D→H per kernel in chained
pipelines) but unverifiable without a GPU. Marked Phase 9.

### Phase 8 #2 — analyzer relaxation for non-static lambda targets

Phase 7 #4 landed the diagnostic. The full deliverable —
`obj::method` and `Class::instanceMethod` lambdas dispatched on
GPU — needs:

1. Analyzer to lift `Reason::NonStatic` for selected non-static
   methods.
2. Marshaller to thread the receiver `this` as the first kernel
   arg.
3. PTX emitter to lower `aload_0; getfield` against device
   memory (escape analysis territory).

(3) is the substantial JIT-compiler work. Without it, even
relaxing the analyzer rarely admits real code (most instance
methods access `this.field`). Marked Phase 10.

### Phase 8 #4 — CUDA Graph capture for stream-affine chaining

Today `thenApplyGpu` works synchronously: it calls `get()` on
the source future before submitting the continuation. True
stream-affine chaining would queue the continuation on the GPU
directly using CUDA's stream-capture API, allowing back-to-back
kernels with zero host-side intermediate.

This is the highest-perf-impact item but also the biggest scope:
needs a new `GraphCapture` type on `cuda-bridge`, an
`OffloadCache::dispatch_into_graph` path, lifetime gymnastics
on the captured `KernelArgs`, plus a graph-replay mechanism on
`future.get()`. Marked Phase 11.

## Verification on no-GPU dev box

| Target | Result |
|---|---|
| `cargo check --workspace` | clean |
| `cargo check --workspace --features rustjvm-vm/gpu-offload` | clean |
| `cargo check -p cuda-bridge --features cuda` | clean |
| `cargo test -p rustjvm-vm --features gpu-offload --lib offload` | 6 passed, 3 ignored |
| `cargo test -p rustjvm-gc --features gpu-offload --lib` | 680 passed |
| All other GPU-tagged test crates | unchanged from Phase 7 |
| `test_classes/gpu/{Benchmark,BenchmarkExplicit}.class` | compiled by javac during `cargo build` |

## Phase 9+ natural backlog (what's left)

In order of impact/effort:

1. **Phase 9 — Phase 8 #5** (skip mid-pipeline D→H). Bounded,
   useful, awaits GPU validation of the existing path first.
2. **Phase 10 — Phase 8 #2** (non-static lambdas + analyzer +
   `getfield` lowering). Large but well-bounded.
3. **Phase 11 — Phase 8 #4** (CUDA Graph capture). Largest;
   blocked by a real-GPU performance baseline first to prove
   the benefit.
4. **Outside the queue:** the original Part-J `first-results.md`
   table still has pending rows. Filling those is the most
   useful "GPU-required" task and unblocks all of the above.
