# Phase 5 implementation spec — close the end-to-end Java loop

Phase 3 + 3.5 built every surface the Java side needs (executor /
future / stream / array / native shims / cudarc backend) but nothing
on the Java side actually invokes `OffloadCache::dispatch_async`.
Today `executor.submit(...)` calls `builtin_submit`, which records a
synthetic `FutureState::Failed` and returns. Phase 5 changes that:
add an **explicit named-method dispatch path** that actually runs a
kernel end-to-end (on a GPU box). Lambda-based submission is
deferred to Phase 6 — lambda → target-method resolution is a
separate large piece of work.

## Scope

1. **Java surface** — add `Native.submitMethod(execHandle, String
   className, String methodName, String descriptor, Object[] args)`
   plus a `GpuExecutor.submit(className, methodName, descriptor,
   Object...)` convenience method.
2. **Rust dispatch** — new `builtin_submit_method` shim that:
   - resolves `class_id` + `method_index` via `shared.class_manager`
   - calls `OffloadCache::lookup_or_compile` to ensure the kernel is
     in the cache
   - marshals the Java `Object[]` into `cuda_bridge::KernelArgs`,
     allocating device buffers for primitive arrays and threading
     through GpuArray handles for resident data
   - creates a stream via `cuda_bridge::Stream::new(ctx)`
   - calls `OffloadCache::dispatch_async`
   - on success, copies output back into the caller's Java array
   - registers the submission + wraps a `GpuFutureImpl`
3. **Marshalling helper** in `vm/src/runtime/gpu_marshal.rs` —
   `marshal_args(class, &method, args, ctx) -> Result<MarshalledArgs>`
   producing `KernelArgs` + a list of post-launch host-array updates.
4. **Output round-trip** — convention: the last array parameter is
   the output (already documented in Phase 1). After
   `dispatch_async` completes, copy the device buffer back into the
   original Java array.
5. **GpuArray routing** — when an arg is a `craton.gpu.GpuArray`
   (detected by class name), look up its `handle` field, fetch the
   `ResidentArray` from `SharedVm.residency`, and reuse the device
   buffer instead of re-uploading.
6. **Stub-mode integration test** — `vm/tests/gpu_submit_method.rs`
   exercises the new path; on a no-GPU dev box it returns Failed
   with the right error message but the full marshal + lookup path
   is exercised.
7. **User doc** — `docs/gpu/async-api.md` gets a new section showing
   the explicit-method submit pattern alongside the lambda-based one
   (with a "lambdas are Phase 6" note).

## Limitations carried over from Phase 4

- **Synchronous-under-the-hood**: Phase 4's `dispatch_async`
  internally calls `stream.synchronize()`. The Java side sees a
  callable-thread-driven async semantics, but the work is not
  overlapped with anything inside the dispatch call.
  PHASE6-FOLLOWUP: replace with event-record + poller.
- **Lambda resolution**: `Native.submit(execHandle, callable)`
  remains as-is (returns a synthetic Failed future). Real
  lambda-target resolution is Phase 6.
- **Object-typed args**: `submitMethod` accepts only primitive
  arrays, primitive scalars, and `GpuArray`. Any other object type
  → `Failed { "unsupported arg type: <fqn>" }`.

## Files touched

| File | Change |
|---|---|
| `craton-gpu/src/main/java/craton/gpu/internal/Native.java` | + `submitMethod` native declaration |
| `craton-gpu/src/main/java/craton/gpu/GpuExecutor.java` | + default `submit(class, method, desc, args...)` method |
| `craton-gpu/src/main/java/craton/gpu/internal/GpuExecutorImpl.java` | + override of the new default (delegate to Native) |
| `native-builtins/src/craton_gpu.rs` | + `builtin_submit_method` + registration |
| `vm/src/runtime/gpu_marshal.rs` | + `marshal_args` + `MarshalledArgs` |
| `vm/src/runtime/offload.rs` | (no changes; `dispatch_async` already finalized in Phase 4) |
| `vm/tests/gpu_submit_method.rs` | new integration test |
| `docs/gpu/async-api.md` | append "Explicit named-method dispatch" section |

## Acceptance

- `cargo check --workspace` (default) clean
- `cargo check --workspace --features cratonvm-vm/gpu-offload` clean
- `cargo check -p cuda-bridge --features cuda` clean
- `cargo test -p cratonvm-vm --features gpu-offload --test gpu_submit_method` passes
- Existing Phase 3 / 3.5 / 4 tests still green
- `docs/gpu/async-api.md` documents the new explicit-method pattern
