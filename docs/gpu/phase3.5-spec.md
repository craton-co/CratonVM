# Phase 3.5 implementation spec — close PHASE3-GUESS / cuda backend port

This is a small follow-up batch closing every `PHASE3-GUESS` and
`PHASE3-CUDA-TODO` / `PHASE2-CUDA-TODO` marker left in source.

## Scope

1. **Java impl classes** for `GpuExecutor`, `GpuFuture`, `GpuStream`
   so the 5 native handlers can instantiate real objects instead of
   returning `null`.
2. **Update native handlers** in `native-builtins/src/craton_gpu.rs`
   to use the new impl classes.
3. **Replace placeholder `Stream`** in `vm/src/runtime/offload.rs`
   with `pub use cuda_bridge::Stream`.
4. **Port the cuda backend** in `cuda-bridge/src/{backend_cuda,
   stream, event, async_memcpy, launch}.rs` against the real
   cudarc 0.13 API. The previous fix agent stubbed every cuda-mode
   body with `NoDriver` because it guessed wrong about cudarc names.
   This time we discover the real API by reading the cudarc Cargo
   cache.
5. **Doc updates** — strike the addressed TODO markers from
   `docs/gpu/async-api.md` / `streams-events.md` / `phase3-spec.md`
   so the docs match reality.

## Items

| # | Description | Files | Approx LOC |
|---|---|---|---|
| P3.5-1 | Java impl classes | `craton-gpu/src/main/java/craton/gpu/internal/{GpuExecutorImpl,GpuFutureImpl,GpuStreamImpl}.java` | ~250 |
| P3.5-2 | Wire native handlers to impl classes | edits in `native-builtins/src/craton_gpu.rs` | ~200 |
| P3.5-3 | Replace placeholder `Stream` in offload.rs | small edit in `vm/src/runtime/offload.rs` | ~30 |
| P3.5-4 | Port cuda backend (all 5 files) | `cuda-bridge/src/{backend_cuda,stream,event,async_memcpy,launch}.rs` | ~600 |
| P3.5-5 | Doc updates removing stale TODOs | `docs/gpu/{async-api,streams-events,phase3-spec}.md`, `cuda-bridge/README.md` | ~80 |

5 items. Three are leaf-of-codebase tasks (P3.5-1, P3.5-3, P3.5-5)
that don't depend on anything in flight. P3.5-2 depends on P3.5-1's
Java FQNs. P3.5-4 is the heavy one and depends on cudarc 0.13's
actual surface.

## Contracts

### 2.1 Java impl classes (P3.5-1)

All three live in `craton.gpu.internal` package, package-private
where possible (only `Native` needs to instantiate them).

