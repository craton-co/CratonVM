# Phase 7 summary — async overlap + caching + lambda completion

Phase 6 left five follow-ups. Phase 7 lands the three biggest and
documents why the remaining two are intentionally deferred.

## Landed

### Phase 7 #1 — real async overlap (`81470b4`)

Removed the in-`dispatch_async` `event.synchronize()` so two
consecutive `submit()` calls on the same stream can queue
back-to-back instead of serializing on the worker thread. The
event-record + writebacks moved into a deferred `FinalizeState`
on `StreamSubmission` that fires on the first
`future.get()` / `Native.futureSynchronize`.

New types:
- `FinalizeState { writebacks, _gc_critical: GcCriticalGuard }`
- `GcCriticalGuard` — `Send`-able RAII handle on
  `vm_heap::GPU_CRITICAL_COUNT` (the existing `SafepointToken` is
  intentionally `!Send` for single-thread RAII; the deferred
  finalize path crosses thread boundaries).

New free function `runtime::offload::finalize_submission(shared,
&Arc<StreamSubmission>) -> Result<(), String>` is what the
native `futureSynchronize` shim now delegates to. Idempotent
(takes the FinalizeState out under a Mutex; subsequent calls
return the cached terminal status).

Behavioral change: until the user calls `future.get()`,
`future.isDone()` returns `0` (Running) — Phase 6 #4's behavior
where it returned `1` immediately after dispatch is gone. This
is more honest about the kernel state.

### Phase 7 #2 — device-buffer caching for GpuArray (`cdfb1d4`)

`craton.gpu.GpuArray` handles now keep their `DeviceBuffer<T>`
alive in a process-wide `device_cache`. Subsequent kernel calls
referencing the same handle borrow from the cached `Arc` instead
of re-uploading host bytes.

Performance shape after this lands:

| Workload | Before | After |
|---|---|---|
| N kernels on one GpuArray | N × (H→D + K + D→H) | (H→D + K + D→H) + (N-1) × (K + D→H) |

The per-kernel D→H writeback still runs so `toHost()` reads
current bytes. Skipping the D→H for kernels in the middle of a
pipeline is a Phase 8 topic — needs an explicit "next access is
also GPU" hint.

`MarshalWriteback::Resident*` variants moved from owning
`DeviceBuffer<T>` to holding `Arc<DeviceBuffer<T>>` so cache +
writeback share the buffer.

Cache eviction on `Native.releaseArray` is **NOT** done in this
phase — the hook lives in native-builtins which can't depend on
the vm crate. Practical consequence: a long-running Java program
that churns through millions of GpuArrays without restart will
accumulate device memory. Marked PHASE8-FOLLOWUP.

### Phase 7 #3 — lambda SAM args (`4cc436a`)

Added `Native.submitWithArg(execHandle, lambda, samArg)` for
single-arg SAMs (the `GpuFunction<T,R>.apply(T)` shape, which
`GpuFuture.thenApplyGpu` uses). Captures + samArg become the
kernel arg list in declaration order.

`GpuFutureImpl.thenApplyGpu` now tries GPU first via
`submitWithArg`; on synthetic Failed (lambda not GPU-eligible)
falls back to the Phase 3.5 CPU `fn.apply` path so existing
user code keeps working.

User code like:

```java
GpuFuture<int[]> stage1 = exec.submit("P", "k1", "([I)[I", in);
GpuFuture<int[]> stage2 = stage1.thenApplyGpu(P::k2);
```

now actually runs `P::k2` on the GPU when the analyzer admits
it. Multi-arg SAMs (`BiFunction`-style) would need a parallel
`submitWithArgs(...Object[] samArgs)` — straightforward
extension when needed.

## Partial — Phase 7 #4 (non-static lambda targets)

The full deliverable (`obj::method` and `Class::instanceMethod`
lambdas dispatched on GPU) requires:

1. The jit-cuda analyzer to lift `Reason::NonStatic` — today
   every non-static method rejects on the first opcode.
2. The marshaller to thread the receiver `this` as the first
   kernel arg.
3. The PTX emitter to handle `aload_0; getfield` against device
   memory.

That is a substantial Phase 8 piece. For Phase 7 we land the
honest diagnostic: `gpu_resolve_lambda_target` now logs a
`tracing::debug!` at `target = "gpu.offload"` when it rejects a
non-static handle kind, naming the target class+member and the
PHASE8-FOLLOWUP marker. Users running with
`--print-gpu-decisions` see exactly why the lambda fell through
to CPU.

## Deferred — Phase 7 #5 (per-heap GPU_CRITICAL_COUNT)

`vm_heap::GPU_CRITICAL_COUNT` is process-wide rather than
per-`VmHeap`. CratonVM is one-VM-per-process by design, so the
distinction is academic. The only use case for a per-heap
counter is parallel unit tests running multiple VMs in the same
process — not on the roadmap.

The process-wide static is `pub`, called from `GcCriticalGuard`
in `runtime::offload`. The GC defer in
`wait_for_gpu_critical_drain` (Phase 6 #1) observes the same
counter via the same path.

If a per-heap counter becomes wanted: move the static onto
`GenerationalHeap` / `G1Collector` (the two `VmHeap` variants),
have `GcCriticalGuard::acquire(&VmHeap)` increment the right
one, and route `wait_for_gpu_critical_drain` through the
`VmHeap::gpu_critical_count` accessor. The Phase 6 #1 unit tests
would mostly carry over.

## Verification on no-GPU dev box

All Phase 7 commits leave the dev-box test matrix green:

| Target | Result |
|---|---|
| `cargo check --workspace` | clean |
| `cargo check --workspace --features rustjvm-vm/gpu-offload` | clean |
| `cargo check -p cuda-bridge --features cuda` | clean |
| `cargo test -p rustjvm-gc --features gpu-offload --lib` | 680 passed |
| `cargo test -p rustjvm-vm --features gpu-offload --lib offload` | 6 pass, 3 ignored |
| `cargo test -p rustjvm-vm --features gpu-offload --lib gpu_residency` | 5 pass |
| `cargo test -p rustjvm-vm --features gpu-offload --lib gpu_marshal` | 9 pass |
| `cargo test -p cuda-bridge` | 12 + 4 doc, 2 ignored |
| `cargo test -p jit-cuda` | 31 pass, 2 ignored |

GPU-host validation: still required. The on-device paths are
untested end-to-end because the dev box has no NVIDIA hardware.
The previously-flagged `PHASE3-CUDA-TODO` markers are all
gone (Phase 3.5 ported the cudarc backend); the remaining
`PHASE7-FOLLOWUP` / `PHASE8-FOLLOWUP` markers track only the
items above.

## What "Phase 8" would naturally contain

Not committed to scope here; just the natural backlog:

- Analyzer relaxation for `Reason::NonStatic` + receiver-as-arg
  marshalling — completes Phase 7 #4.
- `device_cache::release(handle)` wired into `Native.releaseArray`
  via a NativeContext escape hatch — completes Phase 7 #2 leak.
- CUDA Graph capture for stream-affine chaining without
  host-side intermediate `.get()` between linked submits.
- Skip the per-kernel D→H writeback for GpuArrays whose next
  access is another GPU dispatch (needs an explicit user
  signal or static analysis).
- Multi-arg SAMs (`BiFunction`-style) via
  `Native.submitWithArgs(lambda, Object[])`.
- Real end-to-end test against a GPU host with the existing
  `Benchmark.java` fixture (Phase 1 J's `first-results.md`
  scaffold).
