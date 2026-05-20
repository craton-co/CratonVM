# Phase 3 implementation spec — Java `GpuExecutor` + futures + residency

Single source of truth for nine parallel implementation agents.
**Read sections 1 and 2 before writing any code.**

## 1. Scope

Phase 3 surfaces the Phase 2 plumbing to Java code. After Phase 3:

- Java programs can `executor.submit(() -> kernel(a, b))` and get a
  `GpuFuture<Result>`.
- `GpuArray<int[]>` carries device-resident data across kernel calls
  without round-tripping through the host.
- Stream affinity is preserved: `future1.thenApplyGpu(...)` stays on
  the same CUDA stream as `future1` was submitted on.
- Multiple CUDA devices supported via `OffloadCacheRegistry`.

**Transparent invokestatic offload still works unchanged.** Phase 3
adds a parallel imperative path; the declarative path from Phase 1
is unaffected.

## 2. Cross-cutting design contracts

### 2.1 Module layout

```
craton-gpu/
  src/main/java/craton/gpu/
    GpuExecutor.java            NEW — Item P3-1
    GpuFuture.java              NEW — Item P3-1
    GpuStream.java              NEW — Item P3-1
    GpuCallable.java            NEW — Item P3-1
    GpuFunction.java            NEW — Item P3-1
    GpuRunnable.java            NEW — Item P3-1
    GpuException.java           NEW — Item P3-1
    GpuArray.java               NEW — Item P3-2
    internal/
      Native.java               NEW — Item P3-3
      StreamCleaner.java        NEW — Item P3-3
    (existing annotations: GpuKernel/GpuExclude/etc. unchanged)

native-builtins/
  src/craton_gpu.rs             NEW — Item P3-4
  src/lib.rs                    (existing; one line: register module)

vm/
  src/runtime/
    offload.rs                  (existing; refactor — Item P3-5 + additions Item P3-6)
    gpu_marshal.rs              (existing; small touch — Item P3-7)
    gpu_residency.rs            NEW — Item P3-7
    mod.rs                      (existing; one line: pub mod gpu_residency)
  tests/
    gpu_async_stub.rs           NEW — Item P3-8

docs/gpu/
  async-api.md                  NEW — Item P3-9
  README.md                     (one-line append to Document map)
```

### 2.2 Java surface — `craton.gpu` package

#### `GpuExecutor`
```java
package craton.gpu;
import java.util.function.Supplier;

public interface GpuExecutor extends AutoCloseable {
    static GpuExecutor open() { return open(0); }
    static GpuExecutor open(int deviceOrdinal) {
        return craton.gpu.internal.Native.openExecutor(deviceOrdinal);
    }
    <R> GpuFuture<R> submit(GpuCallable<R> task);
    GpuFuture<Void>  launch(GpuRunnable task);
    GpuStream newStream();
    @Override void close();
}
```

The static `open` factories return an `executor` value. The
implementation is internal (`craton.gpu.internal.GpuExecutorImpl` —
NOT public). `internal.Native.openExecutor` returns a concrete
`GpuExecutor`.

#### `GpuFuture<T>`
```java
package craton.gpu;
import java.util.concurrent.CompletableFuture;
import java.util.function.Function;

public interface GpuFuture<T> {
    boolean isDone();
    T get() throws InterruptedException, GpuException;
    T getNow(T fallback);
    <U> GpuFuture<U> thenApplyGpu(GpuFunction<? super T, ? extends U> fn);
    CompletableFuture<T> toCompletableFuture();
}
```

#### `GpuStream`
```java
package craton.gpu;
public interface GpuStream extends AutoCloseable {
    long handle();   // opaque native handle (debug aid)
    @Override void close();
}
```

#### Functional interfaces
```java
// GpuCallable.java
package craton.gpu;
@FunctionalInterface
public interface GpuCallable<R> {
    R call() throws Exception;
}

// GpuFunction.java
package craton.gpu;
@FunctionalInterface
public interface GpuFunction<T, R> {
    R apply(T input) throws Exception;
}

// GpuRunnable.java
package craton.gpu;
@FunctionalInterface
public interface GpuRunnable {
    void run() throws Exception;
}
```

#### `GpuException`
```java
package craton.gpu;
public class GpuException extends RuntimeException {
    public GpuException(String message) { super(message); }
    public GpuException(String message, Throwable cause) { super(message, cause); }
}
```