```java
package craton.gpu.internal;

import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;
import craton.gpu.GpuStream;
import craton.gpu.GpuRunnable;
import craton.gpu.GpuCallable;
import craton.gpu.GpuFunction;
import craton.gpu.GpuException;

final class GpuExecutorImpl implements GpuExecutor {
    private final long handle;
    private volatile boolean closed = false;

    /** Called only from Native.openExecutor — package-private. */
    GpuExecutorImpl(long handle) {
        this.handle = handle;
        StreamCleaner.register(this, handle, StreamCleaner.ResourceKind.EXECUTOR);
    }

    @Override public <R> GpuFuture<R> submit(GpuCallable<R> task) {
        if (closed) throw new GpuException("executor is closed");
        return Native.submit(handle, task);
    }

    @Override public GpuFuture<Void> launch(GpuRunnable task) {
        if (closed) throw new GpuException("executor is closed");
        return Native.launch(handle, task);
    }

    @Override public GpuStream newStream() {
        if (closed) throw new GpuException("executor is closed");
        return Native.newStream(handle);
    }

    @Override public void close() {
        if (closed) return;
        closed = true;
        Native.releaseExecutor(handle);
    }

    long handle() { return handle; }
}

final class GpuFutureImpl<T> implements GpuFuture<T> {
    private final long handle;

    GpuFutureImpl(long handle) {
        this.handle = handle;
        StreamCleaner.register(this, handle, StreamCleaner.ResourceKind.FUTURE);
    }

    @Override public boolean isDone() {
        return Native.futureStatus(handle) != 0; // 0 = running
    }

    @Override @SuppressWarnings("unchecked")
    public T get() throws InterruptedException, GpuException {
        Native.futureSynchronize(handle);
        int s = Native.futureStatus(handle);
        if (s == 2) {
            String msg = Native.futureGetErrorMessage(handle);
            throw new GpuException(msg != null ? msg : "kernel failed");
        }
        return (T) Native.futureGetResult(handle);
    }

    @Override public T getNow(T fallback) {
        if (!isDone()) return fallback;
        try { return get(); } catch (Exception e) { return fallback; }
    }

    @Override
    public <U> GpuFuture<U> thenApplyGpu(GpuFunction<? super T, ? extends U> fn) {
        try {
            T value = get();
            U result = fn.apply(value);
            // Phase 3.5: synchronous chain. Real stream-affine chaining
            // is a follow-up.
            return new CompletedFuture<>(result);
        } catch (Exception e) {
            throw new GpuException("thenApplyGpu failed", e);
        }
    }

    @Override public java.util.concurrent.CompletableFuture<T> toCompletableFuture() {
        return java.util.concurrent.CompletableFuture.supplyAsync(() -> {
            try { return get(); }
            catch (InterruptedException e) { Thread.currentThread().interrupt(); throw new GpuException("interrupted"); }
        });
    }

    long handle() { return handle; }

    /** Trivial already-done future, used internally by thenApplyGpu. */
    private static final class CompletedFuture<T> implements GpuFuture<T> {
        private final T value;
        CompletedFuture(T value) { this.value = value; }
        @Override public boolean isDone() { return true; }
        @Override public T get() { return value; }
        @Override public T getNow(T fallback) { return value; }
        @Override public <U> GpuFuture<U> thenApplyGpu(GpuFunction<? super T, ? extends U> fn) {
            try { return new CompletedFuture<>(fn.apply(value)); }
            catch (Exception e) { throw new GpuException("thenApplyGpu failed", e); }
        }
        @Override public java.util.concurrent.CompletableFuture<T> toCompletableFuture() {
            return java.util.concurrent.CompletableFuture.completedFuture(value);
        }
    }
}

final class GpuStreamImpl implements GpuStream {
    private final long handle;
    private volatile boolean closed = false;

    GpuStreamImpl(long handle) {
        this.handle = handle;
        StreamCleaner.register(this, handle, StreamCleaner.ResourceKind.STREAM);
    }

    @Override public long handle() { return handle; }

    @Override public void close() {
        if (closed) return;
        closed = true;
        Native.closeStream(handle);
    }
}
```

### 2.2 Native handler updates (P3.5-2)

Update these 5 handlers in `native-builtins/src/craton_gpu.rs` to
instantiate the impl classes via the existing
`NativeContext::new_object` pattern:

- `builtin_open_executor` → instantiate `craton/gpu/internal/GpuExecutorImpl` with the handle long.
- `builtin_submit` → instantiate `craton/gpu/internal/GpuFutureImpl`.
- `builtin_launch` → same as submit.
- `builtin_new_stream` → instantiate `craton/gpu/internal/GpuStreamImpl`.
- `builtin_future_get_result` → return the stored result object. For `SerializedResult::Void` return null; for primitive-array variants construct a new Java primitive array of the right type and populate it from the stored bytes (use `ctx.new_array(...)` + `ctx.set_array_element(...)`).

Use the same `NativeContext` API patterns the existing handlers
already use. The 14 handlers that already work today don't need
changes.

### 2.3 Replace placeholder Stream (P3.5-3)

In `vm/src/runtime/offload.rs`, find the PHASE3-CUDA-TODO block
declaring `pub struct Stream { pub id: u64 }` and replace it with:

