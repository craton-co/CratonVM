# Phase 3 Java async API — `GpuExecutor`, `GpuFuture`, `GpuArray`, `GpuStream`

Reference for the **explicit, async** Java surface introduced in Phase 3.
Companion to the automatic `invokestatic` offload path already documented
in [`README.md`](README.md).

The two paths coexist:

- **Automatic offload** (Phase 1/2, the "transparent" path elsewhere in
  these docs) — a `gpu`/`gpu-driver` build plus `--gpu` on the command line;
  a static method the analyzer accepts, called via `invokestatic`, is routed
  to the GPU without any call-site change. The user writes no async code.
  Methods outside the accepted shape run on the CPU as usual.
- **Explicit async API** (Phase 3, this document) — `GpuExecutor` lets the
  application *schedule* offloaded work, chain kernels on the same device
  stream, and read results back as `GpuFuture<T>`.

Same lowering pipeline, same analyzer, same PTX cache. Different entry
point.

> **`--gpu` is required for both.** This document used to say the
> explicit path needs no flag because "the executor probes the driver
> itself". The executor does acquire its own `DeviceContext` — and that
> is not the context the dispatch uses. `OffloadCache::new` sets
> `ctx = None` unless `config.gpu_offload_enabled`, so without `--gpu`
> every `lookup_or_compile` returns `Skip` and every submission is
> recorded as failed. What made the wrong claim survive is that
> `GpuExecutor.open()` still succeeds: the failure appears one call
> later, at `get()`, as "method not offloadable (Skip)".

## Quick start

Before — automatic offload of a single eligible kernel via `invokestatic`:

```java
// Run on a JVM started with --gpu. The call below is dispatched to the
// GPU by the interpreter hook with no application-visible asynchrony.
int[] out = new int[n];
Pipeline.vectorAdd(a, b, out);
useResult(out);
```

After — explicit submission through `GpuExecutor`:

```java
try (GpuExecutor exec = GpuExecutor.open()) {
    GpuFuture<int[]> f = exec.submit(() -> Pipeline.vectorAdd(a, b));
    useResult(f.get());
}
```