#### `GpuArray<T>` (Item P3-2)
```java
package craton.gpu;

/**
 * A handle to data that may live on host, device, or both. Created
 * from a Java primitive array via {@link #wrap}. Passing a GpuArray
 * to a kernel keeps the data device-resident across calls, avoiding
 * H↔D round-trips.
 *
 * Type-erased at runtime: GpuArray<int[]>.toJava() returns Object;
 * the caller must cast.
 */
public final class GpuArray<T> {
    private final long handle;        // native residency-tracker id
    private final Class<?> elementType;
    private GpuArray(long handle, Class<?> elementType) {
        this.handle = handle;
        this.elementType = elementType;
    }
    public static GpuArray<int[]>    wrap(int[] host)    { ... }
    public static GpuArray<long[]>   wrap(long[] host)   { ... }
    public static GpuArray<float[]>  wrap(float[] host)  { ... }
    public static GpuArray<double[]> wrap(double[] host) { ... }
    public GpuFuture<T> toHost();
    public boolean isResident();
    public long handle() { return handle; }
}
```

#### `internal.Native` (Item P3-3)
All entry points are `static native` and registered as native
built-ins on the Rust side (Item P3-4):

```java
package craton.gpu.internal;

import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;
import craton.gpu.GpuStream;
import craton.gpu.GpuCallable;
import craton.gpu.GpuFunction;
import craton.gpu.GpuRunnable;

public final class Native {
    private Native() {}

    // Executor lifecycle.
    public static native GpuExecutor openExecutor(int deviceOrdinal);

    // Submission. The Rust side resolves the lambda's captured method
    // descriptor and dispatches via OffloadCache.
    public static native <R> GpuFuture<R> submit(long execHandle, GpuCallable<R> task);
    public static native GpuFuture<Void>  launch(long execHandle, GpuRunnable task);

    // Stream management.
    public static native GpuStream newStream(long execHandle);
    public static native void      closeStream(long streamHandle);

    // Future query / wait.
    public static native int     futureStatus(long futureHandle);          // 0=running, 1=done, 2=failed
    public static native void    futureSynchronize(long futureHandle);
    public static native Object  futureGetResult(long futureHandle);       // boxed result or null
    public static native String  futureGetErrorMessage(long futureHandle); // null if not failed

    // GpuArray<T> backing.
    public static native long    arrayWrapInt(int[] host);
    public static native long    arrayWrapLong(long[] host);
    public static native long    arrayWrapFloat(float[] host);
    public static native long    arrayWrapDouble(double[] host);
    public static native Object  arrayToHost(long arrayHandle);            // synchronous; returns the host array
    public static native boolean arrayIsResident(long arrayHandle);

    // Cleanup (called from StreamCleaner).
    public static native void    releaseFuture(long futureHandle);
    public static native void    releaseArray(long arrayHandle);
    public static native void    releaseExecutor(long execHandle);
}
```

#### `internal.StreamCleaner` (Item P3-3)
A `PhantomReference`-based cleanup thread for `GpuStream`,
`GpuFuture`, `GpuArray`. Standard java.lang.ref pattern:

```java
package craton.gpu.internal;

import java.lang.ref.PhantomReference;
import java.lang.ref.ReferenceQueue;

public final class StreamCleaner {
    private static final ReferenceQueue<Object> QUEUE = new ReferenceQueue<>();
    private static final Thread WORKER;
    static {
        WORKER = new Thread(StreamCleaner::run, "craton-gpu-cleaner");
        WORKER.setDaemon(true);
        WORKER.start();
    }
    public static <T> void register(T owner, long handle, ResourceKind kind) { ... }
    public enum ResourceKind { EXECUTOR, FUTURE, ARRAY, STREAM }
    private static void run() { /* drain queue, call Native.release* */ }
}
```

### 2.3 Rust surface

#### `native-builtins/src/craton_gpu.rs` (Item P3-4)

A new module that registers a built-in handler for every `Native.*`
method listed in §2.2. Pattern mirrors existing native-builtins:

```rust
// native-builtins/src/craton_gpu.rs

use crate::*;  // bring NativeBuiltin trait, NativeCallContext into scope

#[cfg(feature = "gpu-offload")]
pub(crate) fn register(registry: &mut NativeRegistry) {
    registry.register(
        "craton/gpu/internal/Native",
        "openExecutor",
        "(I)Lcraton/gpu/GpuExecutor;",
        builtin_open_executor,
    );
    // ... one register call per Native method
}

#[cfg(feature = "gpu-offload")]
fn builtin_open_executor(ctx: &mut NativeCallContext) -> NativeResult {
    let device_ordinal = ctx.arg_i32(0)?;
    // Look up the SharedVm's OffloadCacheRegistry and obtain/create an
    // OffloadCache for `device_ordinal`. Allocate an Executor handle.
    // Return a new GpuExecutorImpl Java object with the handle stored.
    todo!()
}

// ... one fn per Native method
```