```rust
pub use cuda_bridge::Stream;
```

`StreamSubmission::stream` field becomes `Arc<cuda_bridge::Stream>`.
The existing `dispatch_async` body that constructs the Failed
submission already takes a `stream: Arc<Stream>` parameter — keep
that signature, just rely on the import re-export.

### 2.4 Port cuda backend (P3.5-4)

The cudarc 0.13 crate is in `~/.cargo/registry/src/index.crates.io-*/cudarc-0.13.*/`.
**Discover the real API by reading the source.** Specifically look at:
- `src/driver/safe/mod.rs` — public types
- `src/driver/safe/core.rs` — `CudaContext` (or whatever the context type is)
- `src/driver/safe/launch.rs` — launch builders
- `src/driver/safe/alloc.rs` — `CudaSlice` allocation patterns
- `src/driver/result.rs` — raw FFI bindings if needed

cudarc 0.13's actual surface in our pinned version: the previous fix
agent reported `CudaContext`, `CudaStream::launch_builder`,
`CudaStream::new_stream`, `memcpy_stod` all don't exist. Whatever
the real names are, **use them**. If the type system genuinely
doesn't support what we want, leave a clear `PHASE4-CUDA-TODO`
explaining the gap — don't `NoDriver`-stub it.

The five files to port:
- `cuda-bridge/src/backend_cuda.rs` — `probe`, `DeviceContextInner`, `DeviceModuleInner`, `DeviceBufferInner`.
- `cuda-bridge/src/stream.rs` — cuda-mode `Stream::new`, `synchronize`, `raw`.
- `cuda-bridge/src/event.rs` — cuda-mode `Event::new`, `synchronize`, `query`, `Stream::record_event`, `Stream::wait_event`.
- `cuda-bridge/src/async_memcpy.rs` — cuda-mode `from_host_async`, `to_host_async`.
- `cuda-bridge/src/launch.rs` — cuda-mode `launch_on_stream`.

Each function currently returns `Err(DeviceError::NoDriver)` with a
PHASE2/3-CUDA-TODO marker. Replace with real cudarc calls.

### 2.5 Doc updates (P3.5-5)

- `docs/gpu/async-api.md` — strike the "stub mode behavior" caveat that says futures always fail. Replace with: "On a no-GPU host, `executor.open()` succeeds, but `submit` returns a Failed future."
- `docs/gpu/streams-events.md` — strike the "PHASE2-CUDA-TODO: cuda mode stubbed" notes.
- `docs/gpu/phase3-spec.md` — add a brief postscript: "Phase 3.5 closed the PHASE3-GUESS shims and ported the cudarc backend. See `phase3.5-spec.md`."
- `cuda-bridge/README.md` — confirm the stream/events section's samples match reality after the port.

## Constraints

1. **Do not run cargo, javac, jar, or anything.** Source only.
2. **Do not commit.**
3. **Java impl classes are package-private** — only `Native` instantiates them.
4. **Stub-mode behavior unchanged** — every cuda-mode function must
   still have an `#[cfg(not(feature = "cuda"))]` arm. After the
   port, the default workspace build still doesn't link cuda.
5. **`PHASE4-CUDA-TODO`** is the new marker for "this still doesn't
   work and needs GPU hardware" — replaces the PHASE2/3-CUDA-TODO
   markers as appropriate.

## Acceptance

- `cargo check --workspace` clean
- `cargo check --workspace --features cratonvm-vm/gpu-offload` clean
- `cargo check -p cuda-bridge --features cuda` clean **(if the port
  succeeds; if cudarc 0.13's API genuinely won't admit what we want,
  the build returning a compile error is honest and is fine — the
  orchestrator will dispatch a fix agent)**
- `cargo test -p cratonvm-vm --features gpu-offload` all-green for
  the GPU surface (offload, gpu_residency, gpu_marshal,
  gpu_async_stub).
- Java impl classes compile via the existing `craton-gpu/build.rs`
  javac pipeline.