> This snippet shows the target shape of the API — a kernel that
> allocates and returns `int[]` directly. As implemented today, the
> dispatch layer only ever surfaces `SerializedResult::Void`: a kernel
> that itself returns an array (rather than writing into a
> caller-supplied output array) is not something the current marshalling
> path can hand back through `GpuFuture<T>`. Every example in this
> document that shows a return-value kernel signature should be read as
> the intended API shape; see [Current limitations](#current-limitations)
> for exactly what dispatches today.

The explicit form is for code that wants to *overlap* host work with
kernel execution, *chain* kernels on a single stream (one H2D, multiple
launches, one D2H), or *fail soft* on machines that have no GPU.

## `GpuExecutor`

`GpuExecutor` owns a `DeviceContext`, a default `GpuStream`, and a
reference to the per-VM `OffloadCache`. One executor binds to one CUDA
device; cross-device traffic requires a second executor.

### Opening

| Constructor | What it does |
| --- | --- |
| `GpuExecutor.open()` | Probe device 0. Throws `GpuException` if the driver is missing **only on `gpu-driver` builds**; on `gpu` (stub) builds it returns a *stub executor* whose every submission yields an immediately-failed future. See [Stub-mode behavior](#stub-mode-behavior). |
| `GpuExecutor.open(int deviceOrdinal)` | Attach to a specific CUDA device. Same fall-back behavior. |

The executor is `AutoCloseable`. Always wrap it in try-with-resources.
Closing the executor:

1. Synchronises the default stream.
2. Cancels any not-yet-launched submissions.
3. Releases the `DeviceContext` and the underlying CUDA primary
   context handle.

### Submitting work

| Method | Purpose |
| --- | --- |
| `<R> GpuFuture<R> submit(Supplier<R> kernel)` | Schedule a kernel for execution. The `Supplier` body **must be a static method reference** (e.g. `() -> Pipeline.vectorAdd(a, b)`); see [Limitations](#limitations). |
| `<R> GpuFuture<R> launch(KernelHandle<R> handle, Object... args)` | Lower-level form. Skips lambda inspection; `handle` is a `CompiledKernel` returned by a previous `prepare(...)` call. |
| `GpuStream newStream()` | Create an additional CUDA stream bound to the executor's context. The default stream is used implicitly if you never call this. |

`submit` returns immediately. The analyzer runs synchronously on the
calling thread *before* the future is returned, so any
`AnalyzerRejection` is observable at submit time, not at `get()` time.

### Ownership

```java
try (GpuExecutor exec = GpuExecutor.open()) {
    GpuFuture<int[]> f = exec.submit(() -> Pipeline.vectorAdd(a, b));
    int[] result = f.get();
}
```

`GpuExecutor` is `AutoCloseable` and **not** thread-safe for `close()`;
submissions are. Treat it like a `java.util.concurrent.ExecutorService`:
one owner thread closes it, many threads may submit while it is open.

### Submission handles must be released

The handle-based API (`dispatchNamedHandle`, `GpuStream.submitMethod`)
hands back a `long` that names an entry in the offload runtime's
submission registry. That entry pins the submission's CUDA stream and
event, and the registry is **process-global** -- not per-executor and not
per-VM. Release each handle when you are done with it:

```java
long h = exec.dispatchNamedHandle("Pipeline", "scale", "([I[I)V", args);
exec.awaitSubmission(h);
exec.releaseSubmission(h);   // <- required; the registry has no other drain
```

`releaseSubmission` is the only drain. Closing the executor does **not**
sweep the registry, and cannot: the map is global, so a close-time
drain-all would free submissions belonging to another executor in the
same process.

The value-returning API (`submit`/`launch`/`submitV` -> `GpuFuture.get()`)
registers a submission too, but you do not manage it by hand:
`GpuFutureImpl` registers itself with `StreamCleaner` (a
`java.lang.ref.Cleaner`), and the cleaning action calls `releaseFuture`
once the future becomes unreachable. So those handles are released **when
the future is collected**, and the registry's occupancy for that API is
bounded by collection frequency rather than by the life of the process.

Measured, 500 `submitV` + `get()` calls with the arrays hoisted out of
the loop so the loop itself allocates almost nothing:

```
no collection forced   registered=500 released=0   live_at_exit=500 peak_live=500
System.gc() every 50   registered=500 released=500 live_at_exit=0   peak_live=50
```

`peak_live` tracks the collection interval exactly. The first row is not
a leak -- it is a program that never gave the collector a reason to run,
which for a real workload is unusual. If you hold thousands of futures
between collections and that matters, take the handle API instead and
release explicitly.

Both of these only work as described since 2026-09-02. Before that
`releaseFuture` did not reach the offload registry, so **neither** path
drained: forcing a collection every 50 iterations still left
`released=0 live_at_exit=500`.

**This was broken until 2026-09-02** and is worth knowing about if you
are reading older code. `GpuExecutor.releaseSubmission(h)` compiles to
`Native.releaseFuture(h)`, which removed the entry from
`native-builtins`' own future table and stopped there; the offload
runtime's registry, keyed by the same handle, was never touched. It had
exactly one insert and one remove, and the remove had no production
caller at all, so **no program could drain it however correctly it was
written**. `bench-gpu/GpuAsyncChainBench.java`, which awaits and releases
every one of its handles, still reported:

```
[cratonvm] gpu submissions: registered=2001 released=0 live_at_exit=2001 peak_live=2001
```

and tripped the runtime's own "1024 submissions are alive" warning. With
the drain wired the same run reports:

```
[cratonvm] gpu submissions: registered=2001 released=2001 live_at_exit=0 peak_live=400
```

`peak_live` is now the program's own outstanding-chain length rather than
every submission it ever made. That census line is printed on any run
that registered a submission; `live_at_exit` should be 0 for a program
that releases what it takes, and `CRATONVM_GPU_NO_SUBMISSION_DRAIN=1`
restores the old behaviour if you need to compare.

## `GpuFuture<T>`

Returned by every `submit` / `launch`. Modelled on
`CompletableFuture` but bound to a CUDA stream.

> **Completion model.** `dispatch_async`
> (`vm/src/runtime/offload.rs`) launches the kernel, records a CUDA event,
> and returns immediately with the submission in the `Running` state.
> `GpuFuture.get()` still works exactly as before: it calls the blocking
> `Native.futureSynchronize` → `finalize_submission`, which calls
> `event.synchronize()` (a **blocking** host wait) and drains the
> writebacks. What's new is that `isDone()` / `futureStatus` are no longer
> a dead read of stale state: they now call `poll_submission_status`
> (`vm/src/runtime/offload.rs`), a genuinely non-blocking check that first
> looks at a `device_done` flag set by a best-effort `cuLaunchHostFunc`
> host callback registered at dispatch time, and falls back to a
> non-blocking `Event::query()` (`cuEventQuery`) if the callback hasn't
> fired yet. If either signals the device is actually done,
> `poll_submission_status` runs the same finalize work `get()` would have
> — inline, on the polling thread, with no further device wait (the event
> has already fired) — so **`isDone()` returning `true` now means the
> submission really is finalized**, not just "probably."
>
> Completion no longer requires a Java thread to
> call anything. The same `cuLaunchHostFunc` host callback that sets
> `device_done` also wakes a process-wide completion reaper thread
> (`ensure_completion_reaper_started`/`completion_reaper_loop` in
> `vm/src/runtime/offload.rs`) that runs `finalize_submission` itself, off
> the mutator entirely — draining writebacks, releasing the GC-critical
> guard, and flipping the submission to `Completed`/`Failed` while the
> application does something else. `isDone()`/`get()` now frequently just
> read an already-terminal status. The reaper is a single background
> daemon thread for the whole process (modelled on the existing
> background-JIT-compiler worker), started lazily on first dispatch and
> captures a `Weak<SharedVm>` so it can never keep a torn-down VM alive.

| Method | Semantics |
| --- | --- |
| `boolean isDone()` | A real non-blocking device probe (`Native.futureStatus` → `poll_submission_status`): checks a host-callback flag, falls back to non-blocking `Event::query()`, and finalizes inline if the device reports done. Safe to poll in a loop — each call does bounded work, never a blocking device wait. The submission is often already finalized by the background completion reaper by the time this is called at all. |
| `T get()` | Block the calling thread until done. This is the call that actually finalizes the submission (waits on the CUDA event, drains writebacks) if `isDone()`/`getNow()` haven't already done so. Throws `GpuException` if the kernel failed or the analyzer rejected the lambda. |
| `T get(long timeout, TimeUnit unit)` | Bounded wait. Throws `TimeoutException` on expiry. |
| `Optional<T> getNow()` | Non-blocking peek, same underlying `poll_submission_status` probe as `isDone()`. Returns the result if the device reports the submission complete (finalizing it inline as a side effect, same as `isDone()`), `Optional.empty()` while still `Running`. |
| `<R> GpuFuture<R> thenApplyGpu(Function<T, R> next)` | Spec'd stream-resident chaining — see [Current limitations](#current-limitations); today's dispatch layer has no mechanism to hand a kernel's device-side output directly to a second launch without a host round-trip. |
| `<R> CompletableFuture<R> thenApplyAsync(Function<T, R> next, Executor cpu)` | Standard CPU continuation. Inserts a D2H copy. |
| `CompletableFuture<T> toCompletableFuture()` | Bridge into JDK async land. Inserts a D2H copy on the first read. |

### Stream-resident chaining

`thenApplyGpu` is *specified* to be the one continuation that stays on
the device: the function must itself be a `@GpuKernel`-eligible static
method reference, admitted the same way `submit`'s lambda is. That
lambda-resolution half is real (`gpu_resolve_lambda_target` /
`gpu_dispatch_method` in `vm/src/vm/vm_exec.rs`) and is what
`submit`/`launch`/`submitWithArg(s)` already use.

What is **not** real yet: the actual "stays on the device" part. A
kernel's result is only ever surfaced to the host as
`SerializedResult::Void` — the write into the caller-supplied `out`
array *is* the result; there is no code path that keeps a kernel's
output as an opaque device-resident handle and feeds it as the next
kernel's input without a host round-trip (see [Current
limitations](#current-limitations)). Until that lands, treat
`thenApplyGpu` chains as target-API documentation rather than a
working no-D2H fast path, and expect each stage to behave like a
fresh `submit` against host-visible arrays.

Mixing CPU continuations (`thenApplyAsync`) and GPU continuations
(`thenApplyGpu`) on the same future is fine. Each CPU continuation
forces a D2H copy at the boundary.

## `GpuArray<T>`

A `GpuArray<T>` is a typed handle to a primitive Java array whose
**device residency** is managed by the executor. Wrapping a host array
does **not** copy it immediately; the upload happens on the first
kernel launch that consumes the array.

| Method | Purpose |
| --- | --- |
| `static GpuArray<int[]> wrap(int[] host)` | Create a handle. `Native.arrayWrap*` takes an eager **byte-copy snapshot** of the array into a Rust-owned buffer (`native-builtins/src/craton_gpu.rs::wrap_primitive_array`) — there is no `Heap::pin_ref` or other GC-pinning call; the snapshot exists precisely so GC moving the original Java array afterward is a non-issue. Type parameter `T` is one of the supported primitive-array types. |
| `static GpuArray<int[]> allocateInt(int len)` | Allocate a device-only array with no host mirror. `toHost()` materialises a fresh Java array on first call. **Both halves landed.** `native-builtins/src/craton_gpu.rs` registers `arrayAllocateInt`/`arrayAllocateLong`/`arrayAllocateFloat`/`arrayAllocateDouble` (`builtin_array_allocate_*`, minting a zero-filled device-only `state::ArrayEntry` the same way `arrayWrap*` does for a host-backed one), and gpu4j 0.4.0 calls them through `GpuArray.allocateInt/Long/Float/Double`, plus `allocateHalf` for fp16 over `arrayAllocateShort`. Note the shape: the factories are static and take no `GpuExecutor`, unlike the `allocate(exec, len)` this row used to describe. |
| `CompletableFuture<T> toHost()` | Schedule a D2H copy and return a future for the host array. **Always synchronises the stream.** Treat it as the expensive read-back operation it is. |
| `int length()` | Element count. Free; does not touch the device. |
| `void close()` | Release the device buffer. Idempotent. |

### Residency across kernels

The point of `GpuArray<T>` is that the array can sit on the device
across many kernels with no intermediate H2D / D2H traffic. The first
kernel that *writes* an array marks it dirty; subsequent kernels that
*read* the array see the dirty version without a host round-trip. A
`toHost()` call is the only operation that forces a D2H copy.

```java
GpuArray<int[]> img = GpuArray.wrap(image);
GpuFuture<int[]> step1 = exec.submit(() -> Pipeline.convolve(img.toHost().get()));
GpuFuture<int[]> step2 = step1.thenApplyGpu(c -> Pipeline.threshold(c, t));
return step2.get();
```

The `step1` lambda above takes the wrapped image and runs a convolve
kernel; `step2` chains a threshold kernel on the same stream. With
`thenApplyGpu` the intermediate `int[]` never round-trips to the
host. The final `step2.get()` is the single D2H copy.

### Type erasure caveat

`GpuArray<int[]>` and `GpuArray<float[]>` are distinct only at compile
time. The runtime element type is recovered from the Java class of the
*wrapped* array (`image.getClass().getComponentType()`). If you build a
`GpuArray<Object>` via raw types and feed it to a kernel expecting
`int[]`, the analyzer rejects the launch at submit time with
`Reason::TypeMismatch`. There is no kernel-side casting.

## `GpuStream`

`GpuStream` exposes one CUDA stream. It is rarely needed; most users
should rely on the executor's implicit default stream.

| Method | Purpose |
| --- | --- |
| `<R> GpuFuture<R> submit(Supplier<R> kernel)` | Submit onto this specific stream instead of the executor's default. |
| `void synchronize()` | Block until the stream is drained. |
| `void close()` | Destroy the stream. Outstanding futures complete or fail before close returns. |

> **Executor default-stream affinity is real; explicit
> `GpuStream` routing is still not.** These used to be one limitation; they
> are now two different states. `submit`/`launch`/`submitWithArg(s)`/
> `submitMethod` all route through `resolve_or_create_default_stream`
> (`native-builtins/src/craton_gpu.rs`), which lazily creates one real CUDA
> stream per `GpuExecutor` handle and caches it (`executor_default_stream:
> HashMap<u64, u64>`) — every submission on the **same** `GpuExecutor`, with
> no explicit stream involved, now serializes on that one real device
> stream, instead of each dispatch minting and tearing down its own
> one-shot stream. `Native.newStream` also mints a genuine CUDA stream now
> (`ctx.gpu_stream_create()`, not bookkeeping), but there is still no
> registered `Native.*` entry point that lets Java code aim a dispatch at
> *that* explicit handle instead of the executor's default — `GpuStream` in
> the implemented API surface is `handle()` + `close()` only, no `submit`.
> So: **one executor implicitly shares one real stream across its
> submissions today; `newStream()` mints a real stream you cannot yet
> route work onto.**

Reasons the API *intends* to let you take a stream explicitly (once
the routing above lands):

- **Overlap**. Two streams = concurrent H2D, kernel, and D2H across
  stages of a pipeline. (`cuda-bridge` does not yet expose a
  page-locked/pinned host-memory allocator — see
  [`streams-events.md`](streams-events.md#best-practices) — so the
  overlap benefit here comes from stream concurrency, not pinned
  transfers.)
- **Isolation**. Errors in one stream do not affect work queued on
  another.
- **Deterministic ordering**. Within a single stream, kernels execute
  in submission order. Across streams there is no ordering guarantee
  beyond what the application enforces.

If you don't have a specific reason to call `newStream()`, don't — the
handle it returns is real, but nothing in the registered `Native.*` surface
lets you route a dispatch onto it, so it still has no observable effect on
where work actually runs today. If your goal is "one executor's submissions
serialize predictably," you already have that from the default stream with no
`newStream()` call at all.

## Three-stage pipeline example

A realistic image-processing pipeline: convolve → threshold → histogram.
Two host arrays (`image`, `kernel`) are wrapped, three kernels are
submitted, the final histogram comes back to the host. Note the device
residency: one H2D for `image`, one D2H for `histogram`.

```java
import static cratonvm.gpu.GpuExecutor.open;

int[] runPipeline(int[] image, int[] kernel, int threshold) {
    try (GpuExecutor exec = open()) {
        GpuArray<int[]> img = GpuArray.wrap(image);
        GpuArray<int[]> ker = GpuArray.wrap(kernel);

        // Stage 1: convolve. Stays on device.
        GpuFuture<int[]> convolved = exec.submit(
                () -> Pipeline.convolve(img.toHost().get(), ker.toHost().get()));

        // Stage 2: threshold. Same stream as stage 1; no D2H.
        GpuFuture<int[]> thresholded = convolved.thenApplyGpu(
                c -> Pipeline.threshold(c, threshold));

        // Stage 3: histogram. Same stream; no D2H.
        GpuFuture<int[]> hist = thresholded.thenApplyGpu(
                Pipeline::histogram);

        // The first and only D2H copy:
        return hist.get();
    }
}
```

The kernel methods themselves are ordinary static methods marked with
`@GpuKernel`; the executor consults the analyzer to confirm that each
one is offload-eligible before issuing the launch.

If any stage's lambda fails analyzer admission (e.g. `Pipeline.histogram`
contained an allocation), the `thenApplyGpu` call that referenced it
returns an already-failed future and the downstream stages never reach
the device. The earlier stages, already in flight, still drain on the
stream and their results are dropped on executor close.

## Stub-mode behavior

On a `cargo build --features gpu` (stub) binary, or on a `gpu-driver`
build run on a machine with no CUDA driver, `GpuExecutor.open()` does
**not** throw. It returns a *stub executor* with three properties:

1. Every `submit(...)` and `launch(...)` returns an **already-failed**
   `GpuFuture<T>` whose `get()` throws
   `GpuException("no CUDA device available")`.
2. `thenApplyGpu(...)` on a failed future propagates the same failure
   without invoking the function.
3. `close()` is a no-op.

This lets unit tests exercise the control flow — `try / catch`, future
chaining, fallback paths — on hardware without a GPU. The stub futures
are *immediately* in the failed state, so `isDone()` returns `true` and
`getNow()` returns `Optional.empty()` instantly.

Code that wants to fall back to a CPU path on stub builds:

```java
int[] result;
try (GpuExecutor exec = GpuExecutor.open()) {
    result = exec.submit(() -> Pipeline.vectorAdd(a, b)).get();
} catch (GpuException e) {
    result = Pipeline.vectorAddCpu(a, b);
}
```

The `try-with-resources` close on a stub executor does not raise.

## Error handling

`GpuException` is **unchecked**. It is thrown by:

| Site | Cause |
| --- | --- |
| `GpuExecutor.open(...)` (gpu-driver only) | Driver init failed for a reason other than "no driver" — e.g. CUDA returns `ERROR_OUT_OF_MEMORY` while creating the primary context. The stub path swallows `NoDriver`; everything else propagates. |
| `submit(...)` | The lambda's referenced method failed analyzer admission. Wraps the analyzer's `Reason` (allocation, non-static, monitor, …). |
| `GpuFuture.get()` | The kernel ran but wrote `1` to `failure_flag` (array bounds, future arithmetic). Or the kernel itself failed to launch (resource exhaustion). |
| `GpuFuture.thenApplyGpu(...)` | The continuation's referenced method failed analyzer admission. Returned as a failed future, not thrown. `get()` on that future raises. |
| `GpuArray.toHost()` | D2H copy failed mid-stream. Rare; usually indicates the kernel that wrote this array faulted. |

`GpuException` carries:

- `cause()` — the underlying Rust `DeviceError` rendered as a Java
  exception chain. `DeviceError::NoDriver` surfaces as the *only*
  exception that the stub executor produces.
- `getKernel()` — the simple class/method name of the offending kernel,
  or `null` for analyzer-time rejections that fire before a kernel was
  ever bound.
- `getReason()` — the analyzer's `Reason` enum value when applicable;
  `null` for device-side failures.

`GpuException` is intentionally **not** a `CompletionException`. It does
not get wrapped twice when transiting `toCompletableFuture()`.

## Cleanup

`try-with-resources` is the only supported lifecycle. Specifically:

```java
try (GpuExecutor exec = GpuExecutor.open();
     GpuArray<int[]> img = GpuArray.wrap(image)) {
    // ... submit kernels ...
}
```

The order matters: `GpuArray` closes first, releasing its device-side
buffer and host-bytes snapshot (see the correction under
[`GpuArray<T>`](#gpuarrayt) — there is no GC pin to release, just a
Rust-owned copy and, once uploaded, a cached `DeviceBuffer`); then the
executor closes, synchronising the stream and releasing the context.

### `StreamCleaner` daemon

If a `GpuExecutor` is abandoned without `close()`, the JVM will
eventually collect it. A daemon thread named `gpu-stream-cleaner`
holds `PhantomReference`s to all live executors and, on enqueue,
releases the device handles. This is **a backup, not the contract**:

- The cleaner runs at GC pace, which is unpredictable.
- It cannot synchronise the stream from within itself (no JVM context),
  so in-flight kernels may have their output buffers freed before they
  complete on the device, producing CUDA `ERROR_ILLEGAL_ADDRESS` errors
  surfaced on the *next* kernel launch in any executor.
- It logs a `WARN` line "executor closed by cleaner; prefer
  try-with-resources" so misuse is observable.

Always `close()` explicitly. The cleaner exists so that one forgotten
executor doesn't pin a CUDA context for the JVM's lifetime; it is not
a substitute for resource management.

## Current limitations

Implementation-status gaps between this document's target API and what
`vm/src/runtime/offload.rs` / `native-builtins/src/craton_gpu.rs`
actually do (post `f4311e5f3`). These are distinct
from the by-design [Limitations](#limitations) below.

- **Push-driven completion model (closed).** `isDone()` /
  `getNow()` still work exactly as described under
  [`GpuFuture<T>`](#gpufuturet): a non-blocking `poll_submission_status`
  probe that finalizes a submission inline the moment it observes the
  device is done. What used to be missing — anything that drives
  completion *without* a Java call — now exists: the `cuLaunchHostFunc`
  host callback (`Stream::add_host_callback`) wakes a background
  completion reaper thread (`ensure_completion_reaper_started` in
  `vm/src/runtime/offload.rs`) that finalizes the submission itself, off
  the mutator, while application code is doing something else entirely.
  See [`async-completion-reaper.md`](async-completion-reaper.md) for the
  reaper's design.
- **Only `Void` results were surfaced at one point; scalar
  reduction results now reach Java too.** `SerializedResult`'s
  `ScalarI32/I64/F32/F64` variants (integer/long reduction kernels, `)I`/`)J`
  descriptors with a `red.global.add` epilogue) are wired into
  `Native.futureGetResult`, which now tries the real submission registry
  first (`NativeContext::gpu_future_take_result`) and boxes a scalar result
  via the same `Integer`/`Long`/`Float`/`Double` boxing path
  (`box_scalar_result` in `native-builtins/src/craton_gpu.rs`), falling
  back to the old pre-Phase-6 synthetic stub-future map only when the real
  registry has nothing for that handle. `PrimitiveArrayI32/I64/F32/F64`
  (a kernel returning a whole array by value, as opposed to writing into a
  caller-supplied `out` array) are still never constructed — that part of
  the target API in this document remains aspirational. Every
  array-**returning** kernel signature shown in this document
  (`Pipeline.vectorAdd(a, b)`, `Pipeline.histogram`, etc.) still describes
  the intended surface, not today's behavior; a scalar-returning reduction
  kernel now genuinely works end to end through `GpuExecutor`.
- **`GpuStream` affinity is partially wired up.** `resolve_or_create_default_stream` gives every submission on
  a given `GpuExecutor` — via `submit`/`launch`/`submitWithArg(s)`/
  `submitMethod`, with no explicit stream involved — one real, shared,
  lazily-created CUDA stream instead of a fresh private one per dispatch.
  What's still not wired: `newStream()` mints a genuine CUDA stream, but no
  registered `Native.*` entry point lets a dispatch be routed onto that
  explicit handle instead of the executor's default — see the note under
  [`GpuStream`](#gpustream).
- **JIT-caller bypass closed.** The automatic path still enters through the
  interpreter's `invokestatic` hook, so `offload_jit_gate` keeps a caller with
  an eligible offload site out of JIT and OSR compilation while `--gpu` is
  active. Callers without eligible sites remain compilable. See
  [`jit-caller-gate.md`](jit-caller-gate.md).

## Limitations

- **`thenApplyGpu` requires a static method reference.** Java's type
  erasure prevents the executor from inspecting an arbitrary lambda's
  body for `@GpuKernel` eligibility; the runtime can only identify the
  target method when the lambda is a direct `MethodHandleInfo` with a
  resolvable `REF_invokeStatic` kind. A non-method-reference lambda
  body — `c -> { int[] r = new int[c.length]; … }` — falls back to a
  CPU continuation with a D2H copy. The executor logs a `DEBUG` line
  identifying which call site lost stream residency.
- **No graph-capture API.** `cudaGraph` and friends are not exposed. The
  three-stage example above is implemented as three discrete launches.
  When a graph API arrives it will be a *new* surface on `GpuStream`,
  not a retrofit of `thenApplyGpu`.
- **No event timing.** `GpuStream` does not expose CUDA events to the
  Java caller. Latency and throughput numbers are measured in Rust via
  `tracing` spans (see [`first-results.md`](first-results.md)) or by the
  application wrapping its `submit` calls in `System.nanoTime`. A
  user-facing event timing API is deliberately deferred — the cost of
  exposing it cleanly across the stub / driver builds is not yet
  justified.
- **Single device per executor.** Multi-GPU sharding is the application's
  job: open one executor per device, partition the work.
- **No cancellation mid-launch.** `GpuFuture.cancel(true)` only succeeds
  if the kernel has not yet been issued to the stream. Once `cuLaunch`
  has run, the kernel runs to completion.

## What this is NOT

- **Not a Java CUDA wrapper.** You cannot `cuMemAlloc` from Java. The
  device-side surface is intentionally narrow — wrap an array, launch a
  kernel, read it back. Anything more is the Rust side's concern.
- **Not a replacement for the automatic `invokestatic` offload.**
  Code that has been running fine under `--gpu` should keep running
  fine; the explicit API is for *new* code that wants async semantics.
- **Not a Project Babylon stand-in.** Babylon's Code Reflection is a
  much broader Java-to-anywhere lowering effort. CratonVM's Phase 3 is
  narrow on purpose: static methods over primitive arrays, async
  scheduling, nothing more. If Babylon ships in mainline OpenJDK with a
  PTX target, we will reconsider the surface. Today, we don't depend on
  it.

## See also

- [`streams-events.md`](streams-events.md) — the Rust-side
  `Stream` / `Event` primitives that back `GpuStream` and `GpuFuture`.
  Required reading if you are extending the Java surface.
- [`annotations.md`](annotations.md) — `@GpuKernel`, the marker the
  analyzer looks for when admitting a method. Phase 3 lambdas resolve
  to methods bearing this annotation.
- [`README.md`](README.md) — top-level reference: build matrix
  (`gpu` vs `gpu-driver`), CLI surface, file index.
- [`reductions.md`](reductions.md) and [`jit-caller-gate.md`](jit-caller-gate.md)
  — scalar-reduction support and the closed JIT-caller bypass.