**Behaviour in stub mode** (no GPU): every native that needs an
actual device returns a "not available" Java exception. The
`futureStatus` / `futureGetResult` methods on synthetic futures
behave correctly so unit tests can exercise the Java-side flow.

#### `vm/src/runtime/offload.rs` (Items P3-5 + P3-6)

**P3-5**: refactor existing single `OffloadCache` into a registry:

```rust
pub struct OffloadCacheRegistry {
    per_device: parking_lot::RwLock<rustc_hash::FxHashMap<u32, Arc<OffloadCache>>>,
}

impl OffloadCacheRegistry {
    pub fn new() -> Self { ... }
    pub fn get_or_create(&self, device_ordinal: u32, config: &VmConfig) -> Arc<OffloadCache>;
    pub fn get(&self, device_ordinal: u32) -> Option<Arc<OffloadCache>>;
}
```

`SharedVm::offload_cache: Arc<OffloadCache>` becomes
`SharedVm::offload_registry: Arc<OffloadCacheRegistry>`. Existing
call sites (Phase 1 hooks in `interpreter.rs`, `vm_init.rs`,
`class_load.rs`) get a one-line change to call
`offload_registry.get_or_create(0, &config)` and use the returned
`Arc<OffloadCache>`.

**P3-6**: add async dispatch on `OffloadCache`:

```rust
pub struct StreamSubmission {
    pub handle: u64,                       // unique id for futureGetResult lookup
    pub stream: Arc<cuda_bridge::Stream>,  // stream the work runs on
    pub status: parking_lot::Mutex<SubmissionStatus>,
}

pub enum SubmissionStatus {
    Running,
    Completed { result: SerializedResult },
    Failed { message: String },
}

pub enum SerializedResult {
    Void,
    PrimitiveArray { ty: PrimitiveType, bytes: Vec<u8> },
    // No object-result support in Phase 3.
}

impl OffloadCache {
    /// Async kernel dispatch. Returns a StreamSubmission whose handle
    /// is the Java-side GpuFuture's native handle.
    pub fn dispatch_async(
        &self,
        stream: &Arc<cuda_bridge::Stream>,
        class_id: ClassId,
        method_index: u16,
        args: KernelArgs,
    ) -> Arc<StreamSubmission> { ... }
}
```

Today (Phase 3 on no-GPU box): `dispatch_async` immediately
constructs a `SubmissionStatus::Failed { message: "no CUDA device" }`
when the cache has no device. The Java side propagates that as a
`GpuException`.

#### `vm/src/runtime/gpu_residency.rs` (Item P3-7)

A side-table that tracks `GpuArray<T>` handles → device buffers.

```rust
pub struct ResidencyTracker {
    arrays: parking_lot::RwLock<rustc_hash::FxHashMap<u64, ResidentArray>>,
    next_handle: std::sync::atomic::AtomicU64,
}

pub struct ResidentArray {
    pub element_type: PrimitiveType,
    pub host_bytes: Vec<u8>,                            // canonical copy
    pub device_bytes: Option<cuda_bridge::DeviceBuffer<u8>>, // None until first use
    pub last_stream: Option<Arc<cuda_bridge::Stream>>,
}

impl ResidencyTracker {
    pub fn new() -> Self;
    pub fn wrap(&self, element_type: PrimitiveType, host_bytes: Vec<u8>) -> u64;
    pub fn to_host(&self, handle: u64) -> Option<Vec<u8>>;
    pub fn is_resident(&self, handle: u64) -> bool;
    pub fn release(&self, handle: u64);
}
```

`SharedVm` gains `residency: Arc<ResidencyTracker>` behind
`#[cfg(feature = "gpu-offload")]`. Item P3-4's native shims call
into it.

### 2.4 Status / data flow

```
Java                                 Rust (cfg gpu-offload)
─────────────────────                ──────────────────────────────
GpuExecutor.open(0)                  Native::openExecutor
  → native call                        ← OffloadCacheRegistry::get_or_create(0)
  ← GpuExecutorImpl(handle=H1)

GpuArray.wrap(int[] arr)             Native::arrayWrapInt
  → native call                        ← ResidencyTracker::wrap(I32, bytes)
  ← long handle=A1

executor.submit(() -> k(a))          Native::submit
  → native call (with lambda)          ← OffloadCache::dispatch_async(stream, classId, methodIdx, args)
  ← GpuFutureImpl(handle=F1)             returns Arc<StreamSubmission>

future.get()                         Native::futureSynchronize, futureGetResult
  → native call                        ← StreamSubmission::wait + return
  ← typed result Java object
```

### 2.5 Stub-mode behavior (the no-GPU dev box)

- `Native::openExecutor` succeeds and returns a synthetic `GpuExecutor`
  (handle is a real registry id, but the cache has no device).
- `Native::submit` immediately constructs a `SubmissionStatus::Failed`
  future with message "no CUDA device available".
- `Native::futureGet*` and `Native::arrayToHost` work correctly
  against the synthetic state.
- `GpuArray.wrap` always succeeds (host bytes are tracked even
  without a device).

This lets us run **integration tests** that exercise the full Java
↔ Rust ↔ Java path with `cargo test --features gpu-offload` on the
dev box — no GPU required.

## 3. Item assignments

| # | Description | Files | LOC |
|---|---|---|---|
| P3-1 | `GpuExecutor`/`GpuFuture`/`GpuStream`/`GpuCallable`/`GpuFunction`/`GpuRunnable`/`GpuException` Java sources | 7 new files in `craton-gpu/src/main/java/craton/gpu/` | ~280 |
| P3-2 | `GpuArray<T>` Java source | 1 new file in `craton-gpu/src/main/java/craton/gpu/` | ~120 |
| P3-3 | `internal.Native` + `internal.StreamCleaner` | 2 new files in `craton-gpu/src/main/java/craton/gpu/internal/` | ~200 |
| P3-4 | Native built-in registrations + Rust impl | new `native-builtins/src/craton_gpu.rs` + 1-line edit in `native-builtins/src/lib.rs` | ~500 |
| P3-5 | `OffloadCacheRegistry` multi-device refactor | edits to `vm/src/runtime/offload.rs`, `vm/src/vm/vm_init.rs`, `vm/src/runtime/interpreter.rs` | ~200 |
| P3-6 | `dispatch_async` + `StreamSubmission` + `SerializedResult` | append to `vm/src/runtime/offload.rs` | ~250 |
| P3-7 | `ResidencyTracker` + Heap touchpoints | new `vm/src/runtime/gpu_residency.rs` + 1-line edit in `vm/src/runtime/mod.rs` | ~300 |
| P3-8 | Stub-mode integration test loading craton-gpu jar | new `vm/tests/gpu_async_stub.rs` | ~250 |
| P3-9 | `docs/gpu/async-api.md` + README link | new doc + 1-line edit | ~400 |

## 4. Constraints (every agent reads this)

1. **Do not run cargo, javac, jar, or any tool.** Source only.
2. **Do not commit.** Orchestrator handles commits and merges.
3. **Match names from §2 exactly.** `GpuExecutor`, `GpuFuture`,
   `Native.openExecutor`, `SubmissionStatus`, `ResidencyTracker`, etc.
4. **Feature gating**: all new Rust code under
   `#[cfg(feature = "gpu-offload")]` where appropriate; native-builtins
   crate's craton_gpu module is feature-gated on `gpu-offload`.
5. **Each agent owns its own files.** Cross-file conflicts:
   - `vm/src/runtime/offload.rs` — P3-5 (refactor) and P3-6 (append).
     P3-5 should preserve appending-friendly structure; P3-6 appends
     a new impl block at end of file.
   - `vm/src/runtime/mod.rs` — P3-7 adds ONE line.
   - `native-builtins/src/lib.rs` — P3-4 adds ONE line.
6. **Two feature flags**:
   - `gpu-offload` on `rustjvm-vm` — enables the runtime side.
   - `gpu-offload` on `native-builtins` — new, gates the registry module.
     Add this feature in P3-4.
7. **Stub-mode compiles without a GPU.** Every native shim has a
   real Rust impl that compiles and returns the right error type
   when the cache has no device.
8. **No JNI.** Use the existing `native-builtins` pattern — register
   handlers keyed by `(class_internal_name, method_name, descriptor)`.
   The interpreter dispatches by lookup. Do not require JNI infrastructure.
9. **If you must guess at an API or trait method name, mark with
   `// PHASE3-GUESS:` so the orchestrator can spot it.**

## 5. Acceptance for Phase 3

- [ ] `cargo check --workspace` (default features) clean
- [ ] `cargo check --workspace --features rustjvm-vm/gpu-offload` clean
- [ ] `cargo check -p craton-gpu` clean (javac compiles the new Java sources)
- [ ] `cargo test -p rustjvm-vm --features gpu-offload --test gpu_async_stub` passes
- [ ] `cargo test -p rustjvm-vm --features gpu-offload --lib offload` passes (existing)
- [ ] `docs/gpu/async-api.md` exists and is linked from `docs/gpu/README.md`
- [ ] All Phase 1 + Phase 2 tests still green

The orchestrator iterates: merge → build → diagnose → dispatch fix
agents → re-build until acceptance.
