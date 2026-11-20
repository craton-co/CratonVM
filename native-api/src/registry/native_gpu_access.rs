#[allow(unused_imports)]
use super::*;

pub trait NativeGpuAccess: NativeInvokeAccess {
    /// Capability boundary: Optional GPU submission and result services.

    /// Phase 5 escape hatch for GPU offload — dispatch the named method
    /// asynchronously on the GPU and return the submission handle. The
    /// default impl returns `None` (no GPU offload). The VM's
    /// `NativeContextImpl` overrides under `#[cfg(feature = "gpu-offload")]`
    /// to resolve `class_name`/`method_name`/`descriptor` against the
    /// class manager, marshal `java_args` into `KernelArgs`, and call
    /// `OffloadCache::dispatch_async`. The returned handle is what the
    /// Java `GpuFutureImpl` wraps; pass it back to
    /// `Native.futureSynchronize` / `Native.futureGetResult` to drive
    /// the future.
    ///
    /// `java_args` follows the same convention as the JVM stack: each
    /// `Value::Object(Some(...))` is a Java array reference, each
    /// `Value::Int/Long/Float/Double` is a primitive scalar.
    fn gpu_dispatch_method(
        &mut self,
        _class_name: &str,
        _method_name: &str,
        _descriptor: &str,
        _java_args: &[Value],
    ) -> Option<u64> {
        None
    }

    /// Dispatch a built-in GEMM: `C[MxN] = A[MxK] * B[KxN]`, row-major.
    ///
    /// `half` selects the fp16 input variant (fp32 accumulate) over the
    /// all-fp32 one. The three handles are `craton.gpu.GpuArray` handles,
    /// not Java arrays, deliberately: their device buffers are cached, so
    /// a weight matrix uploaded once stays resident across calls. A
    /// decode step multiplies by the same weights every token, and
    /// re-uploading them would cost more than the arithmetic.
    ///
    /// `_trans_a` / `_trans_b` read the corresponding operand transposed,
    /// expressed as a stride pair rather than a separate kernel, so the
    /// operand's element count is unchanged and only its indexing differs.
    ///
    /// `_stream_handle` places the launch on a Java-visible `GpuStream`;
    /// `None` uses the shared built-in stream.
    ///
    /// Returns a submission handle with the usual lifecycle, or `None`
    /// when this VM has no GPU offload compiled in.
    ///
    /// This exists because the bytecode lowering cannot express a matrix
    /// multiply — three nested loops, a shared tile, a barrier — and
    /// rejects such a method rather than mis-lowering it. See
    /// `vm::runtime::kernels`.
    #[allow(clippy::too_many_arguments)]
    fn gpu_dispatch_gemm(
        &mut self,
        _half: bool,
        _a_handle: u64,
        _b_handle: u64,
        _c_handle: u64,
        _m: i32,
        _n: i32,
        _k: i32,
        _trans_a: bool,
        _trans_b: bool,
        _stream_handle: Option<u64>,
    ) -> Option<u64> {
        None
    }

    /// GpuStream affinity — mint a new Java-visible CUDA stream on the
    /// per-VM default-ordinal `OffloadCache`.
    ///
    /// Called by `Native.newStream` (wraps the returned handle in a
    /// `GpuStreamImpl`) and, lazily, by every `submit`/`launch`/
    /// `submitMethod`/`submitWithArg(s)` handler the first time a given
    /// `GpuExecutor` handle is used — see
    /// `native-builtins/src/craton_gpu.rs::resolve_or_create_default_stream`.
    /// That laziness is what gives an executor a real *default* stream:
    /// every dispatch through the same executor handle reuses the one
    /// stream minted on its first submit, instead of each call getting
    /// its own private one-shot stream (the gap
    /// `docs/gpu/async-api.md` describes under "GpuStream affinity is
    /// not wired up").
    ///
    /// Returns `None` when there is no device (no driver / `--gpu` off
    /// / `gpu-offload` compiled off on the VM side) — the default impl
    /// here, matching every other no-driver fallback in this trait.
    /// The VM's `NativeContextImpl` overrides under
    /// `#[cfg(feature = "gpu-offload")]` to call
    /// `runtime::offload::OffloadCache::stream_create`.
    fn gpu_stream_create(&mut self) -> Option<u64> {
        None
    }

    /// Begin recording dispatches on `stream_handle` into a graph instead
    /// of running them.
    ///
    /// A launch costs the host whether or not the device is busy, and a
    /// loop issuing hundreds of small launches per unit of work pays that
    /// hundreds of times. Recording them once and replaying the recording
    /// removes the per-launch host cost entirely.
    ///
    /// Default impl answers `false` (no GPU offload). The VM override
    /// calls `runtime::offload::OffloadCache::graph_begin_capture`.
    fn gpu_graph_begin_capture(&mut self, _stream_handle: u64) -> bool {
        false
    }

    /// Stop recording and instantiate. Answers a graph handle, or `0` when
    /// there was no capture or it was invalidated.
    fn gpu_graph_end_capture(&mut self, _stream_handle: u64) -> u64 {
        0
    }

    /// Submit every launch a graph recorded, onto `stream_handle`.
    ///
    /// Answers a submission handle, awaited and released exactly like a
    /// dispatch's, or `0` if the replay was refused. A replay is
    /// asynchronous; a caller that reads a result without awaiting the
    /// handle reads what was there before.
    fn gpu_graph_replay(&mut self, _stream_handle: u64, _graph_handle: u64) -> u64 {
        0
    }

    /// Open an argument-update pass over a captured graph: the caller
    /// re-issues its dispatch sequence and each dispatch rewrites the
    /// arguments of the node it corresponds to, instead of launching.
    fn gpu_graph_begin_replay(&mut self, _stream_handle: u64, _graph_handle: u64) -> bool {
        false
    }

    /// Close the pass and submit the graph once. Answers a submission
    /// handle, or `0` if the caller's sequence did not match the one
    /// that was captured.
    fn gpu_graph_end_replay(&mut self, _stream_handle: u64) -> u64 {
        0
    }

    /// How many nodes a graph holds, or `-1` for an unknown handle. The
    /// count a caller checks against the dispatches it made while
    /// capturing: a graph with fewer nodes replays successfully and does
    /// less.
    fn gpu_graph_node_count(&self, _graph_handle: u64) -> i32 {
        -1
    }

    /// Free a graph. Idempotent, like every other release in this trait.
    fn gpu_graph_release(&mut self, _graph_handle: u64) {}

    /// Release a stream minted by [`gpu_stream_create`](Self::gpu_stream_create).
    /// Safe to call on an unknown or already-released `handle`
    /// (no-op) — same idempotent-release convention as
    /// [`gpu_release_array_cache`](Self::gpu_release_array_cache).
    ///
    /// Default impl is a no-op (no GPU offload). The VM override calls
    /// `runtime::offload::OffloadCache::stream_release`.
    fn gpu_stream_release(&mut self, _handle: u64) {}

    /// Synchronize a stream minted by [`gpu_stream_create`](Self::gpu_stream_create),
    /// blocking until all prior work submitted to that stream has completed.
    ///
    /// Default impl is a no-op (no GPU offload).
    fn gpu_stream_synchronize(&mut self, _handle: u64) -> Result<(), String> {
        Ok(())
    }

    /// Promote a completed ephemeral `DeviceArray` future result to a resident
    /// `GpuArray` registered under `new_array_handle` without host round-trip.
    /// Returns `Some((elem, len))` on success, `None` if unavailable or already claimed.
    fn gpu_promote_ephemeral_array(
        &mut self,
        _future_handle: u64,
        _new_array_handle: u64,
    ) -> Option<(DeviceArrayElem, usize)> {
        None
    }

    /// Stream-affine sibling of [`gpu_dispatch_method`](Self::gpu_dispatch_method):
    /// identical contract, plus `stream_handle`.
    ///
    /// * `Some(h)` — pin this dispatch onto the CUDA stream previously
    ///   minted by [`gpu_stream_create`](Self::gpu_stream_create) under
    ///   handle `h`. Two dispatches pinned to the SAME `h` serialize in
    ///   submission order (the ordering guarantee a CUDA stream gives
    ///   for free). An `h` that was never minted, or was already
    ///   released via [`gpu_stream_release`](Self::gpu_stream_release),
    ///   is a hard failure (a `Failed` submission), not a silent
    ///   fresh-stream fallback.
    /// * `None` — identical to calling
    ///   [`gpu_dispatch_method`](Self::gpu_dispatch_method) directly: a
    ///   fresh, private, one-shot stream for this dispatch alone.
    ///
    /// Default impl delegates to `gpu_dispatch_method` and ignores
    /// `stream_handle` — correct for every mock/test context (no GPU
    /// offload at all) and for a VM build with `gpu-offload` off. The
    /// VM's `NativeContextImpl` overrides under
    /// `#[cfg(feature = "gpu-offload")]` to call
    /// `runtime::offload::dispatch_method_from_native_on_stream`.
    fn gpu_dispatch_method_on_stream(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        java_args: &[Value],
        _stream_handle: Option<u64>,
    ) -> Option<u64> {
        self.gpu_dispatch_method(class_name, method_name, descriptor, java_args)
    }

    /// Phase 6 #4 — query the real GPU submission registry for the
    /// future at `handle`. Returns:
    ///   * `Some(0)` — Running
    ///   * `Some(1)` — Completed
    ///   * `Some(2)` — Failed
    ///   * `None`    — the handle is not in the real registry
    ///                 (caller should fall back to the synthetic
    ///                 future state in native-builtins).
    /// Default impl returns None (no GPU offload).
    fn gpu_future_status(&self, _handle: u64) -> Option<i32> {
        None
    }

    /// 2026-07-11 — take the real GPU submission's completed result for
    /// `handle`, without blocking. This is the read half `gpu_future_status`
    /// was missing: `gpu_future_status` (now backed by
    /// `runtime::offload::poll_submission_status`) tells a caller *that*
    /// a submission finished; this method hands back *what it produced*.
    ///
    /// Returns:
    ///   * `Some(GpuFutureResult::Scalar*)` — the submission is complete
    ///     and its kernel returned a scalar (`)I`/`)J`/`)F`/`)D`).
    ///   * `Some(GpuFutureResult::Void)` — the submission is complete and
    ///     either the kernel had a void return, or it wrote its result
    ///     into a caller-owned primitive array rather than the future's
    ///     result slot (array results are delivered via writeback into
    ///     the caller's own arrays, not through the future — see
    ///     [`GpuFutureResult`]'s doc comment).
    ///   * `None` — the handle is not in the real registry, the
    ///     submission is still `Running`, or it failed. This method
    ///     never blocks and never finalizes-and-waits on the caller's
    ///     behalf beyond what an already-observed completion allows: a
    ///     caller that hasn't first seen `gpu_future_status`/
    ///     `futureIsDone` report completion should treat `None` here as
    ///     "not ready yet" and fall back to the blocking
    ///     `gpu_future_synchronize` path, not as a permanent failure.
    ///
    /// Default impl returns `None` (no GPU offload).
    fn gpu_future_take_result(&self, _handle: u64) -> Option<GpuFutureResult> {
        None
    }

    /// Phase 6 #4 — block until the real GPU submission at `handle`
    /// completes (via its recorded event). Returns:
    ///   * `Some(Ok(()))`    — completed
    ///   * `Some(Err(msg))`  — submission failed; `msg` carries the reason
    ///   * `None`            — handle not in the real registry
    /// Default impl returns None.
    fn gpu_future_synchronize(&self, _handle: u64) -> Option<Result<(), String>> {
        None
    }

    /// Why the submission at `handle` failed, as recorded at the point of
    /// failure. Returns `None` when the handle is not in the real
    /// registry or the submission did not fail.
    ///
    /// Backs `Native.futureErrorKind`. Without it the Java side can only
    /// guess the `GpuException` subclass from the driver's message text.
    ///
    /// Default impl returns `None` (no GPU offload).
    fn gpu_future_error_kind(&self, _handle: u64) -> Option<GpuErrorKind> {
        None
    }

    /// Block until the submission at `handle` completes or
    /// `timeout_nanos` elapses.
    ///
    /// Returns one of [`GPU_AWAIT_COMPLETED`], [`GPU_AWAIT_TIMED_OUT`], or
    /// [`GPU_AWAIT_UNSUPPORTED`]. The default is `GPU_AWAIT_UNSUPPORTED`,
    /// which tells the caller to fall back to the polling path it already
    /// has — so a VM without this override behaves exactly as before.
    ///
    /// The point of implementing it here rather than in Java is that the
    /// Java fallback costs one native crossing per poll and floors its
    /// latency at the poll interval. A native implementation waits on the
    /// device's own completion signal.
    fn gpu_future_await(&self, _handle: u64, _timeout_nanos: u64) -> i32 {
        GPU_AWAIT_UNSUPPORTED
    }

    /// Phase 8 #1 — evict the device-side buffer cache entry for
    /// the given `GpuArray` handle. Called by
    /// `Native.releaseArray` so a long-running Java program that
    /// churns through GpuArrays doesn't accumulate device memory.
    ///
    /// Default impl is a no-op (no GPU offload). The VM override
    /// calls `runtime::offload::device_cache::release(handle)`.
    /// Drop the offload runtime's registry entry for one async
    /// submission handle.
    ///
    /// The handle is the same one `gpu_future_synchronize` /
    /// `gpu_future_await` take: the Java "future handle" IS the offload
    /// submission handle. Default no-op so a host without the offload
    /// runtime (or a `gpu-offload`-less build) needs no arm.
    ///
    /// Added 2026-09-02. `offload::SUBMISSIONS` had one insert and one
    /// remove, and the remove had no production caller -- no Java
    /// program, however correctly written, could drain the registry,
    /// because `Native.releaseFuture` only removed from
    /// `native-builtins`' own `state::futures` map. This is the missing
    /// half of that path.
    fn gpu_release_submission(&mut self, _handle: u64) {}

    fn gpu_release_array_cache(&mut self, _handle: u64) {}

    /// Phase 10 #1 — wipe the explicit-submit input-residency cache.
    /// Called by `Native.releaseExecutor` so the device buffers
    /// cached by plain `int[]` / `long[]` / `float[]` / `double[]`
    /// args to `submitMethod` are freed when the Java
    /// `GpuExecutor` is closed.
    ///
    /// Default impl is a no-op (no GPU offload). The VM override
    /// calls `runtime::offload::input_cache::clear_all()`.
    fn gpu_clear_input_cache(&mut self) {}

    /// Phase 9 #1 — materialise the device-side buffer's contents
    /// into host bytes if (and only if) the cache entry is dirty
    /// from a prior kernel's writes. Returns
    /// `Some(little-endian-bytes)` when a download happened (and
    /// the caller should write them into the resident store
    /// before reading the Java array), or `None` when the entry
    /// is unknown or clean (host bytes are already current).
    ///
    /// Default impl returns None (no GPU offload). The VM
    /// override calls
    /// `runtime::offload::device_cache::download_into_bytes_if_dirty(handle)`.
    /// Write little-endian host `bytes` into the device buffer that
    /// backs `handle`, keeping its device pointer.
    ///
    /// The upload counterpart of
    /// [`gpu_array_download_if_dirty`](Self::gpu_array_download_if_dirty),
    /// and the one thing a captured CUDA graph needs from the host
    /// between replays: a graph node holds the pointer it was captured
    /// with, so new input has to be written through it.
    ///
    /// `Some(true)` written, `Some(false)` refused (wrong size, or the
    /// copy failed), `None` no device buffer for this handle yet -- in
    /// which case the caller's host-side store is the only copy and
    /// updating it is sufficient. Default impl answers `None` (no GPU
    /// offload); the VM override calls
    /// `runtime::offload::device_cache::upload_from_bytes`.
    fn gpu_array_upload_bytes(&self, _handle: u64, _bytes: &[u8]) -> Option<bool> {
        None
    }

    fn gpu_array_download_if_dirty(&self, _handle: u64) -> Option<Vec<u8>> {
        None
    }

    /// GPU device enumeration escape hatch.
    ///
    /// Returns one entry per attached CUDA device, in ordinal order:
    /// `(name, compute_major, compute_minor, total_global_mem_bytes)`.
    ///
    /// The default impl returns an empty `Vec` — the truthful answer on
    /// a build without the `gpu-offload` feature, or on a host with no
    /// CUDA driver. The VM's `NativeContextImpl` overrides this under
    /// `#[cfg(feature = "gpu-offload")]` to call [`cuda_bridge::probe`].
    /// Because `probe()` itself returns `Err(NoDriver)` on a driverless
    /// host (or when `cuda-bridge` was built in stub mode), the override
    /// likewise yields an empty `Vec` there — `deviceCount()` honestly
    /// reports `0` rather than pretending a device exists.
    ///
    /// `cuda_bridge::probe` currently reports only the primary device
    /// (ordinal 0); the contract here is general (a `Vec`) so a future
    /// multi-device probe needs no signature change.
    fn gpu_device_info(&self) -> Vec<(String, u32, u32, u64)> {
        Vec::new()
    }

    /// Phase 6 #5 — resolve a `GpuCallable` / `GpuRunnable` /
    /// `GpuFunction` lambda's target method.
    ///
    /// When the user writes
    ///
    /// ```ignore
    /// executor.submit(() -> Pipeline.vectorAdd(a, b, out));
    /// ```
    ///
    /// the lambda is materialised as a proxy object whose class is
    /// recorded in `shared.classes.lambda_proxies`. This method looks the
    /// proxy up and returns
    /// `Some((target_class, target_method, target_descriptor,
    ///        captured_values))` so the dispatcher can route through
    /// `gpu_dispatch_method`.
    ///
    /// Returns `None` for any of:
    ///   * `callable` is not a lambda proxy
    ///   * the impl method handle is not InvokeStatic (instance
    ///     methods cannot run on the GPU)
    ///   * gpu-offload feature is off (default impl)
    fn gpu_resolve_lambda_target(
        &self,
        _callable: ObjectRef,
    ) -> Option<(String, String, String, Vec<Value>)> {
        None
    }

    /// Phase 3 — chain a downstream GPU kernel submission directly onto a
    /// predecessor submission handle's CUDA stream without blocking on host
    /// `.get()` synchronization.
    ///
    /// Returns `Ok(chained_submission_handle)` on enqueue, or `Err(reason)` if
    /// chaining is unavailable or fails. Default impl returns
    /// `Err("unsupported".to_string())` (no GPU offload).
    fn gpu_chain_submission(
        &self,
        _pred_handle: u64,
        _class_name: &str,
        _method_name: &str,
        _method_desc: &str,
        _args: &[Value],
    ) -> Result<u64, String> {
        Err("unsupported".to_string())
    }

    /// Release an ephemeral (non-user-owned) device array handle after kernel
    /// enqueue or host materialization. Implementations keep user-owned
    /// `GpuArray` handles intact and ignore unknown or `0` handles.
    ///
    /// Default impl is a no-op (no GPU offload).
    fn gpu_release_ephemeral_array_after_enqueue(&self, _array_handle: u64) {}

    /// Return the CUDA stream handle bound to `submission_handle`, if known.
    ///
    /// Default impl returns `None` (no GPU offload).
    fn gpu_submission_stream_handle(&self, _submission_handle: u64) -> Option<u64> {
        None
    }

    /// Estimate off-heap VRAM (bytes) currently retained by a GPU submission's
    /// ephemeral output buffer, if any. Default impl returns `0`.
    fn gpu_submission_retained_vram(&self, _submission_handle: u64) -> u64 {
        0
    }

    /// Account for a change in off-heap GPU VRAM retained by live Java
    /// `GpuArray`/`GpuFuture` wrappers (`+` on wrap/allocate/submit, `-` on release).
    ///
    /// Returns `true` when external allocation pressure crosses ZGC/G1 pacing
    /// thresholds and arms the native-boundary pressure latch.
    fn gpu_note_retained_vram(&mut self, _delta_bytes: i64, _wrapper_delta: i64) -> bool {
        false
    }
}

/// JIT round 21 (lane staticcas): the comparison of a static-field CAS
/// ([`NativeSystemAccess::compare_and_exchange_static_field`]): references by
/// identity, primitives by bit pattern (a `float` / `double` too, as HotSpot's
/// `LOCK CMPXCHG` compares them), and the JVM null equal to a typed zero
/// (the coercion the `Unsafe` natives' `unsafe_cas_values_equal` has always
/// made for a slot that was never typed).
pub fn static_cas_values_equal(cur: Value, expected: Value) -> bool {
    match (cur, expected) {
        (Value::Object(a), Value::Object(b)) => a == b,
        (Value::Object(None), Value::Int(0) | Value::Long(0))
        | (Value::Int(0) | Value::Long(0), Value::Object(None)) => true,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
        _ => false,
    }
}

pub trait NativeSystemAccess: NativeThreadAccess {
    /// Per-VM -Xrs policy; the launcher and Java signal registration share it.
    fn reduce_signal_usage(&self) -> bool {
        false
    }

    /// Capability boundary: Process, VM, I/O, FFI, metrics, and diagnostics.

    /// Whether this context can construct and dispatch real generated proxy
    /// classes. Lightweight unit-test contexts intentionally return `false`:
    /// they model native object state but do not own a VM-wide class-loader and
    /// proxy-class namespace.
    fn supports_real_proxy_generation(&self) -> bool {
        true
    }

    /// Force-refresh this thread's deposited GC root snapshot (the same
    /// mechanism `NativeContextImpl::deposit_root_snapshot` uses before a
    /// blocking call) without actually blocking.
    ///
    /// Background: a peer-initiated stop-the-world collection has two ways
    /// to see a thread's roots — (1) a live conservative register/stack scan
    /// if that thread is forcibly frozen while executing JIT-compiled code
    /// (`jit::xt_root_scan`), which does NOT know about `native_pin_roots`
    /// (a native-side `Vec` living on the Rust heap, not the JIT frame), or
    /// (2) this thread's last-deposited snapshot
    /// (`collect_all_root_snapshots`/`root_snapshots_for_os_tids`), which
    /// DOES include `native_pin_roots` but is only refreshed at specific
    /// checkpoints: a cooperative interpreter safepoint arrival, entry into
    /// a blocking native region, or this thread itself initiating a GC.
    /// JIT-compiled code has no periodic cooperative safepoint poll at all
    /// (see the comment on `jit::helpers::jit_safepoint_flush_satb`) — it
    /// only touches those checkpoints via specific GC-triggering runtime
    /// helpers, which a hot loop making only fast-path allocations may never
    /// call.
    ///
    /// A native method that pins a long-lived batch of objects (e.g. a
    /// materialized `Stream` of elements, each pinned once up front) and then
    /// drives per-element re-entrant Java execution that can run for a long
    /// time and/or tier up into JIT — without itself ever blocking or
    /// initiating GC — leaves a window where neither mechanism above sees
    /// those pins: not (1), because `native_pin_roots` isn't scanned that
    /// way, and not (2), because nothing has refreshed the deposit since
    /// before the pins were pushed. A peer thread's GC during that window
    /// can reclaim a still-pinned object; the next read through the pin
    /// (correctly re-validated, `via_pin=true`) observes a stale/reused
    /// address. Confirmed live for JUnit 5's `TestTemplateExecutor`/
    /// `ParameterizedTestExtension` dynamic-test dispatch (`ClassCastException:
    /// java.lang.Object cannot be cast to
    /// org.junit.jupiter.api.extension.TestTemplateInvocationContext`,
    /// `obj_cid=0` — see `wildfly-standalone-boot-attributeaccess-cce-register-invisible-root-RETIRED.md`,
    /// which documents the same family from WildFly's `parallel-extension-add`
    /// boot step) — one more independent occurrence of that already-tracked
    /// "register-invisible root" / cross-thread GC-root-visibility family,
    /// now with this specific closeable checkpoint gap identified.
    ///
    /// Calling this right after establishing such a batch of pins (and
    /// optionally again periodically across a long per-element loop) closes
    /// that window by (re-)publishing a fresh deposit — the exact same
    /// mechanism already relied on for peers parked in a blocking region —
    /// without requiring this thread to actually block. Default impl is a
    /// no-op: test/mock contexts have no cross-thread GC to defend against.
    fn refresh_root_snapshot(&mut self) {}

    /// ES-FAIL-FAMILY-20260710 hunt: arm the GC's dynamic software
    /// write-watchpoint (see `cratonvm_gc::heap::set_dynamic_watch`) at a
    /// raw heap address, so any subsequent write through an instrumented
    /// heap write primitive that covers this address prints its call site.
    /// `addr = 0` disarms. Default no-op so mock/test `NativeContext` impls
    /// don't need to implement it; only the real VM's impl (which has a
    /// live heap to watch) overrides it.
    fn dbg_set_watch_cell(&mut self, _addr: usize) {}

    /// zt t9/pl: `(address, length)` of this VM's own jvmstat perf-data
    /// memory, for `jdk.internal.perf.Perf.attach0(0)`, which HotSpot answers
    /// with a direct buffer over it (`PerfMemory::attach`); the memory stays
    /// mapped for the rest of the process once handed out. `None` when the
    /// VM published no perf data (`-XX:-UsePerfData`,
    /// `CRATONVM_PERF_DATA_FILE=0`) or under `CRATONVM_PERF_ATTACH_SELF=0`:
    /// the native then answers a zero-capacity buffer, as HotSpot does
    /// without perf memory. Default `None` for mock contexts.
    fn attach_own_perf_data(&mut self) -> Option<(usize, usize)> {
        None
    }

    /// zt t10/pl2: `(address, length)` of ANOTHER VM's perf-data memory,
    /// mapped read-only, for `Perf.attach0(<lvmid>)` with an id that is not
    /// this VM's (HotSpot maps that VM's perf memory). The VM records the
    /// view; [`Self::detach_perf_data`] releases it. `None` when no such VM
    /// published perf data for this user, or under
    /// `CRATONVM_PERF_ATTACH_FOREIGN=0`. Default `None` for mock contexts.
    fn attach_foreign_perf_data(&mut self, _lvmid: i32) -> Option<(usize, usize)> {
        None
    }

    /// zt t14/pl5: after [`Self::attach_foreign_perf_data`] mapped nothing
    /// for `lvmid`, the message of the `IllegalArgumentException` HotSpot's
    /// `PerfMemory::attach` throws when it finds no perf data for that id
    /// (`Could not map vmid to user name`); `None` keeps the zero-capacity
    /// answer (the data exists but could not be mapped, or
    /// `CRATONVM_PERF_ATTACH_MISSING_THROWS=0` / `CRATONVM_PERF_ATTACH_FOREIGN=0`).
    /// Default `None` for mock contexts.
    fn foreign_perf_data_missing(&mut self, _lvmid: i32) -> Option<&'static str> {
        None
    }

    /// zt pl4 item 5: `Perf.attach0(<lvmid>)` of another VM by HotSpot 25's
    /// own lookup (`perf_data_file::attach_foreign_hotspot` on POSIX): the
    /// recorded view, `Ok(None)` for a zero-capacity buffer, or the exception
    /// HotSpot throws. `None`: the context does not do it (another OS, a
    /// mock, `CRATONVM_PERF_ATTACH_HOTSPOT_LOOKUP=0`); the caller takes
    /// [`Self::attach_foreign_perf_data`] and
    /// [`Self::foreign_perf_data_missing`]. Default `None`.
    fn attach_foreign_perf_data_hotspot(
        &mut self,
        _lvmid: i32,
    ) -> Option<Result<Option<(usize, usize)>, ForeignPerfAttachFailure>> {
        None
    }

    /// zt t10/pl2: `Perf.detach(buffer)` for the buffer at `address`:
    /// unmaps a view [`Self::attach_foreign_perf_data`] made; anything else
    /// (this VM's own perf data, a zero-capacity buffer) is left alone.
    /// Default no-op for mock contexts.
    fn detach_perf_data(&mut self, _address: usize) {}

    /// Instance VarHandle field metadata has become available.
    /// Called after releasing its native metadata lock. The VM can now
    /// refresh initialized static-final holders on this mutator; mock contexts
    /// have no optimizing compiler. No Java allocation or invocation is owed.
    fn note_varhandle_field_resolution(&mut self) {}

    /// Stable identity for the owning VM/heap.
    ///
    /// Native side caches that store heap `ObjectRef`s must scope entries to
    /// this value; Rust tests can create multiple independent `Vm` instances in
    /// one process, so process-global object caches are otherwise stale across
    /// VM lifetimes. Mock contexts default to a single synthetic scope.
    fn vm_identity(&self) -> usize {
        0
    }

    /// Whether `internal` (slash form) names a class made loadable by a jar or
    /// directory appended at runtime to THIS VM's bootstrap search
    /// (`Instrumentation.appendToBootstrapClassLoaderSearch`,
    /// `-Xbootclasspath/a:`). The VM answers from its `ClassManager`'s census
    /// under the class-manager read lock, so a caller must not hold that lock
    /// (no native can). Mock contexts have no appended search: `false`.
    ///
    /// Round 14 wave 2 (MISC11-2): the census was the process static
    /// `BOOTSTRAP_APPENDED_CLASSES` keyed by `vm_identity`, read through the
    /// free function `cratonvm_classloading::is_bootstrap_appended_class`.
    fn is_bootstrap_appended_class(&self, _internal: &str) -> bool {
        false
    }
    /// The bootstrap-search entry (its path as appended) of a package of
    /// classes appended to that search, once the boot loader has defined a
    /// class of it (`ClassManager::boot_append_package_location`; interpreter
    /// round i2 wave 57, lane L5). Mock contexts: `None`.
    fn boot_append_package_location(&self, _package: &str) -> Option<String> {
        None
    }

    /// The appended packages the boot loader has defined a class in
    /// (`ClassManager::boot_append_loaded_packages`; interpreter round i2
    /// wave 57, lane L5). Mock contexts: none.
    fn boot_append_loaded_packages(&self) -> Vec<String> {
        Vec::new()
    }

    /// The VM's options as HotSpot's `Arguments::jvm_args_array()` holds them
    /// (`VmConfig::jvm_args`): what `VMManagementImpl.getVmArguments0` and so
    /// `RuntimeMXBean.getInputArguments()` answer (interpreter round i4 wave
    /// 85, lane L3). Mock contexts and embedders that record none: empty.
    fn jvm_input_arguments(&self) -> Vec<String> {
        Vec::new()
    }

    /// The owning VM's assertion switches (`VmConfig::assertion_directives`):
    /// what `Class.desiredAssertionStatus0` and `ClassLoader.retrieveDirectives`
    /// answer from (interpreter round i5 wave 107, lane L5). `None` -- mock
    /// contexts and embedders that pass no `-ea` options -- leaves the user
    /// default to `CRATONVM_ENABLE_ASSERTIONS` and system classes off.
    fn assertion_directives(&self) -> Option<&crate::assertion_directives::AssertionDirectives> {
        None
    }

    /// The first byte of the declared descriptor of `obj`'s INSTANCE field at
    /// heap slot `index` (`B C D F I J S Z L [`), from the VM's per-class cache
    /// the descriptor-aware `get_field` already uses; `None` for an unknown
    /// slot (padding, a synthetic carrier) or an unvalidated reference. The
    /// `Unsafe` field view decodes a mixed-width access with it (interpreter
    /// round i4 wave 87, lane L3). Mock contexts: `None`. JIT round 20 wave 8
    /// (lane huntDisp) reads it too: an `Unsafe` store NARROWER than its field
    /// replaces only the field's low bytes, as HotSpot's byte-addressed store
    /// does (`unsafe_natives_ext::unsafe_narrow_field_store`); `None` keeps the
    /// whole-slot store.
    fn instance_field_descriptor_byte(&self, _obj: ObjectRef, _index: usize) -> Option<u8> {
        None
    }

    /// `NativeMethodRegistry::boot_append_package_table`: the boot loader's
    /// package stand-ins answer appended packages from the table recorded at
    /// append (interpreter round i2 wave 57, lane L5). Default `false`.
    fn boot_append_package_table(&self) -> bool {
        false
    }

    /// The instant the owning VM was created: the origin of its
    /// `RuntimeMXBean.getStartTime()` / `getUptime()` and of its `GcInfo`
    /// start and end times (gc-common w17-e,
    /// `common-w16f-jmm-switches-and-start-time-are-process-wide`). Several
    /// VMs share one process (an embedder, the `vm` test binary), so a
    /// process-wide origin gives every VM after the first another VM's start.
    ///
    /// `None` -- the default, for mock contexts and any context that does not
    /// know -- makes the management natives fall back to one process-wide
    /// origin, which is what they used before this accessor existed.
    fn vm_start_instant(&self) -> Option<std::time::Instant> {
        None
    }

    /// Whether the owning VM was launched with `-verbose:gc`
    /// (`VmConfig::verbose_gc`): what `MemoryMXBean.isVerbose()` reports
    /// before any `setVerbose` (gc-common w18-g,
    /// `common-w17e-verbose-gc-launch-flag-invisible-to-memorymxbean`). HotSpot
    /// answers from the `gc` log level, which `-verbose:gc` sets.
    ///
    /// `false` -- the default, for mock contexts -- is HotSpot's answer for a
    /// launch without the flag.
    fn verbose_gc_at_launch(&self) -> bool {
        false
    }

    /// Turn the owning VM's GC logging on or off: `MemoryMXBean.setVerbose` /
    /// `MemoryImpl.setVerboseGC`. HotSpot starts `gc` logging at `info` on
    /// `true` and stops it on `false`, whatever the launch flags were
    /// (gc-common w18-g). The default does nothing: a mock has no GC log.
    fn set_verbose_gc(&self, on: bool) {
        let _ = on;
    }

    /// Store a value into the test output buffer (for `tempPrint`).
    fn record_printed_value(&mut self, value: Value);

    /// VM-accelerated primitive-wrapper recognition. `None` means this context
    /// does not implement the fast path; `Some(None)` means the object is not a
    /// wrapper; `Some(Some(value))` is the unboxed primitive.
    fn fast_unbox_primitive_wrapper(&self, _obj: ObjectRef) -> Option<Option<Value>> {
        None
    }

    /// [`class_id_of_object`](NativeClassAccess::class_id_of_object) and
    /// [`fast_unbox_primitive_wrapper`](Self::fast_unbox_primitive_wrapper) for
    /// a caller that wants BOTH answers about the same object.
    ///
    /// # why
    ///
    /// Because the second already reads the first. Every checked heap access
    /// in this API opens with an `is_object_address` membership probe and a
    /// forwarding barrier, and a caller that asks the two questions separately
    /// pays that pair twice to learn one class id. The map key comparisons do
    /// exactly that -- `chm_wrapper_chain_walk` asks for a node key's class to
    /// screen it against the lookup key's, then unboxes it, and
    /// `chm_wrapper_key_hash` + `chm_get_wrapper_key_fast` do the same to the
    /// lookup key itself.
    ///
    /// MEASURED (2026-09-20 dev build, `ChmProbe get`, 5 M
    /// `ConcurrentHashMap<Integer,Integer>.get`): 11.77 % of samples in
    /// `ZgcRealHeap::is_object_address` and 3.54 % in the `VmHeap` layer above
    /// it, spread over roughly fifteen checked accesses per lookup. This is the
    /// same "validate the receiver ONCE per call" direction
    /// `perf-collection-natives-cost-a-generic-native-dispatch-per-call`'s
    /// proposed fix 2 asks for, applied where two questions share one walk.
    ///
    /// The default implementation is the two calls, in that order, so an
    /// implementor with no fused path needs to do nothing and the answer is
    /// identical either way.
    /// `NativeClassAccess` is a supertrait of this one (through
    /// `NativeHeapAccess: NativeInvokeAccess: NativeClassAccess`), so this
    /// needs no `where` clause and stays dispatchable through
    /// `&dyn NativeContext` -- which is how every collection native holds its
    /// context.
    fn class_id_and_unboxed_primitive(
        &self,
        obj: ObjectRef,
    ) -> (crate::ClassId, Option<Option<Value>>) {
        (
            self.class_id_of_object(obj),
            self.fast_unbox_primitive_wrapper(obj),
        )
    }

    /// Whether the current Java execution stack already contains the exact
    /// instance method on `receiver`.
    ///
    /// Native shadows occasionally need to distinguish a native-first virtual
    /// entry from an `invokespecial` delegation made by real bytecode already
    /// executing in an override. The default is deliberately conservative for
    /// lightweight test contexts, which do not own a live Java frame stack.
    fn is_executing_instance_method(
        &self,
        _receiver: ObjectRef,
        _method_name: &str,
        _descriptor: &str,
    ) -> bool {
        false
    }

    /// Read `out.len()` bytes of native memory at `addr` into `out`.
    ///
    /// `addr` may be either a real OS pointer (e.g. a mapped buffer) OR one of
    /// the VM's `Unsafe.allocateMemory` arena handles (a synthetic high
    /// address, base `0x10_0000_0000`, NOT a dereferenceable pointer). NIO
    /// native dispatchers (`sun/nio/ch/Net.read0`, `SocketDispatcher` …) that
    /// receive a `DirectByteBuffer.address()` MUST go through this instead of a
    /// raw `copy_nonoverlapping`, because `Util.getTemporaryDirectBuffer` backs
    /// its temp buffers with arena handles — dereferencing one raw SIGSEGVs.
    ///
    /// The default implementation fails closed. Implementations that can prove
    /// the address range is valid must override this and perform their own
    /// pointer/arena validation before copying.
    fn copy_from_native_memory(&self, _addr: i64, _out: &mut [u8]) -> bool {
        false
    }

    /// Write `data` to native memory at `addr`. See [`Self::copy_from_native_memory`]
    /// for the arena-handle vs raw-pointer distinction.
    fn copy_to_native_memory(&mut self, _addr: i64, _data: &[u8]) -> bool {
        false
    }

    /// Is `addr` one of the VM's `Unsafe`-arena handles rather than a real,
    /// dereferenceable OS pointer?
    ///
    /// This is the distinction [`Self::copy_from_native_memory`] already makes
    /// internally, surfaced so a caller can *report* which population it is
    /// serving without attempting a transfer. `cratonvm-native-io`'s socket
    /// census uses it to answer a question that decides whether a whole class
    /// of optimisation is reachable at all: the bounce buffer in the direct
    /// `ByteBuffer` transfer path is removable only for a REAL pointer, since
    /// an arena handle cannot be handed to the kernel. Measuring the split on a
    /// live HTTP workload is what tells you whether that work is worth doing —
    /// and the answer must come from a counter, not from an assumption about
    /// which allocator the application used.
    ///
    /// It exists on this trait rather than as a direct call into
    /// `cratonvm-native-builtins` (which owns the tag bit) because
    /// `native-builtins` DEPENDS on `native-io`; the reverse edge would be a
    /// dependency cycle. Duplicating the tag constant into `native-io` was the
    /// other option and is worse — a second copy of a magic number that must
    /// track the arena allocator's, with nothing to notice when it stops.
    ///
    /// The default answers `false` — "assume a real pointer". A context that
    /// does not model the arena has no handles to misreport, and the only
    /// consumer is diagnostic.
    fn native_addr_is_arena_handle(&self, _addr: i64) -> bool {
        false
    }

    /// [`Self::class_id_of_object`], but resolved through the read barrier the
    /// way [`Self::get_field_by_name`] resolves it.
    ///
    /// The two are NOT interchangeable for a caller that memoizes field slots.
    /// `class_id_of_object` validates the address and answers `ClassId(0)` when
    /// it is not a live object base — but if a moving collection relocated the
    /// object and something else has since been allocated at the old address,
    /// it answers the class of THAT object instead. Pairing such an id with
    /// [`Self::get_field`], which forwards internally, would read the new
    /// object's slot layout out of the forwarded original: a wrong field, in
    /// bounds, with nothing raised.
    ///
    /// `get_field_by_name` never has that problem because it forwards first and
    /// derives the id from the forwarded object. Any caller resolving an index
    /// once and reusing it must do the same, which is what this exists for.
    ///
    /// The default delegates, because a context with no moving collector has
    /// nothing to forward.
    fn class_id_of_object_forwarded(&self, obj: ObjectRef) -> ClassId {
        self.class_id_of_object(obj)
    }

    /// `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` for this VM (JIT round 20 wave 8c,
    /// lane huntDisp; opt-in, read once per VM): do `Unsafe` instance-field
    /// offsets encode `16 + slot * 8 + k` (byte `k` of the field at `slot`)
    /// instead of the slot index? Every native that answers an offset and
    /// every native that takes one asks this same answer
    /// (`native-builtins` `unsafe_encode_field_offset` / `unsafe_field_slot`).
    /// The default answers `false`: slot indices, as before.
    fn unsafe_byte_field_offsets(&self) -> bool {
        false
    }

    /// Record a printed line (for System.out.println capture in tests).
    fn record_printed_line(&mut self, text: String);

    /// Get a system stream object (stdout or stderr).
    fn get_system_stream(&self, name: &str) -> Option<ObjectRef>;

    /// Pin the canonical `System.in` object on the VM so natives and `GETSTATIC`
    /// agree after `initPhase1` allocates it. Default: no-op.
    fn cache_system_stdin(&mut self, _stream: ObjectRef) {}

    /// Look up the canonical `java.lang.Module` mirror for a module name
    /// (`None`/`Some("")` ⇒ the unnamed module). `Class.getModule()` MUST return
    /// the SAME instance for every class in a module — the JDK compares modules
    /// by identity (see `Throwable.validateSuppressedExceptionsList`, HIB-CV-29).
    /// Default: `None` (mock contexts have no persistent store).
    fn get_cached_module_mirror(&self, _module_name: Option<&str>) -> Option<ObjectRef> {
        None
    }

    /// Store the canonical `java.lang.Module` mirror for a module name so that
    /// subsequent `Class.getModule()` calls return the identical instance.
    /// The mirror is registered as a permanent GC root. Default: no-op.
    fn cache_module_mirror(&mut self, _module_name: Option<&str>, _module: ObjectRef) {}

    /// `--real-jdk` (SBR2 PUREMOD): fill `java.lang.Class.module` of every class
    /// that already has a mirror, from the module objects the boot layer cached
    /// (HotSpot's `java_lang_Class::fixup_module_field`). Mirrors made after
    /// this point get the field at creation. Returns how many it set. Default:
    /// no-op (mock contexts have no mirrors).
    fn fixup_class_module_fields(&mut self) -> usize {
        0
    }

    /// `--real-jdk`: record that the `java.lang.Module` `module` of a
    /// non-boot `ModuleLayer` holds each package of `packages` (internal,
    /// `/`-separated form) for the user-defined loader namespace `loader_ns`
    /// its `Module.loader` names -- HotSpot's per-loader package-to-module
    /// map, which `Module.defineModule0` fills. The module is kept as a
    /// permanent root, as a loader keeps its modules. A later record of the
    /// same `(loader_ns, package)` replaces the earlier one. Default: no-op.
    fn record_layer_module_packages(
        &mut self,
        _loader_ns: u32,
        _packages: &[String],
        _module: ObjectRef,
    ) {
    }

    /// The `java.lang.Module` [`Self::record_layer_module_packages`] recorded
    /// for `(loader_ns, package)` (`package` in internal form), or `None`.
    /// Default: `None`.
    fn layer_module_of_package(&self, _loader_ns: u32, _package: &str) -> Option<ObjectRef> {
        None
    }

    /// Whether any layer module package was recorded in this VM: the one
    /// test `Class.getModule()` makes before it computes a package name.
    /// Default: `false`.
    fn has_layer_module_packages(&self) -> bool {
        false
    }

    /// Get a system property by key.
    fn get_system_property(&self, key: &str) -> Option<String>;

    /// Snapshot every system property as a `(key, value)` list.  Used by
    /// `System.getProperties()` to materialise a populated Properties
    /// object when real-JDK's `System.props` static field is null.
    fn list_system_properties(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Was `key` given on the command line (`-Dkey=...`, `VmConfig::system_properties`)?
    /// Interpreter round i7 wave 139, lane L1b: `SystemProps$Raw.vmProperties()` forwards a
    /// VM-internal key only when the user set it, as HotSpot's `JVM_GetProperties` lists only
    /// `Arguments`' own keys and the `-D` ones. The default answers `true` (forward every key,
    /// the behaviour before wave 139) for mocks and out-of-tree contexts.
    fn is_command_line_system_property(&self, key: &str) -> bool {
        let _ = key;
        true
    }

    /// True iff the `CRATONVM_REAL` differential switch prefers real
    /// bytecode over a `SyntheticStub` native for `class_name` — what
    /// `vm::runtime::env_cache::real_bytecode_selector().prefers_real(..)`
    /// answers. `native-builtins` cannot depend on `vm` (the dependency runs
    /// the other way), so this is the `NativeContext` seam that lets a
    /// native ask the question without a circular crate edge.
    ///
    /// Exists because `java/util/Properties`'s native surface is retired
    /// (`RETIRED_SHADOW_PHASE3_TRIPLES`) on the precondition that
    /// `System.getProperties()`'s singleton carries a populated real `map`
    /// field before real bytecode can read it — true under `--real-jdk`
    /// (`native_system_get_properties_jdk_only`), but `CRATONVM_REAL=all`
    /// reaches the identical real-bytecode path in plain `Compatible` mode,
    /// where the singleton's `map` is still permanently null. See
    /// `jdk-only/the-system-properties-real-map-is-null-and-it-blocks-the-chm-retirement-20260909-RETIRED-20260921.md`.
    /// Default `false` (mock contexts have no differential switch).
    fn prefers_real_bytecode(&self, _class_name: &str) -> bool {
        false
    }

    /// True iff strict `--real-jdk` policy is in force for this VM
    /// (`CompatibilityMode::JdkOnly`).
    ///
    /// The seam for a native whose Rust-side REPRESENTATION of a JDK object is
    /// only readable by the natives that wrote it. Under `--real-jdk` real
    /// bytecode is authoritative and reads the object's real fields directly,
    /// so a representation that lives only in a side table is invisible to it.
    /// Default `false` (mock contexts are compatible-mode).
    fn is_jdk_only(&self) -> bool {
        false
    }

    /// How long the `--compatible` shutdown-hook runner waits for the hook
    /// threads, in milliseconds; `0` waits forever (HotSpot's answer). This
    /// VM's `VmConfig::shutdown_hook_timeout_ms`
    /// (`CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS`; per VM since interpreter round i5
    /// wave 102, lane L6). Default 0, the VM's default (WARNING
    /// interp-i5-hook-timeout-flip-20261015: was 30 000).
    fn shutdown_hook_timeout_ms(&self) -> u64 {
        0
    }

    /// Is the JDK's parallel-capable loader model in force: a loader class
    /// that never called `registerAsParallelCapable()` gets a null
    /// `parallelLockMap` and is locked on its own monitor around a load, and
    /// a re-entrant VM-initiated load through it is a `ClassCircularityError`.
    /// `--real-jdk`, or `--compatible` with `CRATONVM_COMPAT_PARALLEL_LOADERS`
    /// (`NativeMethodRegistry::compat_parallel_capable_loaders`; interpreter
    /// round i1 wave 48, lane L5). Default: [`Self::is_jdk_only`].
    fn faithful_loader_locks(&self) -> bool {
        self.is_jdk_only()
    }

    /// Does this VM behave as HotSpot for the `--compatible` row `a`
    /// (`cratonvm_types::hotspot_alignment`): `--real-jdk`, or `--compatible`
    /// with the row in the VM's set (`NativeMethodRegistry::aligned`;
    /// interpreter round i1 wave 50, lane L5). Default: [`Self::is_jdk_only`]
    /// (mock contexts are compatible-mode with no row).
    fn aligned(&self, a: cratonvm_types::HotSpotAlignment) -> bool {
        let _ = a;
        self.is_jdk_only()
    }

    /// Row `a` answered other than its row-off route would have: count it in
    /// the VM's alignment census (`ClassRealm::alignment_census`, the exit
    /// `ALIGNMENT census` line; interpreter round i1 wave 51, lane L5).
    /// Called only on the arm where the two answers differ. Default: no-op
    /// (mock contexts).
    fn note_alignment_divergence(&self, a: cratonvm_types::HotSpotAlignment) {
        let _ = a;
    }

    /// Does the `URLClassLoader` family run the JDK's own bodies:
    /// `--real-jdk`, or `--compatible` over a real JDK with the
    /// `JdkUrlClassLoaders` row (`NativeMethodRegistry::jdk_url_class_loaders`;
    /// interpreter round i1 wave 52, lane L5). Default: [`Self::is_jdk_only`].
    fn jdk_url_class_loaders(&self) -> bool {
        self.is_jdk_only()
    }

    /// `NativeMethodRegistry::builtin_loader_package_sync`: the built-in
    /// loaders' package listings first put every package of a class the VM
    /// defined for them into their own `packages` map (interpreter round i2
    /// wave 55, lane L5). Default `false`: an implementation without a real
    /// JDK has no such map.
    fn builtin_loader_package_sync(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::resource_streams_open_url`: resource streams
    /// are the JDK's `url.openStream()` (interpreter round i2 wave 56, lane
    /// L5). Default `false`.
    fn resource_streams_open_url(&self) -> bool {
        false
    }
    /// `NativeMethodRegistry::defined_class_resources_defining_loader`: a
    /// run-time defined class's own bytes answer a `.class` resource only for
    /// the VM's CGLIB names through the application loader that defined them
    /// (interpreter round i2 wave 57, lane L5). Default `false`.
    fn defined_class_resources_defining_loader(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::jdk_built_packages`: the application loader's
    /// `Package` for a class-path class is built by the JDK's
    /// `BuiltinClassLoader.definePackage(pn, man, url)` (interpreter round i2
    /// wave 58, lane L5). Default `false`.
    fn jdk_built_packages(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::reflect_class_access`: the served reflective
    /// doors (`Method.invoke`, `Constructor.newInstance`, `Class.newInstance`
    /// and `--compatible`'s `Field` accessors) ask `verifyMemberAccess`'s
    /// class-accessibility half by RUNTIME package (interpreter round i2 wave
    /// 59, lane L3). Default `false`.
    fn reflect_class_access(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::reflect_invoke_plan`
    /// (`CRATONVM_REFLECT_INVOKE_PLAN`): the VM's `Method` mints carry a
    /// per-VM invocation plan id in `slot`, which the served `Method.invoke`
    /// reads its caller-independent facts from (interpreter round i3 wave 74,
    /// lane L3). Default `false`.
    fn reflect_invoke_plan(&self) -> bool {
        false
    }

    /// This VM's slot for the served reflective doors' caches -- the
    /// `Method.invoke` plans and the `Field` door identity memo
    /// (`NativeMethodRegistry::reflect_door_cache_slot`), type-erased
    /// because their types live in `native-builtins`. `None` (mock
    /// contexts): nothing is cached, every call takes the by-name path.
    fn reflect_door_cache_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        None
    }

    /// `NativeMethodRegistry::field_door_ident_memo`
    /// (`CRATONVM_FIELD_DOOR_IDENT_MEMO`): the served `Field` value doors
    /// take their refusal identity's text from a per-VM memo instead of
    /// building it per call (interpreter round i3 wave 74, lane L3). Default
    /// `false`.
    fn field_door_ident_memo(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::stack_trace_origin_record`
    /// (`CRATONVM_STACK_TRACE_ORIGIN_RECORD`): the `StackTraceElement` fills
    /// read a class's origin fields from a per-VM record after the first
    /// element of that class (interpreter round i3 wave 75, lane L2).
    /// Default `false`.
    fn stack_trace_origin_record(&self) -> bool {
        false
    }

    /// This VM's slot for the frame-origin records
    /// (`NativeMethodRegistry::stack_trace_origin_record_slot`), type-erased
    /// because their type lives in `native-builtins`. `None` (mock
    /// contexts): nothing is recorded, every element is derived afresh.
    fn stack_trace_origin_record_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        None
    }

    /// `NativeMethodRegistry::stack_trace_fill_name_from_text`
    /// (`CRATONVM_STACK_TRACE_FILL_NAME_FROM_TEXT`): the `StackTraceElement[]`
    /// fills derive each frame's dotted class name from its captured internal
    /// name instead of resolving the class by name first (interpreter round
    /// i3 wave 76, lane L2). Default `false`.
    fn stack_trace_fill_name_from_text(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::compat_stack_trace_keeps_array`
    /// (`CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY`): the native
    /// `Throwable.getStackTrace()` keeps the array it built in `stackTrace`
    /// and answers copies (interpreter round i3 wave 77, lane L2). Default
    /// `false`.
    fn compat_stack_trace_keeps_array(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::stack_trace_fill_switches`: the
    /// `StackTraceElement` fill's five behaviour switches, read once per VM
    /// (interpreter round i3 wave 77, lane L2). A context with no registry
    /// (mocks) reads them from the environment on each call, with the same
    /// rule, so it answers what the process `OnceLock`s it replaced answered.
    fn stack_trace_fill_switches(&self) -> StackTraceFillSwitches {
        StackTraceFillSwitches::from_env()
    }

    /// `NativeMethodRegistry::reflect_invoke_settled`
    /// (`CRATONVM_REFLECT_INVOKE_SETTLED`): a planned static `Method.invoke`
    /// target outside `java/` and `jdk/` dispatches through
    /// [`NativeInvokeAccess::invoke_static_settling`] with the owner its plan
    /// remembers (interpreter round i3 wave 75, lane L3). Default `false`.
    fn reflect_invoke_settled(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::reflect_ctor_plan_per_vm`
    /// (`CRATONVM_REFLECT_CTOR_PLAN_PER_VM`): `Constructor.newInstance`'s
    /// plans live in [`Self::reflect_door_cache_slot`], and a planned public
    /// constructor skips the `exports` caller walk when it cannot refuse
    /// (interpreter round i3 wave 75, lane L3). Default `false`.
    fn reflect_ctor_plan_per_vm(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::reflect_door_word_batch`
    /// (`CRATONVM_REFLECT_DOOR_WORD_BATCH`): a planned `Method.invoke` /
    /// `Constructor.newInstance` reads its mirror words in one
    /// [`NativeHeapAccess::get_fields_by_name`] batch, builds one argument
    /// vector, and the boxing `Method.invoke` wrapper takes the return
    /// descriptor from the plan (interpreter round i3 wave 76, lane L3).
    /// Default `false`.
    fn reflect_door_word_batch(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::reflect_door_phase_stats`
    /// (`CRATONVM_REFLECT_DOOR_PHASE_STATS`): the served `Method.invoke` /
    /// `Constructor.newInstance` doors time their phases into this VM's
    /// reflect-door caches and print a per-phase average to stderr
    /// (interpreter round i3 wave 77, lane L3). Default `false`.
    fn reflect_door_phase_stats(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::reflect_invoke_plan_facts`
    /// (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`): a `Method` plan keeps its
    /// declaring class's access flags, and an exact-class receiver of a
    /// planned virtual `Method.invoke` dispatches through
    /// [`NativeInvokeAccess::invoke_virtual_exact`] without the two
    /// selection reads (interpreter round i3 wave 77, lane L3). Default
    /// `false`.
    fn reflect_invoke_plan_facts(&self) -> bool {
        false
    }

    /// Is the reflective `exports` gate disabled
    /// (`CRATONVM_REFLECT_NO_EXPORT_GATE` present)? The VM answers from
    /// `NativeMethodRegistry::reflect_export_gate_disabled`, read once;
    /// this default reads the latched snapshot per call, as the served
    /// doors did before interpreter round i3 wave 75 (lane L3).
    fn reflect_export_gate_disabled(&self) -> bool {
        cratonvm_types::flags::runtime_var("CRATONVM_REFLECT_NO_EXPORT_GATE").is_ok()
    }

    /// `VmConfig::reflect_cs_adapter` (`CRATONVM_REFLECT_CS_ADAPTER`): the
    /// descriptor of the `@CallerSensitiveAdapter` overload the served
    /// `Method.invoke` runs for the static method `(name, descriptor)` of
    /// `class_id` -- HotSpot 25's `MethodHandleAccessorFactory.findCSMethodAdapter`:
    /// the method is `@CallerSensitive`, and its class declares a static
    /// method of the same name whose parameters are the method's plus a
    /// trailing `java.lang.Class` (the JDK's `@CallerSensitiveAdapter`).
    /// `None` when the switch is off or there is no such adapter (interpreter
    /// round i2 wave 61, lane L3). Default `None`.
    fn caller_sensitive_adapter(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
    ) -> Option<String> {
        None
    }

    /// `NativeMethodRegistry::inner_classes_agreement`: a member class's
    /// declaring class must list it back in its own `InnerClasses`, resolved
    /// by the declaring class's loader (`Reflection::check_for_inner_class`;
    /// interpreter round i2 wave 59, lane L3). Default `false`.
    fn inner_classes_agreement(&self) -> bool {
        false
    }

    /// One call of the `URLClassPath$Loader` walk intrinsic
    /// (`NativeMethodRegistry::nested_jar_walk_index`): `skipped` when the
    /// nested-jar index answered `null` without the JDK body, else the body
    /// ran. Counted per VM for the `CRATONVM_DBG=access` LOADER census line
    /// (`nested-jar-walk-skipped=` / `nested-jar-walk-asked=`; interpreter
    /// round i2 wave 54, lane L5). Default: no-op (mock contexts).
    fn note_nested_jar_walk(&self, skipped: bool) {
        let _ = skipped;
    }

    /// One call of the nested-jar bytes intrinsic on `Resource.getBytes`
    /// (`NativeMethodRegistry::nested_jar_bytes_index`) whose receiver is the
    /// generic `URLClassPath$Loader`'s `Resource`: `served` when the VM's copy
    /// of the nested jar answered the bytes, else the JDK body ran. Counted
    /// per VM for the `CRATONVM_DBG=access` LOADER census line
    /// (`nested-jar-bytes-served=` / `nested-jar-bytes-body=`; interpreter
    /// round i2 wave 63, lane L5). Default: no-op (mock contexts).
    fn note_nested_jar_bytes(&self, served: bool) {
        let _ = served;
    }

    /// One call of the jar-loader bytes intrinsic on `Resource.getBytes`
    /// (`NativeMethodRegistry::jar_loader_bytes_index`) whose receiver is a
    /// `URLClassPath$JarLoader` `Resource`: `served` when the VM's mapping of
    /// the jar answered the bytes, else the JDK body ran. Counted per VM for
    /// the `CRATONVM_DBG=access` LOADER census line
    /// (`jar-loader-bytes-served=` / `jar-loader-bytes-body=`; interpreter
    /// round i2 wave 64, lane L5). Default: no-op (mock contexts).
    fn note_jar_loader_bytes(&self, served: bool) {
        let _ = served;
    }

    /// One [`NestedJarRowEvent`] of the nested-jar rows, counted per VM for
    /// the `CRATONVM_DBG=access` LOADER census line (interpreter round i2
    /// wave 69, lane L5). Default: no-op (mock contexts).
    fn note_nested_jar_row(&self, event: NestedJarRowEvent) {
        let _ = event;
    }

    /// zt t16/pl7: one `JVM_DefineClass` / `JVM_DefineClassWithSource` of
    /// `bytes` class-file bytes (`ClassLoader.defineClass1` / `defineClass2`,
    /// and `Unsafe.defineClass0`, which HotSpot routes through
    /// `JVM_DefineClass`), counted once the natives have the bytes, before
    /// the class is parsed, as `jvm_define_class_common` counts it: the
    /// `sun.cls.defineAppClasses` and `sun.cls.appClassBytes` perf counters.
    /// Not `Lookup.defineClass` / hidden classes (`JVM_LookupDefineClass`)
    /// nor JNI `DefineClass`, which HotSpot does not count there. `true`
    /// when it counted (the native then times the define too); default:
    /// no-op and `false` (mock contexts).
    fn note_jvm_define_class(&self, bytes: usize) -> bool {
        let _ = bytes;
        false
    }

    /// zt t16/pl7: one `Unsafe.defineClass0` call, counted on entry as
    /// HotSpot's `Unsafe_DefineClass_impl` counts it
    /// (`sun.cls.unsafeDefineClassCalls`). Default: no-op (mock contexts).
    fn note_unsafe_define_class_call(&self) {}

    /// zt t16/pl7: this VM's class-trace figures, for a define native's
    /// `sun.cls.defineAppClassTime` event
    /// (`cratonvm_types::perf_class_trace::ClassTraceTime`); `None` unless
    /// `CRATONVM_PERF_CLASS_TIMERS` (default ON) armed them. Default: `None`
    /// (mock contexts).
    fn class_trace_counters(
        &self,
    ) -> Option<std::sync::Arc<cratonvm_types::perf_class_trace::ClassTraceCounters>> {
        None
    }

    /// This VM's slot for the `Resource.getBytes` intrinsics' archive caches
    /// (`NativeMethodRegistry::archive_bytes_cache_slot`; the nested-jar and
    /// jar-loader bytes indexes, interpreter round i2 wave 64, lane L5): a
    /// per-VM byte cache, type-erased because its type lives in
    /// `native-builtins`. `None` (mock contexts): the intrinsics open the
    /// archive for each ask, uncached. Since wave 65 the same slot also holds
    /// the `URLClassPath$Loader` walk index's entry-name sets and the
    /// `URLClassLoader` local class-path cache (process globals before).
    fn archive_bytes_cache_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        None
    }

    /// This VM's loader-identity tables (`native-builtins`'
    /// `classloader::LoaderIdentityTables`, owned by `SharedVm`), type-erased
    /// because the type lives in `native-builtins`. `None` (mock contexts):
    /// the natives use a detached instance keyed by `vm_identity()`.
    /// Interpreter round i5 wave 101, lane L5: process globals before.
    fn loader_identity_tables(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        None
    }

    /// A stamp that moves whenever an EXISTING class's hierarchy edges, its
    /// in-place provenance (a stub promoted to real bytes, with a new method
    /// table), its removal (unload) or a class redefinition could change what
    /// a by-class-id walk of superclasses and declared methods answers. The VM
    /// answers `hierarchy_edge_epoch + class_origin_in_place_epoch +
    /// class_redefinition_count`, each monotone, so the sum moves when any of
    /// them does. `None` (the default; mock contexts): no stamp, so memos keyed
    /// by class id are not kept. JIT round 23 wave 6, the proxy-define page
    /// (`CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO`).
    fn class_hierarchy_memo_stamp(&self) -> Option<u64> {
        None
    }

    /// This VM's `--enable-preview` (`VmConfig::enable_preview`): what
    /// `jdk.internal.misc.PreviewFeatures.isPreviewEnabled` answers, the same
    /// bit the class manager gates preview class files with. Default `false`
    /// (HotSpot's; mock contexts). Interpreter round i5 wave 101, lane L5: a
    /// process global in the reader, set only by the launcher, before.
    fn preview_enabled(&self) -> bool {
        false
    }

    /// zt t18/hunt2: this VM's `--finalization=` (`VmConfig::finalization_enabled`,
    /// JEP 421): what `java.lang.ref.Finalizer.isFinalizationEnabled()`
    /// answers and what `jcmd GC.finalizer_info` reads. Default `true`
    /// (HotSpot's; mock contexts).
    fn finalization_enabled(&self) -> bool {
        true
    }

    /// This VM's slot for the `ZipFile$Source.getEntryPos` miss intrinsic's
    /// resolved field slots (`NativeMethodRegistry::zip_source_slot_cache`;
    /// `CRATONVM_ZIP_ENTRY_MISS_INTRINSIC`, interpreter round i3 wave 76,
    /// lane L5), type-erased because the type lives in `native-builtins`.
    /// `None` (mock contexts): the intrinsic resolves the slots per call.
    fn zip_source_slot_cache(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        None
    }

    /// This VM's slot for the record intrinsics' `boolean`-component memo
    /// (`NativeMethodRegistry::record_boolean_components_slot`), type-erased
    /// because the type lives in `native-builtins`. `None` (mock contexts):
    /// the components are read per call. Interpreter round i6 wave 117, lane
    /// L4: a process-wide `static` before (AGENTS.md).
    fn record_boolean_components_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        None
    }

    /// This VM's slot for the `CRATONVM_VH_REAL_VARFORM` record
    /// (`NativeMethodRegistry::vh_real_varform_slot`): the ids of the hidden
    /// form classes this VM defined and the global roots of its shared
    /// `VarForm`s, type-erased because the type lives in `native-builtins`
    /// (`lang_invoke::VhRealVarFormRecord`). `None` (mock contexts): no form
    /// is ever built. Interpreter round i7 wave 136, lane L4 (page
    /// `i135-L4-real-varform-classes-are-application-visible`).
    fn vh_real_varform_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        None
    }

    /// The "hidden frames never observed" latch (interpreter round i6 wave
    /// 117, lane L4; `CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH`): `true` only while
    /// nothing in this VM can have seen a hidden `LambdaForm` activation -- no
    /// `SHOW_HIDDEN_FRAMES` walk has run ([`Self::note_hidden_frames_observed`]),
    /// no `-XX:+ShowHiddenFrames`, no JVMTI agent or JDWP configured, no JVMTI
    /// listener. Default `false` (mock contexts: assume observed).
    fn hidden_frames_never_observed(&self) -> bool {
        false
    }

    /// Does a JDWP debugger observe this VM's frames now (interpreter round
    /// i7 wave 144, lane L1; `CRATONVM_MH_FORM_FRAMES_UNDER_DEBUGGER`, parity,
    /// default ON)? `true` while a debugger session is attached. A
    /// method-handle call then runs the JDK's real `LambdaForm` frames
    /// (`DirectMethodHandle$Holder.invokeStatic` and the like), so a step
    /// completes in them and a listing shows them, as on HotSpot. The direct
    /// `linkTo*` lanes decline while it holds. Default `false` (mock
    /// contexts, and a VM without a debugger).
    fn debugger_observes_frames(&self) -> bool {
        false
    }

    /// A stack walk that lists hidden frames is running in this VM: clear the
    /// latch of [`Self::hidden_frames_never_observed`] for good. A no-op by
    /// default.
    fn note_hidden_frames_observed(&self) {}

    /// This VM's slot for the `java.lang.ref` layout memo
    /// (`NativeMethodRegistry::reference_layout_slot`;
    /// `CRATONVM_REFERENCE_LAYOUT_MEMO`, interpreter round i3 wave 77, lane
    /// L5), type-erased because the type lives in `native-builtins`. `None`
    /// (mock contexts): the memo resolves the layout per call.
    fn reference_layout_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        None
    }

    /// This VM's slot for `native-io`'s process tables (the spawned
    /// children, their exit and pipe records, the exit reapers and the exit
    /// signal; `NativeMethodRegistry::process_tables_slot`), type-erased
    /// because the type lives in `native-io`. `None` (mock contexts): one
    /// detached table set. Interpreter round i5 wave 101, lane L1: process
    /// globals before, so two VMs in one process saw each other's children.
    fn process_tables_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        None
    }

    /// `NativeMethodRegistry::nested_jar_shared_outer`: do the nested-jar walk
    /// and bytes indexes open each Spring Boot fat jar's outer archive once
    /// per VM (held in [`Self::archive_bytes_cache_slot`]) instead of once
    /// per nested spec (interpreter round i2 wave 66, lane L5)? Default
    /// `false` (mock contexts open per spec, as before).
    fn nested_jar_shared_outer(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::nested_jar_outer_index`: do the nested-jar walk
    /// and bytes indexes keep each fat jar's outer central directory once per
    /// VM WITHOUT its bytes, reading each nested jar with a bounded
    /// positioned read at the ask (interpreter round i2 wave 67, lane L5)?
    /// Default `false` (mock contexts open per spec, as before).
    fn nested_jar_outer_index(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::nested_jar_refusal_stamp`: does a refused
    /// nested-jar walk-index or bytes-index row (and a jar-loader bytes-index
    /// row) remember the outer file's stamp, so a later ask that finds the
    /// file changed rebuilds it instead of running the body for the VM's life
    /// (interpreter round i2 wave 68, lane L5)? Default `false` (mock
    /// contexts keep a refusal, as before).
    fn nested_jar_refusal_stamp(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::nested_jar_bytes_directory`: does a nested-jar
    /// bytes-index row over a STORED nested jar hold only its central
    /// directory and read each entry from the outer file at the ask, instead
    /// of owning a copy of the whole nested jar (interpreter round i2 wave 68,
    /// lane L5)? Default `false` (mock contexts keep the copy, as before).
    fn nested_jar_bytes_directory(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::nested_jar_outer_handle_reuse`: does a burst of
    /// reads from the directory-only rows of
    /// [`Self::nested_jar_bytes_directory`] keep one outer fat-jar handle open
    /// (closed about 50 ms after the burst's last read) instead of opening and
    /// closing the fat jar once per class (interpreter round i2 wave 69, lane
    /// L5)? Default `false` (mock contexts open per read, as before).
    fn nested_jar_outer_handle_reuse(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::nested_jar_pooled_stat`: does a hit on a
    /// directory-only nested bytes row whose fat jar has a kept handle skip
    /// its own `stat`, leaving the file check to the pooled read that follows
    /// (interpreter round i3 wave 71, lane L5)? Default `false` (mock
    /// contexts `stat` on every hit, as before).
    fn nested_jar_pooled_stat(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::nested_jar_class_path_outer`: do the nested-jar
    /// walk and bytes indexes build their rows for a fat jar that is on the
    /// VM's application class path (`java -jar app.jar`) from the class
    /// path's own bytes and parse ([`Self::class_path_outer_archive`]), with
    /// no open, map or parse of their own (interpreter round i2 wave 69, lane
    /// L5)? Default `false` (mock contexts open the file, as before).
    fn nested_jar_class_path_outer(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::jar_loader_class_path_archive`: does the
    /// jar-loader bytes index build its row for a jar that is on the VM's
    /// application class path from the class path's own bytes and parse
    /// ([`Self::class_path_outer_archive`]), with no open, map or parse of
    /// its own (interpreter round i2 wave 70, lane L5)? Default `false`
    /// (mock contexts open the file, as before).
    fn jar_loader_class_path_archive(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::jar_loader_class_path_direct`: does the
    /// jar-loader bytes index ask the application class path for the bytes
    /// of a jar it holds ([`Self::class_path_jar_entry_bytes`]) before it
    /// builds or reads a row (interpreter round i3 wave 71, lane L5)?
    /// Default `false` (mock contexts use the rows, as before).
    fn jar_loader_class_path_direct(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::define_one_parse`: do the `defineClass1` /
    /// `defineClass2` natives hand the class file their supertype pre-pass
    /// parsed to the define ([`DefineClassFull::preparsed`]) instead of
    /// letting it parse the bytes again (interpreter round i3 wave 72, lane
    /// L5)? Default `false` (two parses, as before).
    fn define_one_parse(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::real_builtin_loaders`: is
    /// `CRATONVM_REAL_BUILTIN_LOADERS` on for this VM (the JDK's own
    /// `ClassLoaders` objects as the platform and application loaders under
    /// `--real-jdk`; interpreter round i4 wave 95, lane L5 moved it off a
    /// process `OnceLock`)? Default `true`, the switch's default (mock
    /// contexts answer `is_jdk_only` themselves, which gates it).
    fn real_builtin_loaders(&self) -> bool {
        true
    }

    /// `NativeMethodRegistry::define_supertype_dictionary`: does the define
    /// natives' supertype pre-pass ask the defining loader's `loadClass` for
    /// a supertype once per (loader, name), as HotSpot's dictionary does,
    /// instead of once per define (interpreter round i3 wave 72, lane L5)?
    /// Default `false` (one call per define, as before).
    fn define_supertype_dictionary(&self) -> bool {
        false
    }

    /// `ClassManager::noverify_relaxation` of this VM: `(legality,
    /// structural)`, whether a define relaxes the `-Xverify:none` modifier /
    /// member-name legality checks and the structural format checks. The
    /// define natives' supertype pre-pass judges with the same scope as the
    /// define (t1 wave 13, lane I24W). Default `(false, false)`: strict.
    fn define_format_relaxation(&self) -> (bool, bool) {
        (false, false)
    }

    /// `NativeMethodRegistry::define_records_supertype_initiation`: does the
    /// define natives' supertype pre-pass record the defining (user) loader
    /// as an initiating loader of each supertype it resolves, as HotSpot's
    /// `resolve_super_or_fail` files it in the loader's dictionary
    /// (interpreter round i3 wave 79, lane L5)? Default `false` (no record).
    fn define_records_supertype_initiation(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::loader_kind_memo`: do the class-loader natives
    /// answer "is this a user-defined loader / a bare `URLClassLoader`" from a
    /// per-VM memo keyed by the receiver's class id, instead of a by-name
    /// `java/lang/ClassLoader` lookup, a superclass walk and a class-name copy
    /// per call (interpreter round i3 wave 81, lane L5)? Default `false` (the
    /// per-call computation, as before).
    fn loader_kind_memo(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::clv_sub_fast_key`: does the
    /// `AbstractClassLoaderValue` side table answer a complete `Sub` id row
    /// from a per-thread memo keyed by the VM identity, and build a `Sub` key
    /// shape without its lock vector (t1 wave 4 lane S; made per-VM in t1
    /// wave 6 lane Y3, it used to latch in a process `OnceLock`)? Default
    /// `false` (the locked table and both vectors, as before).
    fn clv_sub_fast_key(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::loader_namespace_index`: do the class-loader
    /// natives answer "which namespace is this user loader" from a per-VM
    /// index keyed by the loader's address and valid for one GC epoch
    /// (`gc_collection_count`), and the real-`ClassLoader`-layout verdict from
    /// a per-VM memo keyed by class id, instead of two process-global `Vec`
    /// scans under their mutexes and a by-name field walk per ask
    /// (interpreter round i3 wave 83, lane L5)? Default `false` (the scans, as
    /// before).
    fn loader_namespace_index(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::platform_resource_jrt_walk`: does the platform
    /// loader's (and a null-parent loader's) `ClassLoader.getResource` native
    /// ask only the runtime-image class-path entries for its `jrt:` URL
    /// ([`NativeClassAccess::first_runtime_image_resource_url`]) instead of
    /// listing every entry's URLs and keeping the first `jrt:` one
    /// (interpreter round i3 wave 73, lane L5)? Default `false` (the whole
    /// list, as before).
    fn platform_resource_jrt_walk(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::inflater_reuse_buffers`: do the
    /// `java.util.zip.Inflater` heap-array natives read the input and inflate
    /// into per-stream scratch buffers kept with the stream's zlib state,
    /// instead of allocating (and zeroing) both per call (interpreter round
    /// i3 wave 73, lane L5)? Default `false` (two fresh buffers per call).
    fn inflater_reuse_buffers(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::inflater_input_window`: does the
    /// `java.util.zip.Inflater.inflateBytesBytes` native hand zlib the
    /// caller's input in bounded windows (stepping again inside the one call
    /// while a step consumes its whole window), instead of copying the whole
    /// remaining input on every call (interpreter round i3 wave 75, lane L5)?
    /// Default `false` (the whole remaining input per call, as before).
    fn inflater_input_window(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::inflater_raw_adler_zero`: does a raw
    /// (`nowrap = true`) `java.util.zip.Inflater` keep no ADLER-32 and answer
    /// `getAdler()` = 0, as zlib's `strm->adler` of a raw stream does under
    /// HotSpot, instead of running a checksum over every byte it inflates
    /// (interpreter round i3 wave 74, lane L5)? Default `false` (the running
    /// checksum, as before).
    fn inflater_raw_adler_zero(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::inflater_sharded_table`: does a new
    /// `java.util.zip.Inflater` stream's zlib state go in one of the sharded
    /// handle table's 63 lock shards, so streams inflated on different
    /// threads take different locks (interpreter round i5 wave 100, lane L5)?
    /// Default `false` (the one shared table and lock, as before).
    fn inflater_sharded_table(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::jdk_feature`: the booted real image's feature
    /// version (its `release` file), for the class paths a native builds
    /// (multi-release jars are served at it; interpreter round i5 wave 100,
    /// lane L5). Default `None` (unknown: the newest this VM serves).
    fn jdk_feature_version(&self) -> Option<u32> {
        None
    }

    /// `NativeMethodRegistry::arrays_mismatch_bulk`: do the
    /// `jdk.internal.util.ArraysSupport.mismatch` byte/char intrinsics and
    /// `vectorizedMismatch`'s element-index path compare the two payloads in
    /// bulk copies instead of two `get_array_element` calls per element
    /// (interpreter round i3 wave 74, lane L5)? Default `false` (per element,
    /// as before).
    fn arrays_mismatch_bulk(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::compat_url_loader_defined_packages`: under
    /// `--compatible` too, a user loader's `getDefinedPackage` class-file
    /// probe answers "defined" only once the loader has defined a class of the
    /// package, as `--real-jdk` does (interpreter round i2 wave 64, lane L5).
    /// Default `false` (mock contexts keep the historical answer).
    fn compat_url_loader_defined_packages(&self) -> bool {
        false
    }

    /// `NativeMethodRegistry::string_literal_identity`: does a native standing
    /// in for a JDK 25 body that answers a string literal answer the pooled
    /// literal (j27 wave 7, lane T1LOOPSTATIC, SBEMPTY17-4: per VM)? Default:
    /// the switch read now (mock contexts have no registry; default ON, `=0`
    /// kill), so a mock answers what the process `OnceLock` answered.
    fn string_literal_identity(&self) -> bool {
        true
    }

    /// Does the `IMPL-JARS` archive fallback after a failed `loadClass` run
    /// only for Elasticsearch's `EmbeddedImplClassLoader`
    /// (`NativeMethodRegistry::impl_jars_es_loader_only`; interpreter round i2
    /// wave 54, lane L5)? Default `true` (mock contexts).
    fn impl_jars_es_loader_only(&self) -> bool {
        true
    }

    /// Do runtime-image classes of the JDK's application-loader modules
    /// report the application loader, and non-boot image classes a
    /// `jrt:/<module>` `CodeSource`
    /// (`NativeMethodRegistry::image_app_module_loaders`; interpreter round
    /// i1 wave 50, lane L5)? Default `false` (mock contexts).
    fn image_app_module_loaders(&self) -> bool {
        false
    }

    /// Does the base `ClassLoader.loadClass` native hold a parallel-capable
    /// user loader's per-name lock (`getClassLoadingLock(name)`) around the
    /// load, as the JDK's own `loadClass` does
    /// (`NativeMethodRegistry::per_name_loader_locks`; interpreter round i1
    /// wave 51, lane L5)? Default `false` (mock contexts).
    fn per_name_loader_locks(&self) -> bool {
        false
    }

    /// Does the base `ClassLoader.loadClass` native's parent-first step ask a
    /// BARE `java.net.URLClassLoader` parent's own `loadClass`, as it asks a
    /// user-defined parent's (`NativeMethodRegistry::delegates_to_url_parent`;
    /// interpreter round i2 wave 60, lane L5)? Default `false` (mock
    /// contexts).
    fn delegates_to_url_parent(&self) -> bool {
        false
    }

    /// Does a served loader rung read only the resolver's explicit absence
    /// shape (`ClassFileError::ClassNotFound` of the requested name) as "not
    /// found", and hand every other linkage refusal back as its throwable
    /// (`NativeMethodRegistry::resolver_absence_shape`; interpreter round i2
    /// wave 61, lane L5)? Default `false` (mock contexts).
    fn resolver_absence_shape(&self) -> bool {
        false
    }

    /// The Java throwable a VM linkage error raises, as the VM's own
    /// conversion renders it (`runtime::exceptions::linkage_throwable`):
    /// `(internal class name, message)`, or `None` for an error that is not a
    /// `VmError::Linkage`. Interpreter round i2 wave 61, lane L5, for the
    /// served loader rungs (`classloader_real::load_class_visible_to`), which
    /// kept a partial copy (`definer_diagnosis`). Default `None` (mock
    /// contexts).
    fn linkage_throwable_of(
        &self,
        error: &cratonvm_types::error::VmError,
    ) -> Option<(&'static str, String)> {
        let _ = error;
        None
    }

    /// Does the VM keep JVMS §5.3 initiating-loader records for user loaders
    /// (`findLoadedClass` sees a class the loader initiated, and the loader
    /// may not define that name itself)? `--real-jdk`, or `--compatible` with
    /// `CRATONVM_COMPAT_INITIATING_RECORDS` (`ClassRealm::initiating_records_on`;
    /// interpreter round i1 wave 48, lane L5). Default: [`Self::is_jdk_only`].
    fn initiating_records_on(&self) -> bool {
        self.is_jdk_only()
    }

    /// `CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK` (`VmConfig::loader_drive_under_loader_lock`,
    /// default ON, `=0` off): a VM-initiated `loadClass` through a loader that
    /// is not parallel-capable re-checks the loader's dictionary under the
    /// loader's monitor and publishes its answer before releasing it, as
    /// HotSpot's `resolve_instance_class_or_null` does. The natives' door is
    /// `classloader_real::invoke_load_class_as_the_vm`. `false` by default
    /// (a context without a VM keeps the old order). Interpreter round i6
    /// wave 130, lane L7.
    fn loader_drive_under_loader_lock(&self) -> bool {
        false
    }

    /// Does a direct (reflective) call of `LambdaMetafactory.metafactory` /
    /// `altMetafactory` run the JDK's own factory rather than the registered
    /// reflective bridge? `CRATONVM_LMF_REFLECTIVE_TO_JDK`, read once per VM
    /// (interpreter round i1 wave 49, lane L4). Default: `false` (mock
    /// contexts keep the bridge).
    fn lambda_metafactory_reflective_to_jdk(&self) -> bool {
        false
    }

    /// Do the `MethodHandles` combinator natives make the JDK's argument
    /// checks? `--real-jdk`, or `--compatible` with
    /// `CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE` (on by default;
    /// `ClassRealm::mh_combinator_checks`, interpreter round i1 wave 50, lane
    /// L4). Default: [`Self::is_jdk_only`].
    fn mh_combinator_checks(&self) -> bool {
        self.is_jdk_only()
    }

    /// Do `Instrumentation.getObjectSize` and the C JVMTI `GetObjectSize`
    /// answer HotSpot's sizes (its compressed-reference layout, modelled from
    /// the object's shape) rather than this heap's footprint?
    /// `CRATONVM_HOTSPOT_OBJECT_SIZE`, read once per VM (interpreter round i2
    /// wave 62, lane L1). Default: `false` (mock contexts keep the
    /// footprint).
    fn hotspot_object_sizes(&self) -> bool {
        false
    }

    /// The HotSpot size [`Self::hotspot_object_sizes`] computed for an
    /// instance of `class_id` while the class had `tag` instance fields in
    /// all, if this VM remembers it (interpreter round i2 wave 62, lane L1).
    /// Default: none (mock contexts compute every time).
    fn hotspot_size_memo(&self, class_id: ClassId, tag: usize) -> Option<u64> {
        let _ = (class_id, tag);
        None
    }

    /// Remember a size for [`Self::hotspot_size_memo`]. Default: nothing.
    fn remember_hotspot_size(&self, class_id: ClassId, tag: usize, size: u64) {
        let _ = (class_id, tag, size);
    }

    /// Is a `java.net.URL`'s application-provided `URLStreamHandler` the
    /// authority for its bytes (`URL.openStream` runs the JDK's
    /// `openConnection().getInputStream()` through it)? `--real-jdk`, or
    /// `--compatible` with `CRATONVM_COMPAT_URL_APP_HANDLER`
    /// (`NativeMethodRegistry::url_app_handler_is_authoritative`; interpreter
    /// round i1 wave 49, lane L6). Default: [`Self::is_jdk_only`].
    fn url_app_handler_is_authoritative(&self) -> bool {
        self.is_jdk_only()
    }

    /// Do VM-raised messages carry HotSpot's text -- a loader named by its
    /// `nameAndId`, `ClassLoader.defineClass`'s argument, header and
    /// duplicate-definition errors -- rather than this VM's older wording?
    /// `--real-jdk`, or `--compatible` with `CRATONVM_COMPAT_MODULE_MESSAGES`
    /// (`NativeMethodRegistry::messages_name_user_loaders`; interpreter round
    /// i1 wave 49, lane L6). Default: [`Self::is_jdk_only`].
    fn messages_name_user_loaders(&self) -> bool {
        self.is_jdk_only()
    }

    /// This VM has minted a `VarHandle` with invoke-exact behaviour
    /// (`withInvokeExactBehavior()`); `exact_slot` is the field slot of
    /// `java.lang.invoke.VarHandle.exact`. The doors that hold a call site's
    /// descriptor read that slot of each `VarHandle` receiver and judge an
    /// exact one against the call site
    /// (`lang_invoke::varhandle_exact_call_site_refusal`). Since wave 49 they
    /// also resolve the slot themselves on their first `VarHandle` site (an
    /// exact handle the JDK's bytecode mints reports nothing), so this only
    /// publishes it early. Per-VM. Default no-op (mock contexts).
    fn note_exact_var_handle_minted(&mut self, _exact_slot: usize) {}

    /// Set a system property. Returns the old value if any.
    fn set_system_property(&mut self, key: &str, value: &str) -> Option<String>;

    /// Remove a system property from the global store. Returns the old value if
    /// any. Default no-op (mock contexts have no live store); the VM overrides it.
    fn remove_system_property(&mut self, _key: &str) -> Option<String> {
        None
    }

    /// Register (or look up) a minimal synthetic class with the given name
    /// and instance-field count, returning its `ClassId`.
    ///
    /// Unlike [`ensure_class_initialized`], this never fails: when the real
    /// `.class` file cannot be loaded it still produces a usable `ClassId`
    /// whose class declares `num_fields` instance fields. Native allocators
    /// MUST use this (rather than `ClassId::new(0)`) as the fallback class
    /// when allocating an object with a non-zero field count — allocating
    /// with `ClassId::new(0)` (`java/lang/Object`, which declares zero
    /// fields) produces an "undersized object layout" object that the GC's
    /// `get_field` bounds guard rejects on every field access.
    ///
    /// # This spelling cannot report a refusal — prefer the fallible one
    ///
    /// [`Self::try_ensure_synthetic_class`] is the same operation with an
    /// error channel, and it is what a native should call whenever its own
    /// signature can carry a failure (`MethodCallResult` and friends —
    /// `ClassIdentityError` converts into `MethodCallFailed` with `?`).
    ///
    /// "Never fails" above is a statement about the *signature*, not about
    /// reality. There is one question this operation genuinely cannot answer:
    /// a `name` that two or more **distinct** classes already carry. Minting a
    /// stand-in for it is the defect this pair exists to remove — the stand-in
    /// is filed under the bootstrap loader, which is probed first, so it then
    /// outranks every real class that made the name ambiguous. Behaviour when
    /// that happens:
    ///
    /// * **this method** does *not* mint under `name`. The VM's context hands
    ///   back a distinctly-named, correctly-sized
    ///   `cratonvm/synthetic/AmbiguousName$…` stand-in: safe to allocate
    ///   against, and it fails every identity question about `name` (a
    ///   `checkcast` to `name` is false, a method lookup finds nothing) so the
    ///   failure surfaces at the call that depended on the answer, naming the
    ///   requested class.
    /// * **the default implementation** below returns `ClassId::new(0)`, as it
    ///   always has.
    ///
    /// Neither is a guess between the ambiguous candidates, and neither
    /// registers anything under `name`.
    ///
    /// The default implementation falls back to `ClassId::new(0)` so mocks
    /// and non-VM contexts still compile; real VM contexts override it.
    ///
    /// # Provenance
    ///
    /// `#[track_caller]`, and it must stay that way. `ClassManager::
    /// admit_compatibility_class` records the Rust `Location::caller()` as the
    /// census's `requested_by`, and without this attribute every native in the
    /// workspace is reported as the one line of `NativeContextImpl` that
    /// forwards the call — which is what the 2026-08-05 census found (all
    /// seven native-minted classes attributed to `vm_exec.rs:13670`). The
    /// attribute is free at runtime for callers that never fabricate.
    ///
    /// # The infallible spelling is GONE
    ///
    /// `ensure_synthetic_class` — same operation, no error channel — was
    /// deleted on 2026-08-10 by Real-JDK wave 2 step 3, along with
    /// `ClassManager::ensure_synthetic_class` behind it. It recorded the
    /// `--real-jdk` violation and then fabricated anyway, so a strict run
    /// reported a violation while continuing in the exact state contract §5
    /// forbids, and no signature in the workspace could say otherwise. Every
    /// caller now either propagates the refusal, absorbs it at a site that
    /// documents why, or — if what it wants is a VM-generated shape rather than
    /// a compatibility stand-in — asks [`Self::ensure_vm_internal_class`],
    /// which is a different question with a different answer.
    ///
    /// This is the fallible spelling: the same operation, with a channel for
    /// the two answers that are not a `ClassId`.
    ///
    /// # The ambiguity contract
    ///
    /// [`ClassIdentityError::AmbiguousName`] means the name is carried by two
    /// or more distinct classes and the VM refuses to pick or to mint a third.
    /// **A native that receives it should refuse in turn** — propagate with
    /// `?`, or re-ask with an initiating loader
    /// ([`NativeClassAccess::class_id_by_name_and_loader`],
    /// [`NativeClassAccess::class_id_by_name_via_referencing_class`]) if it has
    /// one. It must not fall back to a fabrication, to
    /// `ClassId::new(0)`, or to any same-named class of its own choosing:
    /// every one of those is the guess the refusal exists to prevent, and two
    /// distinct classes treated as one is type confusion — it defeats the
    /// verifier and produces machine code that reads the wrong object layout.
    ///
    /// **The stated consequence:** a shim that hits this stops working, and
    /// its Java-visible failure is an exception from the refusing native
    /// rather than a wrong answer. That is the intended trade. It is reachable
    /// only when an application really does have two distinct classes under
    /// one name (two `GroovyClassLoader$InnerLoader`s compiling the same
    /// script name, a webapp loader shadowing a container class), which is
    /// exactly the situation in which the old behaviour silently corrupted
    /// both.
    ///
    /// [`ClassIdentityError::Refused`] is the `--real-jdk` policy refusal:
    /// propagate it, retrying cannot help.
    ///
    /// # Default implementation
    ///
    /// `Ok(ClassId::new(0))` — the answer the deleted infallible default always
    /// gave, so mocks and non-VM contexts are unaffected.
    ///
    /// `#[track_caller]` for the reason stated above.
    #[track_caller]
    fn try_ensure_synthetic_class(
        &mut self,
        name: &str,
        num_fields: usize,
    ) -> Result<ClassId, ClassIdentityError> {
        let _ = (name, num_fields);
        Ok(ClassId::new(0))
    }

    /// Register (or look up) a **VM-generated** class — the other half of the
    /// §5 API boundary, and the reason deleting the infallible compatibility
    /// spelling did not have to break dynamic proxies.
    ///
    /// [`Self::try_ensure_synthetic_class`] mints *compatibility stand-ins*,
    /// the one thing `--real-jdk` forbids. This mints the classes a conforming
    /// JVM creates without any class file — array-adjacent shapes, lambda and
    /// proxy implementation classes and their superclasses, reflection
    /// accessors, and the VM's own internal allocation shapes. Contract §1 item
    /// 6 permits those in every mode, so **this never refuses and never records
    /// a violation**, and it is infallible for that reason rather than by
    /// oversight.
    ///
    /// **Do not reach for it to silence a refusal.** Contract §11's
    /// zero-stub census becomes unfalsifiable if a compatibility stand-in is
    /// minted through this door: the substitution continues and the report goes
    /// green. `ClassManager::ensure_generated_class` behind it `debug_assert`s
    /// on a `CompatibilityStub` origin, which a release build will not catch —
    /// the assertion is a backstop, not the decision. The decision is whether
    /// the JVM specification says a class file must exist for this name.
    ///
    /// # Default implementation
    ///
    /// `ClassId::new(0)`, matching the fallible sibling's default, so mocks and
    /// non-VM contexts compile unchanged. Real VM contexts override it.
    #[track_caller]
    fn ensure_vm_internal_class(&mut self, name: &str, num_fields: usize) -> ClassId {
        let _ = (name, num_fields);
        ClassId::new(0)
    }

    /// Check if a ClassId represents an interface.
    fn is_interface_class(&self, class_id: ClassId) -> bool;

    /// `(is_interface_class, superclass_of)` for `class_id`, answered by ONE
    /// class-store read where the VM can (W4B: `Class.getSuperclass` asked the
    /// two separately, two `class_manager` read locks per call). The default
    /// asks them one after the other, so mocks need no change.
    fn interface_flag_and_superclass_of(&self, class_id: ClassId) -> (bool, Option<ClassId>) {
        (
            self.is_interface_class(class_id),
            self.superclass_of(class_id),
        )
    }

    /// Check if a ClassId represents an abstract class. Default returns false.
    fn is_abstract_class(&self, _class_id: ClassId) -> bool {
        false
    }

    /// T1.6.7 — `Thread.holdsLock(Object)`. Returns `true` iff the
    /// current thread currently holds the monitor for `obj`. Default
    /// implementation returns `false` so non-monitor-aware contexts
    /// (mocks, stubs) fall back to the spec-permitted "no" answer.
    fn current_thread_holds_lock(&self, _obj: ObjectRef) -> bool {
        false
    }

    /// [`Self::current_thread_holds_lock`] for a LEAF native
    /// ([`NativeMethodRegistry::set_leaf`]): the same answer when it is read
    /// from the object's mark word and this thread's caches alone -- no lock
    /// taken, nothing allocated -- and `None`, with nothing done, when only the
    /// full answer can give it (a cache miss, an inflated monitor outside this
    /// thread's cache, a forwarded word). JIT tier2 wave 5, lane nat21
    /// (`NATIVE21-2`). The default answers `None`, so mocks and out-of-tree
    /// contexts take the full answer.
    fn current_thread_holds_lock_lock_free(&self, _obj: ObjectRef) -> Option<bool> {
        None
    }

    /// T19_K2 — Register a native-spawned OS thread with the VM's
    /// `ThreadRegistry`.
    ///
    /// Used by event-loop schedulers (Vert.x / Netty / XNIO) that spawn
    /// their own carrier OS threads via `std::thread::spawn` rather than
    /// going through `Thread.start0`. Returning the thread ids these
    /// schedulers create through this entry point ensures:
    ///
    /// * the CLI's `wait_for_non_daemon_threads()` waits for them when
    ///   `daemon == false` (otherwise the VM exits as soon as `main()`
    ///   returns even though Quarkus / Keycloak's HTTP listeners are
    ///   still alive),
    /// * GC root scanning sees their stacks (T1.5.1 path),
    /// * JVMTI thread-list APIs see them.
    ///
    /// Parameters:
    /// * `name`     — thread label (shown in `ThreadInfo`, panic logs)
    /// * `daemon`   — `false` for Vert.x / Netty event loops, `true` for
    ///                truly background schedulers (XNIO IO threads, GC
    ///                workers)
    /// * `join_handle_ptr` — opaque `Box<JoinHandle<()>>` raw pointer.
    ///                The VM takes ownership and arranges for it to be
    ///                joined when `wait_for_non_daemon_threads()` runs.
    ///                Pass `0` to register without a join handle (the
    ///                caller is responsible for ensuring the thread
    ///                eventually terminates on its own).
    ///
    /// Returns the registered `ThreadId.0` (a u64) on success. A value
    /// of `0` indicates the call was a no-op (mock context or registry
    /// not available); callers should treat this as "thread spawned but
    /// not VM-tracked" — the OS thread still runs, it just won't keep
    /// the process alive.
    ///
    /// The default implementation is a no-op so mock contexts and
    /// any future trait consumers don't need to implement registry
    /// plumbing. The VM override (`vm/src/vm/vm_exec.rs`) wires it
    /// into `ThreadRegistry::register_with_daemon` + `set_join_handle`.
    fn register_native_thread(
        &mut self,
        _name: &str,
        _daemon: bool,
        _join_handle_ptr: usize,
    ) -> u64 {
        0
    }

    /// Return GC/STW hooks for a thread registered via
    /// [`Self::register_native_thread`].
    ///
    /// The returned object is intentionally independent of `&mut self` so a
    /// spawned host thread can carry it into its event loop and bracket native
    /// waits without a full `NativeContext`.
    fn native_thread_blocker(&self, _thread_id: u64) -> Option<Arc<dyn NativeThreadBlocker>> {
        None
    }

    /// T19_K2 — Mark a previously-registered native thread dead.
    ///
    /// Called from a native-spawned OS thread's exit path right before
    /// the OS thread's `JoinHandle` returns. Flips the registry's
    /// `alive` flag for the given id so `is_alive()` returns false and
    /// `alive_non_daemon_thread_ids()` no longer reports it. The
    /// `JoinHandle` is still kept by the registry — `join()` will
    /// observe the dead flag and return immediately.
    ///
    /// Default impl is a no-op.
    fn unregister_native_thread(&mut self, _thread_id: u64) {}

    /// gcj j2/cm2: a thread registered by [`Self::register_native_thread`]
    /// whose OS thread could not be spawned. The entry would otherwise stay
    /// alive, counted by every later stop-the-world census, with no thread to
    /// arrive and no `JoinHandle` to report it finished. The VM unregisters it
    /// where the native-thread lifecycle fix applies
    /// (`CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK`, every backend); with it off,
    /// and by default, nothing happens (the behaviour before this method).
    fn abandon_native_thread(&mut self, _thread_id: u64) {}

    /// T19_K2 — Attach a `Box<JoinHandle<()>>` raw pointer to an
    /// already-registered native thread.
    ///
    /// Used by event-loop schedulers that need to know the assigned
    /// `ThreadId` BEFORE spawning the OS thread (so the spawned closure
    /// can capture it and use it on exit). The two-phase API is:
    ///
    ///   1. Call `register_native_thread(name, daemon, 0)` to get the
    ///      `ThreadId.0` without an attached handle.
    ///   2. Spawn the OS thread; capture the id in its closure.
    ///   3. Call `attach_join_handle_to_native_thread(id, raw_ptr)` to
    ///      hand the `JoinHandle<()>` over to the registry.
    ///
    /// `join_handle_ptr` follows the same ownership protocol as
    /// `register_native_thread`: a raw `Box<JoinHandle<()>>` pointer
    /// the VM takes back as `Box::from_raw`.
    ///
    /// Returns `true` if the attach succeeded, `false` if `thread_id`
    /// was unknown (in which case the caller MUST reclaim the
    /// `Box<JoinHandle<()>>` via `Box::from_raw` or it leaks the OS
    /// thread).
    fn attach_join_handle_to_native_thread(
        &mut self,
        _thread_id: u64,
        _join_handle_ptr: usize,
    ) -> bool {
        false
    }

    /// T19_K4 — Attach a `java.lang.Thread` mirror to an
    /// already-registered native thread.
    ///
    /// Used by event-loop schedulers (Vert.x / Netty / XNIO) that
    /// register their carrier OS thread via
    /// [`Self::register_native_thread`] but also need a real
    /// `java.lang.Thread` mirror so:
    ///
    ///  * `Thread.currentThread()` resolves to the right object when
    ///    Java-side code runs on the event-loop carrier (e.g. a
    ///    Runnable that consults the thread name),
    ///  * `ThreadRegistry::find_thread_id_by_thread_obj` works
    ///    cross-thread (so other code that has the mirror handle can
    ///    locate the `ThreadId`),
    ///  * the carrier thread shows up in
    ///    `ThreadRegistry::alive_thread_objects()` (and therefore in
    ///    `Thread.enumerate()` / JVMTI thread-list listings).
    ///
    /// `thread_id` must be a value previously returned by
    /// [`Self::register_native_thread`] / the two-phase
    /// [`Self::attach_join_handle_to_native_thread`] flow. The
    /// `java_thread_obj` should be a freshly-allocated synthetic
    /// `java.lang.Thread` mirror with `name` set on slot 0 and
    /// (optionally) `tid` on slot 2 — `register_native_thread`
    /// already stamped the registry entry, this call just attaches
    /// the mirror so look-ups by `ObjectRef` succeed.
    ///
    /// Returns `true` if the thread id was found and the mirror was
    /// stored, `false` if the id was unknown. Default impl is a
    /// no-op so mock contexts don't need to model a heap.
    fn set_native_thread_java_obj(&mut self, _thread_id: u64, _java_thread_obj: ObjectRef) -> bool {
        false
    }

    /// Emit a `jdk.VirtualThreadPinned` JFR event for the current thread.
    /// Called when a pinned virtual thread is about to block its carrier.
    ///
    /// Round-4: `reason` is `&'static str` (JEP 491 pin-reason taxonomy:
    /// "Synchronized", "Native", "Thread.sleep while pinned", ...).
    fn emit_virtual_thread_pinned_jfr(&mut self, _reason: &'static str) {}

    /// Start the recorder used by the real-JDK jdk.jfr.Event boundary.
    /// The default keeps lightweight NativeContext mocks independent of JFR.
    fn jfr_begin_java_recording(&mut self) {}

    /// Stop the recorder used by the real-JDK jdk.jfr.Event boundary.
    fn jfr_end_java_recording(&mut self) {}

    /// Whether the real-JDK JFR boundary currently has a running recording.
    fn jfr_java_recording_active(&self) -> bool {
        false
    }

    /// SBR5 lane jfr: a nonzero tick count while the real JFR backend has a
    /// recording running, `0` otherwise (one relaxed load). A native that shadows
    /// JDK code which commits a `jdk.internal.event` event reads it before the
    /// operation and hands it to the matching `jfr_*_done` after.
    fn jfr_vm_ticks(&self) -> i64 {
        0
    }

    /// SBR5 lane jfr: does the JIT cache hold a compiled body of this method
    /// (what a JFR stack frame is labelled `JIT compiled` for, versus
    /// `Interpreted`)?
    fn jfr_method_compiled(
        &self,
        class_id: ClassId,
        class_name: &str,
        method: &str,
        descriptor: &str,
    ) -> bool {
        let _ = (class_id, class_name, method, descriptor);
        false
    }

    /// SBR5 lane jfr: the (young, old) collector names the VM's heap reports in
    /// `jdk.GCConfiguration` (`"N/A"` for a collector with no young generation).
    fn jfr_collector_names(&self) -> (&'static str, &'static str) {
        ("N/A", "N/A")
    }

    /// zs s2/ob: ZGC's configuration for `jdk.GCConfiguration` /
    /// `jdk.GCHeapConfiguration`; `None` (the default, and every other
    /// collector, and `CRATONVM_ZGC_JFR_HOTSPOT_CONFIGURATION=0`) keeps the
    /// writer's generic figures.
    fn jfr_zgc_configuration(&self) -> Option<JfrZgcConfiguration> {
        None
    }

    /// zs s7/jf: does the `Runtime.gc()` native write `jdk.SystemGC` itself,
    /// on the calling thread with its stack and around the collection (as
    /// HotSpot's `JVM_GC`)? `Some(invoked_concurrent)` when it does -- ZGC, an
    /// explicit GC this VM does not ignore, `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER`
    /// on -- and the collection's own events then leave the event out. `None`
    /// (the default, and every other collector) keeps the collection's row.
    fn jfr_system_gc_on_caller(&self) -> Option<bool> {
        None
    }

    /// `FileOutputStream` wrote `bytes` bytes (`stream` is the stream object);
    /// `start_ticks` is what [`Self::jfr_vm_ticks`] answered before.
    fn jfr_file_write_done(&mut self, stream: ObjectRef, bytes: i64, start_ticks: i64) {
        let _ = (stream, bytes, start_ticks);
    }

    /// Record one Java `jdk.jfr.Event.commit()` through the VM recorder.
    ///
    /// `event_name` is the **JFR event name** — the `@Name` value when the
    /// event class carries one, else its binary class name — not the internal
    /// class name. That is what a consumer sees from
    /// `RecordedEvent.getEventType().getName()` and what
    /// `RecordingStream.onEvent(String, …)` matches on, so the name has to be
    /// canonicalised on the way in rather than decorated here.
    ///
    /// `fields` carries `(field name, JVM field descriptor, current value)` in
    /// the order the event type declares them. The descriptor is what lets the
    /// VM register the right JFR field type — `Value::Int` alone cannot
    /// distinguish a `boolean` from an `int` — and a `String` field arrives as
    /// its `Value::Object` because only the VM side can read the characters
    /// out of the heap.
    fn jfr_emit_java_event(
        &mut self,
        _event_name: &str,
        _fields: &[(String, String, Value)],
        _start_ns: u64,
        _duration_ns: u64,
    ) {
    }

    /// Apply the settings a `jdk.jfr.Recording` carries to the VM recording the
    /// Java boundary is driving.
    ///
    /// `enabled_names` is the set of JFR event names the Java side enabled.
    /// `None` means "no name filter" — record everything, which is what
    /// CratonVM's own recordings want. `Some(&[])` means **record nothing**, and
    /// that distinction is the point: a `new Recording()` with no `enable(...)`
    /// call records no events on HotSpot, so an empty list cannot be allowed to
    /// mean "everything".
    ///
    /// `thresholds` is `(event name, minimum duration in nanoseconds)`; an event
    /// shorter than its threshold is dropped.
    ///
    /// Both are keyed by NAME because the Java side knows which events are
    /// enabled before any of them has been committed, and a CratonVM event type
    /// gets its id at first commit.
    fn jfr_configure_java_recording(
        &mut self,
        _enabled_names: Option<&[String]>,
        _thresholds: &[(String, u64)],
    ) {
    }

    /// Remember the output requested by the JDK recorder so stopping it can
    /// flush the VM recording to the same path.
    fn jfr_set_java_output(&mut self, _path: &str) {}

    /// Dump the Java-owned recording to a caller-selected path.
    fn jfr_dump_java_recording(&mut self, _path: &str) {}

    // -- VM stats methods (for JMX) --

    /// Effective available-processor count, honoring container/cgroup CPU
    /// limits when `-XX:+UseContainerSupport` is active. This backs
    /// `Runtime.availableProcessors()` and the JMX `OperatingSystemMXBean`.
    ///
    /// The default (used by mock/test contexts) returns the host hardware
    /// thread count; the VM overrides it to prefer the cgroup-derived count
    /// from `VmConfig::container_effective_processors` when present.
    fn available_processor_count(&self) -> i32 {
        std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(1)
    }

    /// Resolved modelled HotSpot flags for this VM, including their origins.
    /// Empty in a context without a VM configuration.
    fn hotspot_vm_flags(&self) -> Vec<crate::hotspot_flags::HotspotVmFlag> {
        Vec::new()
    }

    /// zt t24/x20: `HotSpotDiagnosticMXBean.setVMOption(name, ..)` of a
    /// modelled `{manageable}` row other than ZGC's `SoftMaxHeapSize` (which
    /// has its own door, `zgc_set_soft_max_heap_flag`): `value` is the
    /// `Flag.setLongValue` argument read as HotSpot's unsigned flag value.
    /// `None` when this context models no writable flag of that name (the
    /// default: mock/test contexts); `Some(Err(text))` with HotSpot's
    /// `WriteableFlags` message for a value its range or constraint refuses;
    /// `Some(Ok(()))` once the value is in force (origin `{management}`).
    fn set_manageable_hotspot_flag(&self, name: &str, value: u64) -> Option<Result<(), String>> {
        let _ = (name, value);
        None
    }

    /// zt t27/lg1: `setVMOption(name, ..)` of a modelled `{manageable}`
    /// `bool` or `ccstr` row (`Flag.setBooleanValue` passes `"true"` /
    /// `"false"`, `Flag.setStringValue` the text). Same answers as
    /// [`Self::set_manageable_hotspot_flag`]; `None` by default.
    fn set_manageable_hotspot_text_flag(
        &self,
        name: &str,
        value: &str,
    ) -> Option<Result<(), String>> {
        let _ = (name, value);
        None
    }

    /// Maximum heap size in bytes, as reported by `Runtime.maxMemory()` and the
    /// JMX `MemoryMXBean`. The default (mock/test contexts) is the historical
    /// 256 MiB placeholder; the VM overrides it to return the configured
    /// `-Xmx` (which is itself container-aware once sized from a cgroup limit).
    fn max_heap_bytes(&self) -> i64 {
        256 * 1024 * 1024
    }

    /// Initial heap size in bytes, as reported by the JMX `MemoryMXBean`'s
    /// heap `MemoryUsage.getInit()`. The default (mock/test contexts) mirrors
    /// `max_heap_bytes`'s historical placeholder; the VM overrides it to
    /// return the configured `-Xms` (`VmConfig::initial_heap_size`).
    fn initial_heap_bytes(&self) -> i64 {
        16 * 1024 * 1024
    }

    /// HotSpot's `[MinHeapSize, InitialHeapSize, MaxHeapSize]` for this
    /// command line, the figures its `gc,init` block prints (`Heap Min /
    /// Initial / Max Capacity`): `-Xms` / `-XX:MinHeapSize` or HotSpot's
    /// `min(8M, initial)`, `-Xms` clamped to `-Xmx`, and `-Xmx` itself --
    /// which [`Self::max_heap_bytes`] (`Runtime.maxMemory()`) reports less
    /// what the heap can never allocate (one survivor space on the
    /// Generational backend). JFR's `jdk.GCHeapConfiguration` and the
    /// `MaxHeapSize` / `InitialHeapSize` flag rows are these. gci i42/mx.
    /// Default (mocks): the initial size twice and [`Self::max_heap_bytes`].
    fn hotspot_heap_sizes(&self) -> [i64; 3] {
        let initial = self.initial_heap_bytes();
        [initial, initial, self.max_heap_bytes()]
    }

    /// `(DisableExplicitGC, ExplicitGCInvokesConcurrent)` as this VM applies
    /// them (`VmConfig::explicit_gc`), for JFR's `jdk.GCConfiguration`
    /// `isExplicitGCDisabled` / `isExplicitGCConcurrent`. gci i42/mx. Default
    /// (mocks): both off, HotSpot's defaults.
    fn explicit_gc_flags(&self) -> (bool, bool) {
        (false, false)
    }

    /// `(UseTLAB, MinTLABSize, TLABRefillWasteFraction)` as the command line
    /// set them (`VmConfig::tlab_flags`, HotSpot's defaults when not given),
    /// for JFR's `jdk.GCTLABConfiguration` `usesTLABs` / `minTLABSize` /
    /// `tlabRefillWasteLimit`. gci i43/lf. Default (mocks): HotSpot's
    /// defaults.
    fn hotspot_tlab_configuration(&self) -> (bool, i64, i64) {
        (true, 2048, 64)
    }

    /// Returns the number of classes currently loaded in the VM.
    fn loaded_class_count(&self) -> usize;

    /// Bytes of class metadata the VM holds for its live classes — a measured
    /// LOWER bound (the serviceability heap summary's `metaspace_used`), or
    /// `None` where there is no class store (mocks). Backs the JMX non-heap
    /// `MemoryUsage`; `None` makes the bean fall back to its class-count
    /// estimate. gc-common w3-f (2026-09-23).
    fn class_metadata_bytes(&self) -> Option<u64> {
        None
    }

    /// gci i33/ms: HotSpot 25's metaspace figures from one class-store walk,
    /// `[used, committed, class-space used, class-space committed,
    /// class-space max]` in bytes (`used` / `committed` cover both spaces, as
    /// HotSpot's `Metaspace` pool does; the class-space part is its
    /// `Compressed Class Space` pool). `None` (the default: mocks, or the
    /// VM's no-class-space shape): the pools fall back to
    /// [`Self::class_metadata_bytes`] with `committed == used` and an empty
    /// `Compressed Class Space`.
    fn metaspace_space_figures(&self) -> Option<[u64; 5]> {
        None
    }

    /// `-XX:MaxMetaspaceSize` in bytes, `None` when unbounded (HotSpot's
    /// default) or unknown (the default: mocks). The `Metaspace` pool's `max`.
    /// gci i15/nh.
    fn max_metaspace_bytes(&self) -> Option<u64> {
        None
    }

    /// The JIT's code cache: `(bytes of compiled code mapped now, configured
    /// cap or None when unbounded)`, for the `CodeHeap` pools. `None` (the
    /// default: mocks) is "no code cache". gci i15/nh.
    fn jit_code_cache_bytes(&self) -> Option<(u64, Option<u64>)> {
        None
    }

    /// zt t3/ob: the `init` each of the three `CodeHeap '...'` pools reports,
    /// when the VM states HotSpot's (ZGC: `CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT`).
    /// zt t30/nh (`CRATONVM_JMX_CODE_CACHE_HOTSPOT_LAYOUT`): HotSpot's whole
    /// code cache layout on every collector -- each heap's `init` from
    /// `InitialCodeCacheSize`, and the one `CodeCache` pool of an unsegmented
    /// code cache ([`jmx_non_heap_pool_names`]). `None` (the default: mocks)
    /// keeps gci i15/nh's figures (2 MiB on the non-profiled heap, 0 on the
    /// other two) and the three heaps.
    fn code_cache_layout(&self) -> Option<cratonvm_types::code_cache_layout::CodeCacheLayout> {
        None
    }

    /// Cumulative classes reclaimed by class-loader unloading.
    fn unloaded_class_count(&self) -> u64 {
        0
    }

    /// Returns the cumulative number of GC collections that have occurred.
    fn gc_collection_count(&self) -> u64;

    /// A counter that moves whenever an object address may come to name a
    /// different object (every moving or freeing collection, G1's cleanup
    /// reclaim and a Generational concurrent sweep slice, which book no
    /// collection). Address-keyed memos stamp
    /// and check THIS, never [`Self::gc_collection_count`] (a statistic).
    /// Only equality across two reads means anything. The default is the
    /// collection count, right for every backend but G1 and for test mocks;
    /// the VM answers `HeapRealm::address_epoch` (`VmHeap::address_epoch` plus
    /// the Generational concurrent sweep's slice count).
    /// Lane gcI wave 9 and gci i1/wr + i2/wr
    /// (`r15w4-mh2-address-memo-epoch-misses-g1-cleanup-patch`).
    fn gc_address_epoch(&self) -> u64 {
        self.gc_collection_count()
    }

    /// Accumulated collection time in milliseconds, for
    /// `GarbageCollectorMXBean.getCollectionTime()`.
    ///
    /// The default answers with [`Self::gc_collection_count`] — the historical
    /// stand-in, kept for contexts (mocks, backends) with no pause accounting,
    /// because callers such as H2's `Utils.collectGarbage()` spin until this
    /// value moves and a count moves once per collection. The VM overrides it
    /// with real accumulated pause time where the backend keeps one (the
    /// generational collector, since 2026-09-23: each pause rounded up to a
    /// whole millisecond, so it still advances on every collection).
    fn gc_collection_time_ms(&self) -> u64 {
        self.gc_collection_count()
    }

    /// The running collector's `GarbageCollectorMXBean`s, one per bean.
    ///
    /// gen r4w2/obs (2026-09-23). **Empty means "no per-bean model"**, and is
    /// the default: the JMX natives then keep their historical single-bean
    /// answer (`G1 Young Generation`, counting [`Self::gc_collection_count`]),
    /// so mocks and the G1/ZGC backends are unchanged. The VM answers for the
    /// generational backend with HotSpot Serial's two beans.
    fn gc_collectors(&self) -> Vec<GcCollectorInfo> {
        Vec::new()
    }

    /// The running collector's heap `MemoryPoolMXBean`s. Empty (the default)
    /// keeps the historical `Eden Space` / `Old Gen` pools whose usage is the
    /// JMM "undefined" sentinel. gen r4w2/obs; see [`Self::gc_collectors`].
    fn gc_memory_pools(&self) -> Vec<GcMemoryPoolInfo> {
        Vec::new()
    }

    /// The non-heap `MemoryPoolMXBean`s the VM measures, in HotSpot's pool
    /// order relative to each other (they are listed before the heap pools,
    /// as HotSpot 25 lists `Metaspace` before the ZGC pools). Each one's
    /// `manager_names` are plain (non-GC) memory managers, and its
    /// `collection_used` is meaningless (`getCollectionUsage()` is `null` for
    /// a non-heap pool). Empty (the default: mocks, and every backend but
    /// ZGC) keeps the heap-only lists. zs s3/ob2.
    fn non_heap_memory_pools(&self) -> Vec<GcMemoryPoolInfo> {
        Vec::new()
    }

    /// `MemoryImpl.getMemoryPools0` handed the [`Self::non_heap_memory_pools`]
    /// out, so a collection's `GcInfo` can name them from now on. The default
    /// does nothing. zs s3/ob2.
    fn note_non_heap_pools_published(&self) {}

    /// Heap pool `pool`'s peak `(used, committed)` as the collector sampled
    /// it at the edges of every collection since the last reset (HotSpot's
    /// `record_peak_memory_usage` at `gc_begin` / `gc_end`), for
    /// `MemoryPoolMXBean.getPeakUsage()`. `None` (the default: mocks, and
    /// backends that do not sample) leaves the peak to query-time samples.
    /// gen r5w2/obs6 (2026-09-26). gci i14/mp: the VM answers on every backend
    /// that describes its pools, and records the query's own sample first
    /// (HotSpot's `get_peak_memory_usage`), so the answer never falls.
    fn gc_pool_peak(&self, _pool: &str) -> Option<(i64, i64)> {
        None
    }

    /// `MemoryPoolMXBean.getPeakUsage()`'s one call: record `(used,
    /// committed)` -- the caller's own sample of `pool`, taken for the answer
    /// -- as a peak sample, then answer the peak ([`Self::gc_pool_peak`]).
    /// HotSpot's `get_peak_memory_usage` records the current usage before it
    /// answers. The default records nothing. gci i14/mp finding 13: the VM's
    /// `gc_pool_peak` took a second pool sample of its own for this.
    fn record_gc_pool_peak(&self, pool: &str, _used: i64, _committed: i64) -> Option<(i64, i64)> {
        self.gc_pool_peak(pool)
    }

    /// `MemoryPoolMXBean.resetPeakUsage()`: reset `pool`'s collector-sampled
    /// peak ([`Self::gc_pool_peak`]) to its current usage. The default does
    /// nothing. gen r5w2/obs6.
    fn reset_gc_pool_peak(&self, _pool: &str) {}

    /// Does heap pool `pool` support `(usage threshold, collection usage
    /// threshold)`? What its `MemoryPoolImpl` is constructed with
    /// (`isUsageThresholdSupported()` / `isCollectionUsageThresholdSupported()`).
    /// `true` is a promise to deliver the `MEMORY_THRESHOLD_EXCEEDED` /
    /// `MEMORY_COLLECTION_THRESHOLD_EXCEEDED` notifications, so the default
    /// (mocks, and every backend that does not check thresholds) is
    /// `(false, false)`. gen r5w3/obs7 (2026-09-26;
    /// `gengc-r5w2-obs6-heap-pools-support-no-usage-thresholds`).
    fn gc_pool_threshold_support(&self, _pool: &str) -> (bool, bool) {
        (false, false)
    }

    /// `MemoryPoolImpl.setUsageThreshold0` (`collection == false`) /
    /// `setCollectionThreshold0` (`true`): arm `pool`'s threshold at
    /// `threshold` bytes (`0` disarms it). Returns whether the running
    /// collector checks that threshold. The default does nothing and answers
    /// `false`. gen r5w3/obs7.
    fn set_gc_pool_threshold(&self, _pool: &str, _collection: bool, _threshold: i64) -> bool {
        false
    }

    /// `GarbageCollectorExtImpl.setNotificationEnabled(bean, enabled)`: the
    /// JDK calls this when `collector`'s bean gains its first
    /// `NotificationListener` or loses its last. Returns whether the running
    /// collector has such a bean. gen r4w3/obs2 (2026-09-23).
    ///
    /// The default (mocks, G1, ZGC) is `false` and does nothing: those
    /// backends send no GC notifications.
    fn set_gc_notification_enabled(&self, _collector: &str, _enabled: bool) -> bool {
        false
    }

    /// `collector`'s most recent completed collection, for
    /// `GarbageCollectorMXBean.getLastGcInfo()`; `None` before its first one
    /// and on backends without a per-bean model (the default).
    /// gen r4w3/obs2 (2026-09-23).
    fn gc_last_collection(&self, _collector: &str) -> Option<GcNotificationInfo> {
        None
    }

    /// Force a garbage collection cycle and run pending finalizers.
    /// Used by `System.gc()` / `Runtime.gc()`.
    fn force_gc(&mut self);

    /// `obj` was just allocated by the VM for the running program, where
    /// HotSpot posts JVMTI `VMObjectAlloc` (`Object.clone`,
    /// `Array.newInstance`, a reflective constructor): the C JVMTI table's
    /// event, when an agent listens (interpreter round i2 wave 63, lane L1).
    /// Answers where `obj` is afterwards (an agent's callback may collect).
    /// Default: `obj`, no event.
    fn jvmti_vm_object_alloc(&mut self, obj: ObjectRef) -> ObjectRef {
        obj
    }

    /// Whether [`Self::jvmti_vm_object_alloc`] would post an event now (a C
    /// JVMTI env listens for `VMObjectAlloc`). A native lane that allocates
    /// and constructs in ONE funnel call (`lang_invoke.rs`
    /// `mh_real_constructor_fast` / `mh_real_ctor_widening_serve`) declines
    /// while it is armed, so the JDK's own door posts at its
    /// `Unsafe.allocateInstance`, before `<init>` runs, as HotSpot does (gcj
    /// j1/jt, page `gci-i47w-oz47-jvmti-reflective-constructor-posts-no-vmobjectalloc`).
    /// Default: `false`.
    fn jvmti_vm_object_alloc_armed(&self) -> bool {
        false
    }

    /// Does this VM run a JIT (`VmConfig::jit_enabled`, as
    /// [`NativeMethodRegistry::jit_enabled`] answers at registration)? A
    /// native lane that only pays without a JIT declines when it does
    /// (interpreter round i9: `lang_invoke.rs` `mh_real_ctor_cast_adapter_fast`,
    /// which measured no gain with the JIT on). Default: `true`, so a context
    /// that does not know keeps such a lane out.
    fn vm_jit_enabled(&self) -> bool {
        true
    }

    /// A collection the VM itself needs (a native-side table that can only
    /// shed dead entries after one) -- NOT an explicit GC, so it ignores
    /// `-XX:+DisableExplicitGC` / `-XX:+ExplicitGCInvokesConcurrent`.
    /// Default: `force_gc()` (mocks). gc-common w8-g.
    fn force_gc_for_vm(&mut self) {
        self.force_gc();
    }

    /// JVMTI `ForceGarbageCollection`: [`Self::force_gc_for_vm`] reported
    /// with HotSpot's cause, `JvmtiEnv ForceGarbageCollection` (gci i27/ob).
    /// Default: `force_gc_for_vm()` (mocks).
    fn force_gc_for_jvmti(&mut self) {
        self.force_gc_for_vm();
    }

    /// gci i28/dg (`gci-i14mp-...` item 11b): the diagnostic commands the
    /// `com.sun.management:type=DiagnosticCommand` MBean lists (HotSpot's
    /// `jmm_GetDiagnosticCommands`). Default: none (mocks, a VM with no
    /// command table).
    fn dcmd_names(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// gci i28/dg: [`DcmdInfo`] for the command `name`, `None` when it is not
    /// one of [`Self::dcmd_names`]. Default: `None`.
    fn dcmd_info(&mut self, name: &str) -> Option<DcmdInfo> {
        let _ = name;
        None
    }

    /// gci i28/dg: run one diagnostic command line (`<name> <arguments>`) as
    /// HotSpot's `jmm_ExecuteDiagnosticCommand` does and answer its output;
    /// `Err` is the message of the `IllegalArgumentException` the caller
    /// throws (an unknown command: `Unknown diagnostic command`). The VM runs
    /// the command off the calling Java thread, which waits GC-safe. Default:
    /// `Err` for every line.
    fn dcmd_execute(&mut self, line: &str) -> Result<String, String> {
        let _ = line;
        Err("Unknown diagnostic command".to_string())
    }

    /// gci i47w/dh47 (`gci-i47w-j45-hotspotdiagnostic-dumpheap0-unsatisfied-link-FIXED-20261016`):
    /// `com.sun.management.internal.HotSpotDiagnostic.dumpHeap0(path, live)`,
    /// i.e. HotSpot's `jmm_DumpHeap0` -> `HeapDumper(live).dump(path)`: open
    /// `path` exclusively (an existing file is refused), then, when `live`,
    /// one full collection, then the HPROF walk at a pause. `Err` is the
    /// message of the `java.io.IOException` the caller throws (HotSpot's
    /// `os::strerror` text for a failed open, e.g. `File exists`). The VM runs
    /// the dump off the calling Java thread, which waits GC-safe. Default:
    /// `Err` (mocks).
    fn dump_heap_for_mxbean(&mut self, path: &str, live: bool) -> Result<(), String> {
        let _ = (path, live);
        Err("heap dump not supported".to_string())
    }

    /// `Runtime.runFinalization()` / `System.runFinalization()`: run every
    /// finalizer the VM has queued so far — on the VM's reference-delivery
    /// thread when one serves (waiting for it, bounded), inline otherwise —
    /// before returning. No collection. The default does nothing (mocks).
    /// gc-common w4-d (2026-09-23).
    fn run_finalization(&mut self) {}

    /// `JavaLangRefAccess.waitForReferenceProcessing()`: if the VM's
    /// reference-delivery thread has cleaner / finalizer work queued or in
    /// flight, wait for it (bounded) and return `true`; otherwise `false` at
    /// once — HotSpot's "the Reference Handler had work and I waited".
    /// `java.nio.Bits.reserveMemory` asks this before it falls back to
    /// `System.gc()`. The default is `false` (mocks; and every VM whose
    /// reference processing completes inside the collection).
    /// gc-common w5-d (2026-09-24).
    fn wait_for_reference_processing(&mut self) -> bool {
        false
    }

    /// Whether every `Reference` this VM's collector links into a
    /// `ReferenceQueue` is followed, after the pause, by the JDK's own wake-up
    /// (`synchronized (queue.lock) { lock.notifyAll(); }`, run by the VM's
    /// reference-delivery thread) — so a `ReferenceQueue.remove` waiter may
    /// sleep on the queue's `lock` instead of polling. `true` only while such a
    /// thread serves the VM and its policy records the wake-ups; the default
    /// (`false`: mocks, and every VM that does not deliver them) keeps the
    /// poll. gc-common w6-d (2026-09-24).
    fn reference_queue_wake_ups_delivered(&self) -> bool {
        false
    }

    /// Register `obj` for finalization if its class overrides `finalize()`;
    /// a no-op for any other class. HotSpot registers a finalizable instance
    /// when its `Object.<init>` returns and once in `JVM_Clone`; this is the
    /// hook for the native paths that mint such an instance
    /// (`Finalizer.register`, `Object.clone()`). The default does nothing
    /// (mocks). gc-common w5-d (2026-09-24).
    fn register_if_finalizable(&mut self, obj: ObjectRef) {
        let _ = obj;
    }

    /// JIT round 21 wave 10, lane finalize (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`,
    /// default ON): `obj` is an instance this door registered for
    /// finalization and whose constructor call has just returned an
    /// exception; `epoch_before_call` is [`Self::gc_address_epoch`] read
    /// before that call. When the VM's unwinder proved the constructor threw
    /// before `Object.<init>` returned (where HotSpot registers) and no
    /// collection has run since, the registration is withdrawn. The default
    /// does nothing (mocks).
    fn withdraw_unconstructed_finalizable(&mut self, obj: ObjectRef, epoch_before_call: u64) {
        let _ = (obj, epoch_before_call);
    }

    /// Withdraw the registration of a finalizable object when a constructor
    /// door abandons it before invoking `<init>` (for example, argument
    /// adaptation fails after allocation). The caller must prove no constructor
    /// frame was entered and pass the object's current address epoch. The
    /// default does nothing (mocks).
    fn withdraw_finalizable_before_constructor(&mut self, obj: ObjectRef, epoch: u64) {
        let _ = (obj, epoch);
    }

    /// Read a static field value by class and field index.
    ///
    /// `field_index` must be in range for `class_id`'s static field block
    /// (typically resolved via [`static_field_index_by_name`](Self::static_field_index_by_name)).
    /// The trait does NOT validate it (M4a): the implementation MUST
    /// bounds-check and MUST NOT read out of range — fail safe on a bad index.
    fn get_static_field(&self, class_id: ClassId, field_index: usize) -> Value;

    /// Write a static field value by class and field index.
    ///
    /// `field_index` must be in range for `class_id`'s static field block
    /// (typically resolved via [`static_field_index_by_name`](Self::static_field_index_by_name)).
    /// The trait does NOT validate it (M4a): the implementation MUST
    /// bounds-check and MUST NOT write out of range — fail safe on a bad index.
    fn set_static_field(&mut self, class_id: ClassId, field_index: usize, value: Value);

    /// JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`): ONE hardware
    /// read-modify-write of a static slot -- the cell's payload is read, `f`
    /// answers the value to store (`None`: store nothing, e.g. a failed
    /// compare), and a `LOCK CMPXCHG` of the payload stores it, retried only
    /// when the payload changed under it. So the update is atomic against
    /// every other store of the slot, compiled `putstatic` included, as
    /// HotSpot's static `VarHandle` RMWs are.
    ///
    /// `Some((witness, stored))`: the value `f` was given (the one the slot
    /// held at the linearization point) and whether the store happened.
    /// `None`: no hardware cell for the slot (the default for contexts that own
    /// no VM, a slot not yet published, a value whose kind is not the cell's,
    /// a switch off); nothing was stored and the caller takes its lock path.
    /// `f` may be called more than once and must be pure: no allocation, no
    /// Java.
    fn static_field_hw_update(
        &mut self,
        class_id: ClassId,
        field_index: usize,
        f: &mut dyn FnMut(Value) -> Option<Value>,
    ) -> Option<(Value, bool)> {
        let _ = (class_id, field_index, f);
        None
    }

    /// JIT round 21, lane atomics (STATICCAS21-2,
    /// `CRATONVM_JIT_UNSAFE_STATIC_RMW_INLINE`): the caller -- an `Unsafe`
    /// native on a static's `staticFieldBase` mirror -- resolved the `Unsafe`
    /// offset `offset` to `class_id`'s static `field_index` and is about to
    /// update it with [`static_field_hw_update`](Self::static_field_hw_update).
    /// The VM may publish that pair for compiled code, whose `Unsafe` RMW
    /// prefixes then perform the same hardware atomic on the static's payload
    /// word inline. `desc` answers the static's declared descriptor byte; it
    /// is asked only when the VM would publish (a whole `int` / `long` static
    /// only). The default is a no-op: contexts that own no compiled code.
    fn note_unsafe_static_atomic_target(
        &self,
        offset: usize,
        class_id: ClassId,
        field_index: usize,
        desc: &dyn Fn() -> Option<u8>,
    ) {
        let _ = (offset, class_id, field_index, desc);
    }

    /// j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): a static
    /// `VarHandle` native resolved the handle `vh` to `class_id`'s static
    /// `field_index`, and `class_id` is initialized. `writable`: the handle is
    /// read-write and not invoke-exact (its row says so). The VM may then file
    /// that static for every `static final` holding `vh`, so compiled
    /// constant-handle sites perform the access inline. `desc` answers the
    /// static's declared descriptor byte; asked only when the VM would file.
    /// The default is a no-op: contexts that own no compiled code.
    fn note_static_varhandle_target(
        &self,
        vh: ObjectRef,
        class_id: ClassId,
        field_index: usize,
        writable: bool,
        desc: &dyn Fn() -> Option<u8>,
    ) {
        let _ = (vh, class_id, field_index, writable, desc);
    }

    /// JIT round 21 (lane staticcas): compare-and-exchange on a STATIC field,
    /// ONE atomic step against every other access of the static (`putstatic`
    /// in any tier, [`set_static_field`](Self::set_static_field), another
    /// thread's CAS). Returns `(witness, swapped)`: the value the static held,
    /// and whether it equalled `expected` (by [`static_cas_values_equal`]) and
    /// was replaced by `new_val`.
    ///
    /// Before this existed every `Unsafe` / `VarHandle` atomic that landed on
    /// a static was a `get_static_field` then a `set_static_field`, so two
    /// threads that both read `v` both "succeeded"
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    /// The VM implements it as one hardware CAS on the static's storage word
    /// (`vm::compare_and_exchange_static_shared`). This default -- a read, a
    /// compare and a write -- is for the single-threaded test contexts only.
    fn compare_and_exchange_static_field(
        &mut self,
        class_id: ClassId,
        field_index: usize,
        expected: Value,
        new_val: Value,
    ) -> (Value, bool) {
        let cur = self.get_static_field(class_id, field_index);
        let ok = static_cas_values_equal(cur, expected);
        if ok {
            self.set_static_field(class_id, field_index, new_val);
        }
        (cur, ok)
    }

    /// Write a static field by class name and field name.
    /// Resolves the class and finds the static field index by name.
    /// No-op if the class or field cannot be found.
    fn set_static_field_by_name(&mut self, class_name: &str, field_name: &str, value: Value) {
        if let Some(class_id) = self.class_id_by_name(class_name) {
            if let Some(idx) = self.static_field_index_by_name(class_id, field_name) {
                self.set_static_field(class_id, idx, value);
            }
        }
    }

    /// `System.setIn0/setOut0/setErr0` wrote `System.<field_name>` (`"in"`,
    /// `"out"` or `"err"`), null included. The VM then reads a null field as
    /// null instead of substituting its canonical stream (round 15 wave 2,
    /// lane jdkonly: `System.setOut(null); System.out == null` is true on
    /// HotSpot). The default is a no-op for contexts that own no VM.
    fn mark_system_stream_written(&self, field_name: &str) {
        let _ = field_name;
    }

    /// Find the static field index for a field by name.  Returns `None` if the
    /// class doesn't have a static field with that name.
    fn static_field_index_by_name(&self, class_id: ClassId, field_name: &str) -> Option<usize> {
        let _ = (class_id, field_name);
        None
    }

    /// The line separator to PUT ON THE WIRE -- the
    /// `java.lang.System.lineSeparator` static, which `initPhase1` captures
    /// once from the `line.separator` property.
    ///
    /// **Not `get_system_property("line.separator")`.** The JDK reads that
    /// property exactly once, at boot; `PrintStream.println`,
    /// `BufferedWriter.newLine`, `Properties.store` and `Files.write` all go
    /// through `System.lineSeparator()`, which is a plain field read. A later
    /// `System.setProperty("line.separator", ...)` -- or a write straight into
    /// the `System.props` object -- therefore changes nothing on HotSpot, and
    /// used to re-terminate every subsequent line on this VM.
    ///
    /// MEASURED on `SysPropsVmConsumers.java` against HotSpot
    /// 25.0.4+7: the property write was visible to `System.lineSeparator()`
    /// (`changed` against HotSpot's `unchanged`) AND to `println`, and under
    /// `--real-jdk` the two disagreed with each other -- an OBJECT write moved
    /// `println` while the accessor kept answering the old value, so the
    /// restore could put neither back and the probe's own output came out
    /// joined by the test separator. Two consumers of one property, two
    /// authorities, which is the split the retired
    /// `the-system-props-static-field-is-null` write-up's section 12
    /// predicted -- and its prescription (make the Rust store a cache of the
    /// `Properties` object) does not reach it, because the JDK reads neither.
    ///
    /// The live store is the fallback for the window before `initPhase1`
    /// step 3 runs -- which is where a `-Dline.separator=` override is already
    /// sitting, so that override is still honoured -- and the compile-time
    /// platform default the fallback for an image whose `java/lang/System`
    /// declares no such field (synthetic-JDK mode). It must be `\r\n` on
    /// Windows; several hand-rolled copies of this fallback carried `\n`.
    fn captured_line_separator(&self) -> String {
        if let Some(cid) = self.class_id_by_name("java/lang/System") {
            if let Some(idx) = self.static_field_index_by_name(cid, "lineSeparator") {
                if let Value::Object(Some(s)) = self.get_static_field(cid, idx) {
                    if let Some(text) = self.read_string(s) {
                        if !text.is_empty() {
                            return text;
                        }
                    }
                }
            }
        }
        self.get_system_property("line.separator")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| if cfg!(windows) { "\r\n" } else { "\n" }.to_string())
    }

    /// Get the file descriptor table for I/O operations.
    fn fd_table(&self) -> &crate::fd_table::FileDescriptorTable;

    // -- Panama FFI (JEP 454, Java 25) --

    /// Allocate off-heap memory. Returns (alloc_id, raw_pointer) or None on failure.
    ///
    /// # Security (M4b — raw pointer use-after-free)
    ///
    /// The returned `*mut u8` is a *bare base pointer* with no length and no
    /// lifetime tie to `alloc_id`: a subsequent
    /// [`free_native_memory`](Self::free_native_memory) of the same id frees
    /// the block, leaving any retained copy of this pointer **dangling**.
    /// Callers MUST NOT retain the bare pointer across any operation that
    /// could free the allocation, and MUST bounds-check their own offset/len
    /// before dereferencing. For validated access, resolve through the
    /// implementation's [`NativeMemoryTable`](crate::ffi::NativeMemoryTable)
    /// (`get_ptr_checked` / generation-tagged `get_ptr_checked_handle`) rather
    /// than caching this pointer.
    fn allocate_native_memory(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)>;

    /// Free off-heap memory by allocation ID.
    ///
    /// # Security (M4b)
    ///
    /// After this call any `*mut u8` previously obtained for `alloc_id` via
    /// [`allocate_native_memory`](Self::allocate_native_memory) is dangling;
    /// dereferencing it is undefined behaviour. The backing
    /// [`NativeMemoryTable`](crate::ffi::NativeMemoryTable) advances its
    /// generation on free so a recycled id cannot be confused with this
    /// freed allocation by a stale handle.
    fn free_native_memory(&mut self, alloc_id: i64);

    /// Load a native library. Returns library index or error.
    fn load_native_library(
        &mut self,
        path: &str,
    ) -> Result<i64, cratonvm_types::error::MethodCallFailed>;

    /// Record the JNI-link visibility of the library at `lib_index`:
    /// `Some(loader_id)` for a library loaded by a class loader (`0` = bootstrap),
    /// or `None` for a raw library (`RawNativeLibraries` / `SymbolLookup.libraryLookup`),
    /// which is invisible to lazy JNI linking.
    fn set_native_library_loader(&mut self, lib_index: i64, loader_id: Option<u32>) {
        let _ = (lib_index, loader_id);
    }

    /// Find a symbol in a loaded library. Returns the symbol address.
    /// lib_index -1 means search the default/system library.
    fn find_native_symbol(&self, lib_index: i64, name: &str) -> Option<usize>;

    /// `name` in the process's C runtime ONLY -- none of the libraries
    /// [`load_native_library`](Self::load_native_library) loaded -- which is
    /// what `Linker.defaultLookup()` searches on HotSpot (round 14 wave 3, lane
    /// ffm; `r14w1-ffm-symbol-lookup-search-set-and-scope`). The outer `None`
    /// means this context cannot answer the question (the default), and the
    /// caller falls back to `find_native_symbol(-1, name)`; `Some(None)` is
    /// "not in the C runtime".
    fn find_process_native_symbol(&self, _name: &str) -> Option<Option<usize>> {
        None
    }

    /// Release a library previously returned by
    /// [`load_native_library`](Self::load_native_library) — the backing for
    /// `jdk.internal.loader.RawNativeLibraries.unload0`, whose real body
    /// `dlclose`s / `FreeLibrary`s the handle.
    ///
    /// Returns `true` if `lib_index` named a live library that this call
    /// retired, `false` for an out-of-range index, an already-unloaded one, or
    /// an implementation that cannot unload.
    ///
    /// # Contract
    ///
    /// This is a *logical* unload: implementations MUST make subsequent
    /// [`find_native_symbol`](Self::find_native_symbol) calls for the index
    /// fail, but are NOT required to unmap the library. Keeping the mapping
    /// resident is the safe direction of the two errors — a caller can still
    /// hold function pointers obtained from an earlier lookup (a bound FFM
    /// downcall stub does), and unmapping underneath those segfaults, while a
    /// retained mapping costs only address space. Indices are never recycled
    /// while a load is live, so an unloaded index stays unloaded until
    /// `load_native_library` hands that index out again.
    ///
    /// The default implementation is a no-op returning `false`, preserving the
    /// behaviour of implementations that have no unload path.
    fn unload_native_library(&mut self, _lib_index: i64) -> bool {
        false
    }

    /// Register an upcall entry (Java callback for C). Returns the slot index.
    ///
    /// # Security (M4c — confused deputy via slot reuse)
    ///
    /// The returned bare slot index does NOT distinguish between successive
    /// occupants of a reused slot: after the registration is dropped and the
    /// slot re-registered, the same index resolves to a *different* Java
    /// object. The backing
    /// [`UpcallTable`](crate::ffi::UpcallTable) generation-tags every
    /// registration; implementations that hand a trampoline index to native
    /// code SHOULD carry the generation (see
    /// `UpcallTable::register_handle` / `get_checked`) so a stale trampoline
    /// fails closed rather than invoking the wrong callback.
    fn register_upcall(&mut self, entry: crate::ffi::UpcallEntry) -> usize;

    /// Get upcall info by slot index. Returns (target, param_kinds, return_kind).
    ///
    /// # Security (M4c)
    ///
    /// Resolving by bare `slot` cannot detect slot reuse. If the slot was
    /// removed and re-registered since the trampoline was minted, this may
    /// return info for a *different* callback (confused deputy). Where the
    /// caller holds a generation-tagged handle, prefer resolving it through
    /// the implementation's [`UpcallTable::get_checked`](crate::ffi::UpcallTable::get_checked)
    /// so a stale handle fails closed.
    fn get_upcall_info(&self, slot: usize) -> Option<(ObjectRef, Vec<i32>, i32)>;

    /// Record a JFR thread sleep event. Called by Thread.sleep implementations.
    /// Default is no-op; the VM overrides this with the real JFR recorder.
    fn record_thread_sleep(&mut self, _sleep_nanos: i64, _actual_duration_nanos: u64) {}

    /// Record a JFR file read event.
    fn record_file_read(&mut self, _fd: i32, _bytes_read: i64, _eof: bool, _duration_nanos: u64) {}

    /// Record a JFR file write event.
    fn record_file_write(&mut self, _fd: i32, _bytes_written: i64, _duration_nanos: u64) {}
}

/// Complete native-call capability set. Native implementations are split
/// into the narrow supertraits above; this marker exists only at the
/// legacy callback ABI boundary.
pub trait NativeContext: NativeSystemAccess + NativeExceptionAccess + NativeGpuAccess {}

impl<T> NativeContext for T where
    T: NativeSystemAccess + NativeExceptionAccess + NativeGpuAccess + ?Sized
{
}

/// The target of an export of a non-boot layer's module
/// ([`NativeClassAccess::layer_module_add_export`]), by identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerModuleEdgeTarget<'a> {
    /// `addExportsToAll0`: every module.
    Everyone,
    /// `addExportsToAllUnnamed0`: every unnamed module.
    AllUnnamed,
    /// A named module that is not a user-defined loader's layer module
    /// (the boot layer's, by name).
    Named(&'a str),
    /// A module of a non-boot layer defined to the user-defined loader
    /// `loader_ns`.
    Layer { loader_ns: u32, name: &'a str },
    /// The unnamed module of the loader with namespace id `loader_ns`
    /// (`ClassLoaderId::to_native_id`).
    Unnamed { loader_ns: u32 },
}

/// What [`NativeHeapAccess::instantiate`] runs on the instance it allocates
/// (round 15 wave 4, lane jdkonly6; JO5-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeConstruct {
    /// Nothing: `Unsafe.allocateInstance`, JNI `AllocObject`.
    NoConstructor,
    /// The nullary `<init>` of this ANCESTOR class, `invokespecial`-style:
    /// the serialization constructor
    /// (`ReflectionFactory.newConstructorForSerialization`).
    AncestorInit(ClassId),
}

/// The `StackTraceElement` fill's behaviour switches, all default ON with
/// the `on_unless_zero` rule (`runtime_flag_default_on`), read once per VM
/// ([`NativeMethodRegistry::stack_trace_fill_switches`] through
/// [`NativeSystemAccess::stack_trace_fill_switches`]). Interpreter round i3
/// wave 77, lane L2: each was a process `OnceLock` (or an env read per array
/// or per hidden-class frame), so two VMs in one process could not differ on
/// them (AGENTS.md: no process globals for compatibility state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackTraceFillSwitches {
    /// `CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS` (round 14 wave 2): a
    /// captured frame's element names the capture's exact class while it
    /// still names the frame's class, not a by-name lookup's.
    pub exact_class: bool,
    /// `CRATONVM_STACK_TRACE_MODULE_PREFIX` (round 13 wave 13): a filled
    /// element names its class's module and loader, and the native printers
    /// print the `loader/module@version/` prefix.
    pub module_prefix: bool,
    /// `CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN` (round 14 wave 2): every
    /// origin field HotSpot's `fill_in` stores, plus the `format` bits.
    pub full_origin: bool,
    /// `CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO` (round 14 wave 3): one
    /// array's elements copy the origin fields of an earlier element of the
    /// same class.
    pub origin_memo: bool,
    /// `CRATONVM_STACK_TRACE_BINARY_NAMES` (round 15 wave 2): a hidden
    /// class's frame keeps its `/0x...` suffix in the dotted name.
    pub binary_names: bool,
    /// `CRATONVM_STACK_TRACE_WALK_ELEMENT_INTERNED_NAMES` (interpreter round
    /// i4 wave 91, lane L2; page
    /// `interpreter-i77-L2-proposal-walk-element-keeps-interned-names-KEPT-OFF-20261019.md`;
    /// **default OFF**, `truthy_word`): the first element of a walked frame
    /// (`lang_stackwalker::fill_element_from_walk`, walk-lines frames) stores
    /// the frame's own interned `name` String as its `methodName` instead of
    /// decoding it and interning the text again, and keeps the class's cached
    /// dotted name without copying it. The only switch of this struct that
    /// is off by default.
    pub walk_element_interned_names: bool,
}

impl Default for StackTraceFillSwitches {
    /// Every switch at its default (on, but
    /// `walk_element_interned_names`, off).
    fn default() -> Self {
        Self {
            exact_class: true,
            module_prefix: true,
            full_origin: true,
            origin_memo: true,
            binary_names: true,
            walk_element_interned_names: false,
        }
    }
}

impl StackTraceFillSwitches {
    /// Read the switches from the environment: the five default-on ones with
    /// the `on_unless_zero` rule the per-site reads used, and
    /// `walk_element_interned_names` with the `truthy_word` rule.
    pub fn from_env() -> Self {
        fn on(key: &str) -> bool {
            cratonvm_types::flags::runtime_flag_default_on(key)
        }
        Self {
            exact_class: on("CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS"),
            module_prefix: on("CRATONVM_STACK_TRACE_MODULE_PREFIX"),
            full_origin: on("CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN"),
            origin_memo: on("CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO"),
            binary_names: on("CRATONVM_STACK_TRACE_BINARY_NAMES"),
            walk_element_interned_names: false,
        }
    }
}

/// A class's module, by identity
/// ([`NativeClassAccess::module_identity_of_class`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassModuleIdentity {
    /// A module of the VM's name map (the boot layer, `--module-path`, and
    /// whatever else [`NativeClassAccess::module_name_of_class`] labels).
    Named(String),
    /// A module of a non-boot `ModuleLayer` defined to the user-defined
    /// loader `loader_ns`; `version` is its descriptor's version, when it has
    /// one (`Module.defineModule0`'s `version` argument).
    Layer {
        loader_ns: u32,
        name: String,
        version: Option<String>,
    },
    /// The unnamed module of the class's loader.
    Unnamed,
}

impl ClassModuleIdentity {
    /// The module's name, `None` for the unnamed module.
    pub fn name(&self) -> Option<&str> {
        match self {
            ClassModuleIdentity::Named(name) | ClassModuleIdentity::Layer { name, .. } => {
                Some(name)
            }
            ClassModuleIdentity::Unnamed => None,
        }
    }

    /// HotSpot's spelling of the module in a message (`module m` /
    /// `unnamed module`; HotSpot appends an identity hash to the unnamed
    /// form, which is not reproduced).
    pub fn describe(&self) -> String {
        match self.name() {
            Some(name) => format!("module {name}"),
            None => "unnamed module".to_string(),
        }
    }

    /// [`Self::describe`] with a layer module's `@version`, as
    /// `Klass::class_in_module_of_loader` prints it (a `Named` module carries
    /// no version here: see `NativeClassAccess::module_clause_of_class`).
    /// Interpreter round i4 wave 99, lane L5.
    pub fn describe_with_version(&self) -> String {
        match self {
            ClassModuleIdentity::Layer {
                name,
                version: Some(version),
                ..
            } if !version.is_empty() => format!("module {name}@{version}"),
            _ => self.describe(),
        }
    }
}

/// One module question about the modules of two classes, asked by the
/// modules' identities ([`NativeClassAccess::module_access`];
/// `cratonvm_classloading::access_control::module_access`). Interpreter
/// round i1 wave 47, lane L6
/// (`i45-L5-proposal-one-module-access-question-by-identity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleQuestion {
    /// Is the target's module the accessor's own (`targetModule ==
    /// callerModule`)?
    Same,
    /// Does the accessor's module READ the target's (`Module.canRead`)?
    Read,
    /// Does the target's module EXPORT the target's package to the
    /// accessor's module (`Module.isExported(pn, m)`)?
    Export,
    /// Does it OPEN that package to the accessor's module
    /// (`Module.isOpen(pn, m)`)?
    Open,
    /// Does the target's module export the target's package to EVERY module
    /// (`Module.isExported(pn)`)? The accessor is ignored. Interpreter round
    /// i1 wave 49, lane L6
    /// (`interpreter-L6-proposal-an-unqualified-export-question-by-identity-FIXED-20261013.md`).
    ExportAll,
    /// Does it open that package to every module (`Module.isOpen(pn)`)? The
    /// accessor is ignored.
    OpenAll,
}

/// Annotation data extracted from class file attributes.
#[derive(Debug, Clone)]
pub struct AnnotationData {
    /// The annotation type descriptor (e.g. "Ljava/lang/Override;")
    pub type_descriptor: String,
    /// Element-value pairs: (name, value_representation)
    pub elements: Vec<(String, AnnotationElementValue)>,
}

/// Which member annotation attribute
/// [`NativeClassAccess::member_raw_annotation_attributes`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawAnnotationAttr {
    /// `RuntimeVisibleAnnotations` (JVMS 4.7.16) -- the `annotations` field.
    Annotations,
    /// `RuntimeVisibleParameterAnnotations` (4.7.18) -- `parameterAnnotations`.
    ParameterAnnotations,
    /// `AnnotationDefault` (4.7.22) -- `Method.annotationDefault`.
    AnnotationDefault,
    /// `RuntimeVisibleTypeAnnotations` (4.7.20) -- `getTypeAnnotationBytes0`.
    TypeAnnotations,
}

/// A `MethodParameters` entry's name as `JVM_GetMethodParameters` checks it
/// (see [`NativeClassAccess::method_parameters_attribute`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodParameterName {
    /// `name_index` 0: the parameter has no name (a null `Parameter.name`).
    Unnamed,
    /// The `CONSTANT_Utf8` the entry names (possibly empty).
    Name(String),
    /// `name_index` past the constant pool: HotSpot's `bounds_check` throws
    /// `IllegalArgumentException("Constant pool index out of bounds")`.
    OutOfBounds,
    /// `name_index` names an entry that is not a `CONSTANT_Utf8`:
    /// `IllegalArgumentException("Wrong type at constant pool index")`.
    NotUtf8,
}

/// One constant pool entry as `jdk.internal.reflect.ConstantPool` sees it
/// (see [`NativeClassAccess::reflect_constant_pool_entry`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ReflectConstant {
    /// Index 0, out of range, or the second slot of a long/double.
    Invalid,
    /// `CONSTANT_Utf8`, as UTF-16 code units (lone surrogates preserved).
    Utf8(Vec<u16>),
    /// `CONSTANT_Integer`.
    Integer(i32),
    /// `CONSTANT_Float`.
    Float(f32),
    /// `CONSTANT_Long`.
    Long(i64),
    /// `CONSTANT_Double`.
    Double(f64),
    /// `CONSTANT_String`, as UTF-16 code units.
    String(Vec<u16>),
    /// `CONSTANT_Class`, by internal name.
    Class(String),
    /// `CONSTANT_Fieldref` (9), `CONSTANT_Methodref` (10) or
    /// `CONSTANT_InterfaceMethodref` (11): the pool indices it holds and the
    /// names they lead to (the class's internal name, the member's name and
    /// descriptor), which `ConstantPool.getClassRefIndexAt0`,
    /// `getNameAndTypeRefIndexAt0` and `getMemberRefInfoAt0` answer
    /// (interpreter round i5 wave 106, lane L3).
    MemberRef {
        /// The JVMS tag, 9, 10 or 11.
        tag: u8,
        /// `class_index`.
        class_index: u16,
        /// `name_and_type_index`.
        name_and_type_index: u16,
        /// The internal name `class_index`'s `CONSTANT_Class` names.
        class_name: String,
        /// The member's name.
        name: String,
        /// The member's descriptor.
        descriptor: String,
    },
    /// `CONSTANT_NameAndType` (12): the name and the descriptor.
    NameAndType {
        /// The name.
        name: String,
        /// The descriptor.
        descriptor: String,
    },
    /// `CONSTANT_Dynamic` (17) or `CONSTANT_InvokeDynamic` (18): its
    /// `name_and_type_index` (`getNameAndTypeRefIndexAt0` answers it).
    DynamicRef {
        /// The JVMS tag, 17 or 18.
        tag: u8,
        /// `name_and_type_index`.
        name_and_type_index: u16,
    },
    /// Any other entry, by its JVMS tag.
    Other(u8),
}

/// A tree of TYPE_USE annotations mirroring the nested-generic shape of a
/// reified `Type`, keyed by `type_argument_index` at each nesting level
/// (JVMS 4.7.20.2's `type_path`).
///
/// `.anns` holds the annotations whose `type_path` ends exactly at this
/// node; `.children[i]` is the subtree reached by descending into the i-th
/// type argument. A plain (non-generic) annotated type has `anns` populated
/// and `children` empty; a nested generic like
/// `ValueExtractor<ArgumentValue<@ExtractedValue ?>>` needs two levels:
/// `children[0]` (the `ArgumentValue<?>` argument) has its own
/// `children[0]` (the wildcard `?`) carrying `@ExtractedValue` in `anns`.
#[derive(Debug, Clone, Default)]
pub struct TypeArgAnnotations {
    /// Annotations directly on this node (empty remaining `type_path`).
    pub anns: Vec<AnnotationData>,
    /// Per-type-argument subtrees, indexed by `type_argument_index`.
    pub children: Vec<TypeArgAnnotations>,
}

/// A simplified representation of an annotation element value.
#[derive(Debug, Clone)]
pub enum AnnotationElementValue {
    /// A constant int/byte/char/short/boolean value.
    Int(i32),
    /// A constant long value.
    Long(i64),
    /// A constant float value.
    Float(f32),
    /// A constant double value.
    Double(f64),
    /// A string value.
    StringVal(String),
    /// An enum constant: (type_descriptor, const_name).
    Enum(String, String),
    /// A class literal: descriptor string.
    Class(String),
    /// A nested annotation.
    Annotation(AnnotationData),
    /// An array of values.
    Array(Vec<AnnotationElementValue>),
}

/// An entry in a captured Java stack trace.
///
/// WP1.9: `byte_code_index` (-1 when unknown, e.g. for native frames or
/// synthetic bootstrap frames) is populated from each frame's `last_instr_pc`
/// when available and used by `java.lang.StackWalker.StackFrame.getByteCodeIndex()`.
/// Which JDK method a `WrongMethodTypeException` a `MethodHandle` door raises
/// natively stands for, for [`NativeThreadAccess::set_mh_wmte_raise_hint`]
/// (JIT round 16 wave 13, lane wmte). HotSpot raises each in real JDK code,
/// so its trace starts there (JDK 25.0.3, measured by `R16MhWmteFrames`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MhWmteRaise {
    /// `invokeExact`'s type check: `Invokers.newWrongMethodTypeException` at
    /// its `new WrongMethodTypeException`, under `Invokers.checkExactType`
    /// (`handle's method type .. but found ..`).
    ExactType,
    /// `asType`'s refusal: `MethodHandle.asTypeUncached` at its
    /// `new WrongMethodTypeException`, under `setAsTypeCache` and `asType`
    /// (`cannot convert MethodHandle.. to ..`); below them the capture lists
    /// `Invokers.checkGenericType` for an `invoke` call and
    /// `invokeWithArguments` for that one, read from the caller's call.
    AsType,
}

/// The reflective door a thread is inside, for
/// [`NativeExceptionAccess::enter_reflective_call`]: which JDK frames a stack
/// capture lists for it (interpreter round i1 wave 43, lane L3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflectiveCallKind {
    /// `java.lang.reflect.Method.invoke(Object, Object[])`.
    MethodInvoke,
    /// `java.lang.reflect.Constructor.newInstance(Object[])`.
    ConstructorNewInstance,
    /// `java.security.AccessController.doPrivileged(PrivilegedAction)`, which
    /// the VM serves with a bridge that runs the action itself (interpreter
    /// round i1 wave 49, lane L3): its one frame, at its `run()` call.
    DoPrivileged,
    /// `AccessController.doPrivileged(PrivilegedExceptionAction)`: its frame,
    /// and for the `PrivilegedActionException` the bridge wraps a checked
    /// exception in, the frames of the JDK's own `wrapException`.
    DoPrivilegedException,
    /// `AccessController.doPrivileged(PrivilegedAction, AccessControlContext)`.
    DoPrivilegedInContext,
    /// A `StackWalker.walk` the VM serves natively, while the native runs the
    /// JDK's `AbstractStackWalker.doStackWalk` (the JDK pipeline): the frames
    /// HotSpot runs between `doStackWalk` and the caller (`StackWalker.walk`,
    /// `AbstractStackWalker.walk`, `walkHelper`, `beginStackWalk` and the
    /// native `callStackWalk`). JIT round 20, lane walkcompat
    /// (`r19-nested-walk-omits-the-jdk-walk-frames-FIXED-20261012.md`).
    StackWalkerWalk,
    /// A natively served `StackWalker.walk` that calls the walk's function
    /// itself: [`Self::StackWalkerWalk`]'s frames, then `doStackWalk` and
    /// `StackFrameTraverser.consumeFrames`.
    StackWalkerWalkFunction,
    /// A natively served `StackWalker.forEach` running the action through the
    /// JDK stream's `forEach` (the JDK pipeline): `StackWalker.forEach`, the
    /// walk frames down to `consumeFrames`, then `StackWalker.lambda$forEach$0`.
    StackWalkerForEachStream,
    /// A natively served `StackWalker.forEach` that calls the action itself:
    /// [`Self::StackWalkerForEachStream`]'s frames, then
    /// `ReferencePipeline$Head.forEach` and `StackFrameTraverser.forEachRemaining`.
    StackWalkerForEachAction,
    /// `MethodHandle.invokeWithArguments(Object...)`, which the VM serves with
    /// an `Intrinsic` that runs the target itself (interpreter round i5 wave
    /// 104, lane L3; `L3W104WalkOptionsThroughHandles` row
    /// `invokeWithArguments`): its one visible frame, at its `invokeExact` of
    /// the spread invoker. Listed in every view (it is neither hidden nor a
    /// reflection frame); never on top of a throwable the native raised
    /// itself (the served-raise chain names that frame).
    MethodHandleInvokeWithArguments,
    /// `MethodHandle.invokeWithArguments(List)`: its frame at its call of the
    /// array overload, then [`Self::MethodHandleInvokeWithArguments`]'s.
    MethodHandleInvokeWithArgumentsList,
}

impl ReflectiveCallKind {
    /// Is this a VM-served JDK Java method whose visible frames a capture
    /// lists in every view, named afresh (no memo slot), with no raise of its
    /// own: a served `StackWalker` walk or a served
    /// `MethodHandle.invokeWithArguments` (interpreter round i5 wave 104)?
    pub fn is_served_java_frames(self) -> bool {
        self.is_stack_walk() || self.is_invoke_with_arguments()
    }

    /// Is this a served `MethodHandle.invokeWithArguments`?
    pub fn is_invoke_with_arguments(self) -> bool {
        matches!(
            self,
            Self::MethodHandleInvokeWithArguments | Self::MethodHandleInvokeWithArgumentsList
        )
    }

    /// Is this a natively served `StackWalker.walk` / `forEach` (not a
    /// reflective door)? Its frames are listed in every view, as the
    /// `doPrivileged` bridges' are; it raises nothing itself.
    pub fn is_stack_walk(self) -> bool {
        matches!(
            self,
            Self::StackWalkerWalk
                | Self::StackWalkerWalkFunction
                | Self::StackWalkerForEachStream
                | Self::StackWalkerForEachAction
        )
    }

    /// Is this one of the `AccessController.doPrivileged` bridges (not a
    /// reflective door)? Their frames are listed in every view, and a VM may
    /// switch them off on its own (`CRATONVM_STANDIN_PRIVILEGED_FRAMES`).
    pub fn is_privileged(self) -> bool {
        matches!(
            self,
            Self::DoPrivileged | Self::DoPrivilegedException | Self::DoPrivilegedInContext
        )
    }
}

/// Where a field `java.lang.Thread` declares lives on one thread, for
/// [`NativeHeapAccess::thread_declared_field_slot`] (round 15 wave 4, lane
/// threadvm; TR4W3-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadDeclaredField {
    /// Address it by name: the thread is a `java.lang.Thread` itself (its
    /// by-name field is its own), its class chain does not reach
    /// `java.lang.Thread`, or `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS=0`.
    ByName,
    /// `java.lang.Thread`'s own slot, on a subclass instance.
    Declared(usize),
    /// A subclass instance, and `java.lang.Thread` declares no such field: a
    /// subclass's own field of that name is neither read nor written.
    Absent,
}

/// Which argument check of a reflective call failed, for
/// [`NativeExceptionAccess::note_reflective_check`]: where HotSpot's trace of
/// the check's throwable starts (JDK 25's `DirectMethodHandleAccessor` /
/// `DirectConstructorHandleAccessor`, measured by
/// `L3W46ReflectiveChecks.java`). Interpreter round i1
/// wave 46, lane L3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflectiveCheck {
    /// The argument count (`checkArgumentCount`, or the constructor
    /// accessor's own test): an `IllegalArgumentException`.
    ArgumentCount,
    /// A receiver that is not an instance of the declaring class
    /// (`checkReceiver`): an `IllegalArgumentException`.
    Receiver,
    /// A null receiver met by `Method.invoke`'s access check (`obj.getClass()`;
    /// the method is not `setAccessible(true)`): a `NullPointerException`.
    NullReceiverAtAccessCheck,
    /// A null receiver met by `checkReceiver` (the access check is
    /// overridden): a `NullPointerException`.
    NullReceiverAtReceiverCheck,
    /// An argument of the wrong type (the accessor's `ClassCastException`
    /// arm): an `IllegalArgumentException`.
    ArgumentType,
    /// A null argument for a primitive parameter of descriptor `char` (`'I'`,
    /// `'J'`, ...; the accessor's `NullPointerException` arm): an
    /// `IllegalArgumentException`, whose cause, a `NullPointerException`, is
    /// raised in `sun.invoke.util.ValueConversions.unbox<Wrapper>`.
    NullToPrimitive(char),
    /// An argument of the wrong type to a CONSTRUCTOR (the constructor
    /// accessor's `ClassCastException` arm, which keeps the cast failure as
    /// the `IllegalArgumentException`'s cause, where `Method.invoke`'s arm
    /// drops it): an `IllegalArgumentException` listed as [`Self::ArgumentType`]
    /// is, whose cause, a `ClassCastException`, is raised where
    /// [`ReflectiveArgumentCast`] says. Interpreter round i1 wave 47, lane L3.
    ArgumentCast(ReflectiveArgumentCast),
    /// `Constructor.newInstance` of an abstract class's constructor: JDK 25's
    /// `ReflectionFactory.newConstructorAccessor` hands out an
    /// `InstantiationExceptionConstructorAccessorImpl(null)`, whose
    /// `newInstance` raises the message-less `InstantiationException` at its
    /// first construction, in the accessor's place. Interpreter round i2 wave
    /// 55, lane L3.
    AbstractClass,
    /// The declaring class's initialization, which the native is about to
    /// run (it is not initialized yet): JDK 25 initializes it in the accessor
    /// factory, OUTSIDE the method handle, so its `ExceptionInInitializerError`
    /// / `NoClassDefFoundError` is raised unwrapped under
    /// `Unsafe.ensureClassInitialized0`, below `MethodHandleAccessorFactory`,
    /// `ReflectionFactory` and the member's `acquire*Accessor`. Interpreter
    /// round i2 wave 58, lane L3.
    ClassInit,
}

/// Where HotSpot's reflection raises the `ClassCastException` of an argument
/// of the wrong type (JDK 25, measured by
/// `L3W47ConstructorArgCause.java`), for
/// [`ReflectiveCheck::ArgumentCast`]. Interpreter round i1 wave 47, lane L3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflectiveArgumentCast {
    /// A reference parameter: the method handle's `Class.cast`
    /// (`Cannot cast A to B`), right above the accessor.
    Reference,
    /// A primitive parameter of descriptor `char` and an argument that is no
    /// `Number`, `Boolean` or `Character`: the `(Number) x` checkcast of
    /// `ValueConversions.primitiveConversion`, under `unbox<Wrapper>`
    /// (HotSpot's `class A cannot be cast to class java.lang.Number (...)`).
    NotANumber(char),
    /// A primitive parameter of descriptor `char` and an argument whose class
    /// does not widen to it: `primitiveConversion`'s
    /// `wrap.wrapperType().cast(x)` (`Cannot cast A to <Wrapper>`).
    Unconvertible(char),
}

/// Where one hop of a [`ServedRaiseChain`] leaves its body. Interpreter round
/// i2 wave 54, lane L3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServedRaiseHopSite {
    /// Its FIRST call of such a method ([`Self::LastCall`]'s match): the
    /// earlier of two branches (`Field.get`'s access-checked call of its
    /// accessor, before the override branch's).
    FirstCall {
        name: &'static str,
        descriptor: Option<&'static str>,
    },
    /// Its LAST call (any `invoke*`) of a method named `name` whose
    /// descriptor is `descriptor` (any descriptor when `None`), whatever the
    /// owner: the call into the next hop, or into the hidden frames
    /// (`MethodHandle.invokeExact`) between it and the next hop.
    LastCall {
        name: &'static str,
        descriptor: Option<&'static str>,
    },
    /// Its ONE raise site of the chain's throwable: its one construction of
    /// the class, or, for an implicit `NullPointerException` (the error's
    /// message names the action), its one `invoke*` whose JEP 358 action
    /// renders so; a JDK `native` method is listed as its own
    /// `(Native Method)` frame. Only the innermost hop.
    Raise,
    /// Not listed: the first hop only, a signature-polymorphic invoker of
    /// the hop's class (`MethodHandle.invokeExact` / `invoke`, any
    /// descriptor) that the VM served where HotSpot runs the handle's hidden
    /// `LambdaForm` frames; the next hop is looked up in the class it names.
    /// A field handle's null base: `checkBase` -> `requireNonNull` below the
    /// accessor's `invokeExact` (`--real-jdk`). Interpreter round i2 wave 56,
    /// lane L3.
    Hidden,
    /// The innermost hop only, for a `ClassCastException`: its LAST
    /// `checkcast class_name` whose operand is a local just loaded (`aload`),
    /// the raise a construction-site search cannot name --
    /// `ValueConversions.primitiveConversion`'s unguarded `res = (Number) x`
    /// (its earlier guarded cast of the same local never fails; a cast of a
    /// call's result has no local operand). JIT round j27 wave 4, lane
    /// MHROWS2.
    LocalCheckcast { class_name: &'static str },
}

/// One hop of a [`ServedRaiseChain`]: a JDK method, read from the real class
/// bytes at the capture (a class the capture cannot find voids the chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedRaiseHop {
    pub class: &'static str,
    pub name: &'static str,
    pub descriptor: &'static str,
    pub site: ServedRaiseHopSite,
}

/// The JDK frames HotSpot lists for a raise a native serves two or more hops
/// above the throw (`Field.get` with the override: the field accessor's
/// `get` -> `getInt` -> `DirectMethodHandle.checkBase` ->
/// `Objects.requireNonNull`), OUTERMOST first; the first hop is the served
/// method itself. [`NativeExceptionAccess::note_served_raise_chain`].
/// Interpreter round i2 wave 54, lane L3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedRaiseChain {
    /// Internal name of the throwable's class the chain is for.
    pub throwable_class: &'static str,
    pub hops: Vec<ServedRaiseHop>,
}

/// What a settling handle's static door remembers of its served-raise note
/// ([`NativeInvokeAccess::invoke_static_settling_keyed`]): the method the door
/// selected, as `(class, method slot)` (`None`: a selection the keyed
/// families do not answer), for the owner it resolved from, valid while the
/// VM's class-redefinition count is `redefinitions`. Interpreter round i2
/// wave 65, lane L3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledDoorKeyEntry {
    /// The owner the door's selection started from (the handle's settled or
    /// resolved declaring class).
    pub owner: ClassId,
    /// The class-redefinition count the selection was made at.
    pub redefinitions: u64,
    /// The selected method.
    pub key: Option<(ClassId, u32)>,
}

/// One handle record's slot for its door's [`SettledDoorKeyEntry`]: a
/// `Cell`, kept beside the record's settled owner (the records live in
/// per-thread memos, so the slot is never shared between threads). Empty
/// until the VM fills it; a VM with the switch off never does. Interpreter
/// round i2 wave 65, lane L3.
#[derive(Debug, Default)]
pub struct SettledDoorKey {
    pub(in crate::registry) entry: std::cell::Cell<Option<SettledDoorKeyEntry>>,
}

impl SettledDoorKey {
    /// An empty slot.
    pub const fn new() -> Self {
        Self {
            entry: std::cell::Cell::new(None),
        }
    }

    /// The remembered selection, if any.
    #[inline]
    pub fn get(&self) -> Option<SettledDoorKeyEntry> {
        self.entry.get()
    }

    /// Remember `entry` (or forget, with `None`).
    #[inline]
    pub fn set(&self, entry: Option<SettledDoorKeyEntry>) {
        self.entry.set(entry);
    }

    /// The remembered key, when it was selected from `owner` at the
    /// class-redefinition count `redefinitions`: `Some(key)` (itself an
    /// `Option`), else `None` -- a different owner, a moved count, or
    /// nothing remembered.
    #[inline]
    pub fn lookup(&self, owner: ClassId, redefinitions: u64) -> Option<Option<(ClassId, u32)>> {
        self.entry
            .get()
            .filter(|entry| entry.owner == owner && entry.redefinitions == redefinitions)
            .map(|entry| entry.key)
    }
}

/// [`StackTraceEntry::line_number`] of an entry whose line a lean
/// `StackWalker` capture deferred
/// (`NativeExceptionAccess::capture_stack_walk_trace_deferred`; JIT round 19
/// wave 2, lane throws2, WALK17-3 step 1): the line of its method at
/// `byte_code_index`, resolved by
/// `NativeExceptionAccess::resolve_deferred_stack_walk_line` before any
/// reader sees it. Never a real line (lines are `u16`), never `-1` / `-2`.
pub const LINE_NUMBER_DEFERRED: i32 = -0x4c44_0001;

/// [`LINE_NUMBER_DEFERRED`] for an entry whose eager line is read at bci 0
/// (an OSR-overridden interpreter frame whose override bci is negative: the
/// eager re-line reads `bci.max(0)`).
pub const LINE_NUMBER_DEFERRED_AT_BCI_ZERO: i32 = -0x4c44_0002;

#[derive(Debug, Clone)]
pub struct StackTraceEntry {
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    /// Exact descriptor of the captured method when it was available without
    /// a class-store lookup. Together with `method_name` this identifies an
    /// overload exactly for deferred line resolution.
    pub method_descriptor: Option<Arc<str>>,
    pub source_file: Option<Arc<str>>,
    pub line_number: i32, // -1 for unknown, -2 for native methods
    /// Bytecode index of the last-executed instruction in the frame's method.
    /// `-1` for unknown / native. Used by `StackFrame.getByteCodeIndex()`.
    pub byte_code_index: i32,
    /// The frame's own `ClassId`, when captured directly from a live
    /// interpreter frame (`Frame::class_id`) rather than synthesized.
    /// `StackFrame.getDeclaringClass()`/`declaringClass()` implementations
    /// MUST prefer this over re-resolving `class_name` through a global
    /// name-keyed lookup (`class_id_by_name`/`find_class_by_name`): a class
    /// executing its OWN `<clinit>` is guaranteed loaded (this ClassId is
    /// live proof of that) but is not reliably found by a fresh by-name
    /// lookup made from deep inside that same `<clinit>` -- observed via
    /// `SpringFactoriesLoader`/`EntityManagerFactoryUtils` invoking
    /// `LogFactory.getLog()` from their own static initializers, which
    /// walks the stack (log4j-api's `StackLocator`) back to that exact
    /// self-frame and NPEs when `getDeclaringClass()` falls back to null.
    /// `None` only for synthetic entries with no backing interpreter frame.
    pub class_id: Option<ClassId>,
    /// Index of this frame's method within its declaring class's
    /// `Class::methods` list, when the capture path had a `ClassStore` borrow
    /// and resolved it. `None` for synthetic entries and for the deliberately
    /// lock-free cross-thread snapshot (`stackwalker::capture_frames_no_lines`,
    /// which takes no `ClassStore` by design).
    ///
    /// ARCH-2026-07-26 (`cross-owner-closeout`, request CR-SW-1 of
    /// `stackwalk-and-vtable.md`). This exists so
    /// that *deferred* line-number resolution can be **exact**. `class_name` +
    /// `method_name` + `byte_code_index` are not enough: a class may declare an
    /// overload set under one name, the members have different
    /// `LineNumberTable`s, and picking the wrong one prints a line from the
    /// wrong method body. The descriptor is preferred when present; this index
    /// preserves an exact identity for legacy entries that predate it.
    ///
    /// It is an *index*, never a borrow, and it is **never trusted on its own**:
    /// `stackwalker::resolve_line_numbers_in_place` re-reads
    /// `class.methods[idx]` from the live `ClassStore` and re-checks that its
    /// name equals `method_name` before using it, so a class redefinition that
    /// reorders or removes methods fails closed to "unknown line" rather than
    /// resolving against the wrong body. `ClassId`s are monotonic and never
    /// reused (`ClassStore::remove` leaves a tombstone), so a stale entry can
    /// only ever miss, never alias a different class.
    pub method_index: Option<u32>,
}

/// Callback signature for native method implementations.
///
/// # Arguments
/// - `ctx` — mutable reference to the VM context (implements `NativeContext`)
/// - `args` — method arguments: for instance methods, `args[0]` is the receiver (`this`)
///
/// # Returns
/// - `Ok(Some(value))` — method returned a value
/// - `Ok(None)` — method returned void
/// - `Err(MethodCallFailed)` — method threw an exception or had an internal error
pub type NativeCallback = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;

/// Event emitted by the native `ByteArrayOutputStream` implementation before
/// it falls back to growing its in-heap byte buffer.
///
/// Most streams are ordinary buffers, so observers return `Ok(false)` and the
/// I/O crate performs its normal operation.  A small number of bridge APIs
/// expose a ByteArrayOutputStream-shaped Java object while the bytes must go to
/// a native sink immediately (legacy fixed-length HttpURLConnection is one).
/// Keeping that opt-in at the API boundary avoids making native-io depend on a
/// higher-level protocol crate.
pub enum BaosEvent {
    WriteByte(u8),
    /// A Java byte-array slice.  The observer must read it through `ctx` only
    /// after deciding it owns this stream, keeping the ordinary BAOS hot path
    /// allocation-free.
    WriteArray {
        array: ObjectRef,
        offset: usize,
        len: usize,
    },
    Flush,
    Close,
}

/// Return `Ok(true)` when the event was consumed and the ordinary BAOS path
/// must be skipped; `Ok(false)` leaves the receiver's normal buffering intact.
pub type BaosEventHook =
    fn(&mut dyn NativeContext, ObjectRef, BaosEvent) -> Result<bool, MethodCallFailed>;

pub(in crate::registry) static BAOS_EVENT_HOOK: OnceLock<BaosEventHook> = OnceLock::new();

/// Install the process-wide optional BAOS bridge hook.  Registration happens
/// during native bootstrap; repeated registrations are harmless because the
/// first (and only) bridge implementation wins.
pub fn install_baos_event_hook(hook: BaosEventHook) {
    let _ = BAOS_EVENT_HOOK.set(hook);
}

/// Offer a BAOS event to the optional bridge hook.
pub fn dispatch_baos_event(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    event: BaosEvent,
) -> Result<bool, MethodCallFailed> {
    match BAOS_EVENT_HOOK.get() {
        Some(hook) => hook(ctx, stream, event),
        None => Ok(false),
    }
}

/// Event emitted by the native `ByteArrayInputStream` implementation when a
/// reader reaches the end of the buffer, or closes it.
///
/// The mirror image of [`BaosEvent`], and it exists for the mirror-image
/// reason. A bridge API can hand Java a `ByteArrayInputStream`-shaped object
/// whose *lifetime* matters to native state the bridge holds elsewhere — the
/// `https:` response body is the motivating case: HotSpot returns the
/// connection to its `KeepAliveCache` the moment the body is drained, after
/// which every CONNECTION-level accessor throws `IllegalStateException:
/// connection not yet open` again, while the `SSLSession` object the
/// application already holds stays valid. CratonVM reads that body to
/// completion inside `perform` and hands it over whole, so the drain is the
/// only observable "the application is done with this exchange" instant, and
/// it is observable *here* and nowhere else.
///
/// Keeping the observation at the API boundary is what avoids making
/// `native-io` depend on a higher-level protocol crate, exactly as for
/// [`BaosEvent`].
///
/// # This adds no registration
///
/// `ByteArrayInputStream.read()I`, `read([BII)I` and `close()V` are **already**
/// registered natives owned by `native-io` (MEASURED — `--dump-native-registry`
/// under `--real-jdk`, `9ae371468`: `owns_slot=true` on all three, at
/// `native-io/src/lib.rs`). A hook inside an existing body registers nothing,
/// so `regression-suite/bridge-ratchet.sh` and the baselines under `scripts/`
/// do not move. Every design that instead added a `Bridge` over concrete
/// bytecode — a dedicated response-stream class, a `SequenceInputStream`
/// sentinel, a second `close()V` registration — would have needed a baseline
/// refresh; see
/// `docs/feature-designs/native-dispatch-architecture.md`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BaisEvent {
    /// A read returned `-1`: `pos >= count`, the buffer is exhausted.
    ///
    /// **Fires on EVERY exhausted read, not only on the transition**, because
    /// the transition is not observable: a stream constructed empty is at
    /// `pos >= count` from its first read, and the two states are
    /// indistinguishable from inside the read body. Observers must therefore be
    /// idempotent — which they must be anyway, since a drained stream that is
    /// then closed produces `Eof` *and* [`BaisEvent::Close`].
    Eof,
    /// `close()` was called on the stream.
    Close,
}

/// Observer for [`BaisEvent`].
///
/// # Why this returns `()` and [`BaosEventHook`] returns `bool`
///
/// **Deliberate, and not an oversight in the mirroring.** A `BaosEvent`
/// observer legitimately *takes over* the write — the bytes go to a native sink
/// instead of the in-heap buffer — so `Ok(true)` meaning "consumed, skip the
/// ordinary path" is a real choice with a real second branch behind it.
///
/// There is no such choice on the input side. `ByteArrayInputStream.read()`
/// **must** return `-1` at `pos >= count` and `close()` **must** be a no-op, on
/// every path, whatever any observer thinks; those are JDK contracts a lifetime
/// observer has no business editing. A `bool` here would be a return value that
/// every caller is required to ignore — the kind of parameter that eventually
/// gets honoured by someone who reads the type and not the doc, silently
/// turning an EOF into a non-EOF. So the type says what is true: observe, do
/// not decide.
///
/// `Err` is still available and still propagates, for a genuine internal
/// failure the VM must not swallow. Observers should treat it as such and
/// **must not** raise a Java exception from here to signal an ordinary
/// condition: it would surface out of a `ByteArrayInputStream.read()` that
/// HotSpot completes normally, which is a divergence bought in exchange for
/// nothing.
pub type BaisEventHook =
    fn(&mut dyn NativeContext, ObjectRef, BaisEvent) -> Result<(), MethodCallFailed>;

pub(in crate::registry) static BAIS_EVENT_HOOK: OnceLock<BaisEventHook> = OnceLock::new();

/// Install the process-wide optional BAIS lifetime observer. Registration
/// happens during native bootstrap; repeated registrations are harmless because
/// the first (and only) bridge implementation wins — same contract as
/// [`install_baos_event_hook`].
pub fn install_bais_event_hook(hook: BaisEventHook) {
    let _ = BAIS_EVENT_HOOK.set(hook);
}

/// Offer a BAIS lifetime event to the optional observer.
///
/// With no hook installed this is one relaxed `OnceLock` load and a return,
/// which is strictly cheaper than what [`dispatch_baos_event`] already pays on
/// every single byte written through `ByteArrayOutputStream.write(I)V`.
pub fn dispatch_bais_event(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    event: BaisEvent,
) -> Result<(), MethodCallFailed> {
    match BAIS_EVENT_HOOK.get() {
        Some(hook) => hook(ctx, stream, event),
        None => Ok(()),
    }
}

/// Classification of a registered native method.
///
/// The native overlay is three different things wearing one uniform; this tag
/// records which is which so tooling (census dump, differential harness) and
/// the dispatcher can treat them differently:
///
/// - [`NativeKind::Intrinsic`] — a correct fast-path for a hot method (e.g.
///   `Math.abs`, `String.length`). Returns the same answer the real bytecode
///   would, just faster. Always kept; never gated.
/// - [`NativeKind::Bridge`] — a native the VM genuinely needs because it cannot
///   run the real thing: OS syscalls, `sun.*` internals depending on VM state,
///   classes with no real bytecode. It *is* the real behavior. Never gated.
/// - [`NativeKind::SyntheticStub`] — a fake: placeholder/approximate/wrong
///   return values, fabricated objects, or "fake main" launcher short-circuits.
///   These shadow correct real bytecode and are the removal target. Gateable.
///
/// Every `register()` call specifies its [`NativeKind`] explicitly — there is
/// no ambient or default category.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum NativeKind {
    Intrinsic,
    Bridge,
    SyntheticStub,
}

/// Per-kind dispatch total, carrying the one fact that says whether it may be
/// believed. Produced by
/// [`NativeMethodRegistry::invocations_of_kind_checked`].
///
/// The pair exists because the total alone is unfalsifiable in the direction
/// gates actually assert. `synthetic_stub_invocations == 0` is the L4 gate
/// (`docs/book/src/user-guide/real-jdk-mode.md` §4); it is read from a sum of
/// [`NativeMethodRegistry::record_invocation`] counters, and any slot wired to a
/// bypassing dispatch path contributes nothing to that sum however many times
/// it runs. Four such slots are `SyntheticStub` today — the Panama
/// `DowncallHandle` arms in `UNCOUNTED_STACKLESS_NATIVES`; see
/// [`NativeMethodRegistry::invocations_of_kind`] for the measurement. Reading
/// `total` without `incomplete_slots` cannot distinguish those two zeroes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct KindInvocations {
    /// The kind this was measured for.
    pub kind: NativeKind,
    /// Counted dispatches summed over every slot of this kind — a **floor** on
    /// Java-level calls whenever `incomplete_slots > 0`.
    pub total: u64,
    /// Slots of this kind that have declared their count a floor
    /// ([`NativeMethodRegistry::mark_invocations_incomplete`]).
    pub incomplete_slots: usize,
}

impl KindInvocations {
    /// `true` when `total == 0` **and** every slot of this kind is counted, so
    /// the zero actually means "none ran".
    ///
    /// A zero `total` with `incomplete_slots > 0` is not a weaker result than
    /// this — it is *no result*, and it is the state in which an `== 0`
    /// assertion passes while proving nothing.
    pub fn is_conclusive_zero(self) -> bool {
        self.total == 0 && self.incomplete_slots == 0
    }

    /// Whether the instrument can currently see every slot of this kind, i.e.
    /// whether `total` is a total rather than a floor.
    ///
    /// Distinguished from [`Self::is_conclusive_zero`] so a caller that wants
    /// to *report* a non-zero total can still say whether it is the whole
    /// number.
    pub fn is_measurable(self) -> bool {
        self.incomplete_slots == 0
    }

    /// The gate predicate: no dispatches of this kind were counted **and** none
    /// could have escaped counting.
    ///
    /// Identical to [`Self::is_conclusive_zero`] and named for the call site,
    /// because the two readings are worth keeping apart in a gate's source:
    /// `is_conclusive_zero` is a statement about the measurement,
    /// `is_clean` is the verdict drawn from it. A gate must not pass on
    /// `total == 0` alone.
    pub fn is_clean(self) -> bool {
        self.is_conclusive_zero()
    }

    /// One line a failing gate can print that names *which* of the two failure
    /// modes it hit — a gate that says only "assertion failed" sends the reader
    /// to look for a stub that may not exist.
    pub fn describe(self) -> String {
        match (self.total, self.incomplete_slots) {
            (0, 0) => format!("{}: 0 dispatches, all slots counted", self.kind.as_str()),
            (0, n) => format!(
                "{}: 0 counted dispatches, but {n} slot(s) of this kind are declared \
                 incomplete — this zero proves nothing (see NativeMethodRegistry::\
                 invocations_of_kind)",
                self.kind.as_str()
            ),
            (t, 0) => format!("{}: {t} dispatches", self.kind.as_str()),
            (t, n) => format!(
                "{}: at least {t} dispatches ({n} slot(s) declared incomplete, so this \
                 is a floor)",
                self.kind.as_str()
            ),
        }
    }
}

impl NativeKind {
    /// Stable lowercase name for JSON census output.
    pub fn as_str(self) -> &'static str {
        match self {
            NativeKind::Intrinsic => "intrinsic",
            NativeKind::Bridge => "bridge",
            NativeKind::SyntheticStub => "synthetic-stub",
        }
    }

    /// Whether this kind may be registered/invoked under `mode`
    /// (`docs/book/src/user-guide/real-jdk-mode.md` §1.3, §4, 2026-07-31).
    ///
    /// `SyntheticStub` is the ONLY kind `JdkOnly` rejects. `Bridge` and
    /// `Intrinsic` are both explicitly allowed by §1 ("native code may cross VM
    /// boundaries or provide proven intrinsics"), so this is deliberately not
    /// "everything except a reviewed allowlist" — the review is a human process
    /// recorded in the census, not a predicate here.
    ///
    /// Kept `#[inline]` and total: dispatch calls it per native invocation, and
    /// under `Compatible` it must collapse to a constant `true`.
    #[inline]
    pub fn allowed_in(self, mode: CompatibilityMode) -> bool {
        match mode {
            CompatibilityMode::Compatible => true,
            CompatibilityMode::JdkOnly => !matches!(self, NativeKind::SyntheticStub),
        }
    }
}

/// One row of the SBR5 lean native door's census
/// (`CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS`, JIT round 19 wave 2, lane
/// funnel2). The `Cycles*` rows are sums of rdtsc spans over served
/// crossings: `front` = door entry to the funnel call, `call` = the funnel
/// with the native, `tail` = the funnel's return to the door's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NativeSiteDoorEvent {
    /// The door was asked (every compiled call through either dispatch helper).
    Probe = 0,
    /// The slot was empty.
    MissEmpty,
    /// The slot held another site (direct-mapped eviction, or this site was
    /// never filed).
    MissOtherSite,
    /// A per-call guard moved (VM, JIT cache generation, registry generation,
    /// a class was redefined).
    DeclineGuard,
    /// The argument shape did not match the record.
    DeclineShape,
    /// Null, array, invalid or other-class receiver.
    DeclineReceiver,
    /// The capability gate refused.
    DeclineCapability,
    /// Served through the leaf wrapper.
    HitLeaf,
    /// Served through `safe_native_call_prevalidated_objects`.
    HitFunnel,
    /// Served through `safe_native_call_jit_lean`.
    HitLeanFunnel,
    /// Cycle sum: door entry to the call.
    CyclesFront,
    /// Cycle sum: the call (wrapper and native).
    CyclesCall,
    /// Cycle sum: the call's return to the door's.
    CyclesTail,
    /// JIT round 19 wave 3, lane funnel3 (FUNNEL2-19-2): served by a filed
    /// leaf kind's no-wrapper probe (`ArrayList.get`/`size`, `CHM.get`,
    /// `Integer.hashCode`/`equals`, `Object.getClass`, `Reference.refersTo`).
    /// Not timed.
    HitLeafKind,
    /// FUNNEL2-19-2: a filed leaf kind's probe declined and the record's
    /// callback served the call (that crossing is counted again under
    /// `HitLeaf` / `HitFunnel` / `HitLeanFunnel`).
    LeafKindFallback,
    /// FUNNEL2-19-1: the record was found in the second way of its set.
    HitSecondWay,
    /// FUNNEL2-19-3: an object result skipped the funnel's entry walk (a
    /// fresh-result native under `CRATONVM_JIT_NATIVE_FRESH_RESULT`).
    FreshResult,
}

impl NativeSiteDoorEvent {
    /// Number of rows (the census array's length).
    pub const COUNT: usize = 17;
}

/// One row of the schema-version-2 native census
/// (`docs/book/src/user-guide/real-jdk-mode.md` §4).
///
/// Schema v1 was `dump_registrations()`'s `(class, method, descriptor, kind)`
/// tuple, which could not answer either of the two questions the Real-JDK gate
/// actually asks: *who* registered a stub (so it can be deleted at the source)
/// and *whether anything called it* (so a stub nobody dispatches is not treated
/// as a live blocker). Both were previously unrecoverable after boot — the
/// registration log kept only the triple, and re-registration of a triple
/// last-write-wins into the slot with no history at all.
///
/// One row per *registration*, not per slot: a triple registered twice appears
/// twice, in registration order, and only the surviving row carries the
/// invocation count (see [`NativeMethodRegistry::census`]).
#[derive(Debug, Clone)]
pub struct NativeCensusEntry {
    pub class: String,
    pub name: String,
    pub descriptor: String,
    pub kind: NativeKind,
    /// Registration site, when recorded (`"native-builtins/src/lib.rs:1234"`).
    /// `None` only for rows registered before provenance tracking existed —
    /// currently unreachable, but the contract keeps the `Option`.
    pub registered_by: Option<String>,
    /// Kind of the entry this registration overwrote, if any. `Some` means this
    /// row superseded an earlier registration of the identical triple.
    pub overwrote: Option<NativeKind>,
    /// Times this slot was dispatched **through a path that resolved it by
    /// name or id** this run. `0` on a superseded row: the count belongs to
    /// whoever currently owns the slot.
    ///
    /// # This is a lower bound, and reading it as a call count has cost time
    ///
    /// Not "times the method ran". A dispatch served from a pre-resolved
    /// function pointer — the interpreter's intrinsic table, the JIT's thin
    /// direct-call helpers — never touches the registry and is not here.
    /// MEASURED 2026-08-17: 100,000 `Math.abs` calls report `1`; the same run
    /// under `CRATONVM_DISABLE_INTRINSICS=1` reports `100,000`. 100,000
    /// `HashMap.get` calls report `1,873` with the JIT on and `100,001` under
    /// `--nojit`. Full method, controls and the causal test:
    /// `docs/feature-designs/native-dispatch-architecture.md`.
    ///
    /// Consequences already paid for in this directory: a lane concluded a
    /// body was dead from `invocations = 0` when its workload simply never
    /// reached the counted path, and another was told to read the field as a
    /// boolean. **Check [`Self::invocations_complete`] first.** For a census
    /// that is exact for everything measured, run with `--nojit` and
    /// `CRATONVM_DISABLE_INTRINSICS=1`.
    pub invocations: u64,
    /// Whether [`Self::invocations`] is a total (`true`) or a floor (`false`)
    /// for this slot — see
    /// [`NativeMethodRegistry::mark_invocations_incomplete`].
    ///
    /// `true` is the default and means "no dispatch path has declared itself a
    /// bypass for this slot". Until the sites listed on that method actually
    /// call it, `true` is the answer for every row, and the honest reading of
    /// the whole column is still the one on [`Self::invocations`].
    pub invocations_complete: bool,
    /// Whether this registration still **owns its slot**, i.e. whether a
    /// dispatch of this triple would reach *this* row's callback.
    ///
    /// `false` means a later `register*` of the identical triple replaced it
    /// (last-write-wins into the slot), so the row records that a registration
    /// happened and nothing more: it can never be dispatched, and adjudicating
    /// its kind decides nothing.
    ///
    /// # Why this is a column and not left to the reader
    ///
    /// Every consumer of this census was inferring it — or not inferring it.
    /// Measured on `JdkOnlyCensusLoadProbe` (JDK 25, 2026-08-06): of 11,876
    /// rows, **1,237 own no slot**, and **1,092 of the 9,660 rows the
    /// adjudication reports as unadjudicated `Bridge` are among them**. So the
    /// live unadjudicated surface is 8,568, and a reclassification wave sized
    /// off the larger number would go looking for ~1,100 registrations that
    /// cannot affect anything.
    ///
    /// It is derivable from census order (within a triple, the last row owns the
    /// slot) — which is exactly why it belongs here instead: that is an
    /// undocumented ordering guarantee for external scripts to depend on, and
    /// [`NativeMethodRegistry::census`] already has the answer in hand.
    ///
    /// This is the fourth distinct way this census has been misread; the other
    /// three are in
    /// `jdk-only-census-one-class-one-platform-FIXED-20260810.md`.
    pub owns_slot: bool,
    /// Whether [`Self::kind`] was **stated at this registration site**
    /// (`register_with_kind`) or inherited from an ambient `set_category` in
    /// some enclosing registrar (`register`).
    ///
    /// This is the column the 157-entry reclassification was blocked on.
    /// `registered_by` says *where* a registration was written; only this says
    /// whether anybody decided what it is. A `false` here on a `SyntheticStub`
    /// row means nothing more than "no `set_category` covered this call site",
    /// since `SyntheticStub` is the default — which is very different from a
    /// deliberate stub, and the two were previously indistinguishable.
    ///
    /// Read it with the direction of the mistake in mind: `false` on a
    /// `Bridge` row is the *dangerous* one, because `Bridge` is never the
    /// default and can only have been inherited from a `set_category` line that
    /// covered more registrations than its author was thinking about.
    pub kind_stated: bool,
    /// Whether ANYONE chose [`Self::kind`] — at the registration site
    /// (`register_with_kind`) **or** in an enclosing `set_category` /
    /// `with_category` scope — as opposed to the registration simply landing on
    /// the constructor's `SyntheticStub` default.
    ///
    /// Deliberately wider than [`Self::kind_stated`], and it answers a
    /// different question. `kind_stated` asks *"was this adjudicated here?"*;
    /// this asks *"is this kind an opinion at all?"*. `with_category`'s own doc
    /// calls itself "how a whole `register_*` function tags all of its
    /// registrations" — that is an opinion expressed once for a group, and a
    /// row covered by one is not on the default.
    ///
    /// This is the column step 3 of
    /// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up
    /// is scored against: *"flip the default last — once every registration
    /// states its kind, `current_category` can default to something that fails
    /// loudly (or be deleted)"*. `kind_stated` cannot score it, because it is
    /// `false` on the thousands of rows a `with_category` scope covers
    /// deliberately. `kind_chosen: false` is the honest count of registrations
    /// nobody has an opinion about, and the default cannot be made to fail
    /// until it is zero.
    ///
    /// `false` does **not** mean the kind is `SyntheticStub`: `register`'s
    /// downgrade rule preserves a prior *chosen* kind over an unchosen
    /// re-registration of the same triple, so an unchosen row can carry any
    /// kind the earlier registration chose.
    pub kind_chosen: bool,
}

// ===========================================================================
// DUPLICATE-REGISTRATION ANALYSIS  (the shadowed-native gate)
// ===========================================================================

/// One registration that a LATER `register*` of the identical triple displaced.
///
/// # The defect species this exists to make findable
///
/// [`NativeMethodRegistry::register`] is last-write-wins and **updates the
/// existing slot in place**. So a correct, guarded, carefully-written native
/// silently loses to an unguarded twin registered later, and every symptom then
/// points at the wrong source file. Four instances cost a full
/// measure-fix-rebuild cycle each before anyone looked for the pattern:
///
/// 1. `MethodHandles$Lookup.defineHiddenClass` — a placeholder registered late
///    in `lang_invoke.rs` shadowed the real implementation, and returned a
///    `Lookup` whose slot 0 was never written, so `lookupClass()` answered null.
/// 2. `Files.copy(Path,Path,CopyOption...)` — the winner never read its options
///    argument, so a copy onto an existing file overwrote instead of throwing
///    `FileAlreadyExistsException`.
/// 3. `Module.getResourceAsStream` — registered twice from inside the SAME
///    function, ~9k lines apart; the wave-2 `opens` gate was dead code.
/// 4. `SSLContext.getInstance` — four competing registrations; the live one
///    threw an `IOException` whose message said `NoSuchAlgorithmException`.
///
/// The misdirection that made these expensive is worth stating once: **forcing
/// "the native" over real bytecode does not help when the slot holds a
/// DIFFERENT native.** Only provenance distinguishes the two, and only the
/// census keeps it.
///
/// # Why this is mechanical
///
/// Nothing here is inferred. `register` is `#[track_caller]`, so every accepted
/// registration already records its `Location`, and `registrations` is
/// append-only — the LOSER's row survives, tagged
/// [`NativeCensusEntry::owns_slot`]`== false`. The pair is therefore recoverable
/// exactly, with no heuristics and no source scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedRegistration {
    /// Class of the shadowed triple.
    pub class: String,
    /// Method name of the shadowed triple.
    pub name: String,
    /// Descriptor of the shadowed triple.
    pub descriptor: String,
    /// `file:line` of the registration that LOST, i.e. the callback that can
    /// never be dispatched. `None` only if provenance was not recorded.
    pub shadowed_at: Option<String>,
    /// [`NativeKind`] the losing registration carried.
    pub shadowed_kind: NativeKind,
    /// `file:line` of the registration that currently OWNS the slot — the
    /// callback a dispatch of this triple actually reaches.
    pub winner_at: Option<String>,
    /// [`NativeKind`] of the winning registration.
    pub winner_kind: NativeKind,
}

/// Strip the `:line` suffix off a `"file:line"` provenance string and normalise
/// `\` to `/`.
///
/// Both halves matter for a frozen baseline. `Location::file()` is the path as
/// the compiler saw it, which is backslash-separated on Windows and
/// forward-slash on Linux — a baseline keyed on the raw string is red on one of
/// the two hosts. And the LINE must go: a duplicate keyed on a line number goes
/// stale the moment anyone inserts a comment above the call, which is how fixed
/// line bands in this repo's source-witness tests have failed before.
pub(in crate::registry) fn provenance_file(site: Option<&str>) -> Option<String> {
    let site = site?;
    let file = match site.rfind(':') {
        Some(i) => &site[..i],
        None => site,
    };
    Some(file.replace('\\', "/"))
}

impl ShadowedRegistration {
    /// `class.name descriptor`, the way the rest of this crate spells a triple.
    pub fn triple(&self) -> String {
        format!("{}.{}{}", self.class, self.name, self.descriptor)
    }

    /// Source file of the losing registration, without its line number and with
    /// separators normalised. See [`provenance_file`].
    pub fn shadowed_file(&self) -> Option<String> {
        provenance_file(self.shadowed_at.as_deref())
    }

    /// Source file of the winning registration. See [`provenance_file`].
    pub fn winner_file(&self) -> Option<String> {
        provenance_file(self.winner_at.as_deref())
    }

    /// Whether the two registrations live in DIFFERENT source files.
    ///
    /// This is the shape of instances 1, 2 and 4 above, and the one a reviewer
    /// has no chance of catching by eye: the two files are never open at the
    /// same time. Instance 3 was same-file (and same *function*), which is why
    /// this is a classifier and not the gate's only question.
    pub fn cross_file(&self) -> bool {
        match (self.shadowed_file(), self.winner_file()) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        }
    }

    /// Whether the winner and the loser disagree about what this native *is*.
    ///
    /// The highest-signal subset. A `SyntheticStub` displacing a `Bridge` is
    /// instance 1 exactly: a placeholder overwriting a real implementation. It
    /// also changes behaviour beyond the callback, because `NativeKind` decides
    /// three separate things — `CompatibilityMode::JdkOnly` refuses a
    /// `SyntheticStub`, `CRATONVM_NO_STUBS` drops one, and
    /// `synthetic_stub_kind_should_yield_to_real_bytecode` arbitrates for one.
    pub fn kind_disagreement(&self) -> bool {
        self.shadowed_kind != self.winner_kind
    }

    /// Stable one-line key for a frozen baseline: the triple plus the two
    /// FILES, tab-separated, with no line numbers.
    ///
    /// Deliberately coarser than the full provenance. Moving a registration
    /// within its file must not turn the gate red, but moving it to a different
    /// file — or adding a third registrar — must, because that is a new pair a
    /// human has not looked at. Two different triples shadowed by the same file
    /// pair stay distinct rows, since the triple leads the key.
    pub fn key(&self) -> String {
        format!(
            "{}\t{} => {}",
            self.triple(),
            self.shadowed_file().unwrap_or_else(|| "<unknown>".into()),
            self.winner_file().unwrap_or_else(|| "<unknown>".into()),
        )
    }
}

/// Every row in `rows` that a later registration of the identical triple
/// displaced, paired with the registration that won.
///
/// Free function over a census rather than a method on the registry, because
/// the gate that consumes it lives in `native-builtins/tests` (it needs the
/// registrar crates, which `native-api` cannot depend on) while the logic must
/// be unit-testable here, where no boot path is available.
///
/// One row per LOSER, so a triple registered four times yields three rows, all
/// naming the same winner. That is the right shape for a ratchet: fixing one of
/// the four removes exactly one row.
///
/// The winner is the row with [`NativeCensusEntry::owns_slot`]`== true`, of
/// which a triple has exactly one — `register` either pushes a slot or rewrites
/// the existing slot's `reg_index` to point at the newest registration, so
/// `census`'s reverse map can only attribute the slot to the last accepted
/// registration. The `unwrap_or(last)` fallback below is unreachable and exists
/// so a hypothetical desync under-reports rather than panicking in CI.
pub fn shadowed_registrations_in(rows: &[NativeCensusEntry]) -> Vec<ShadowedRegistration> {
    // Group by triple, preserving first-seen order so the output is
    // deterministic across runs — the property any frozen baseline rests on.
    let mut order: Vec<(&str, &str, &str)> = Vec::new();
    let mut groups: FxHashMap<(&str, &str, &str), Vec<usize>> = FxHashMap::default();
    for (i, row) in rows.iter().enumerate() {
        let key = (
            row.class.as_str(),
            row.name.as_str(),
            row.descriptor.as_str(),
        );
        let bucket = groups.entry(key).or_insert_with(|| {
            order.push(key);
            Vec::new()
        });
        bucket.push(i);
    }

    let mut out = Vec::new();
    for key in order {
        let idxs = match groups.get(&key) {
            Some(v) if v.len() > 1 => v,
            _ => continue,
        };
        let winner_idx = idxs
            .iter()
            .copied()
            .find(|i| rows[*i].owns_slot)
            .unwrap_or_else(|| idxs[idxs.len() - 1]);
        let winner = &rows[winner_idx];
        for &i in idxs {
            if i == winner_idx {
                continue;
            }
            let loser = &rows[i];
            out.push(ShadowedRegistration {
                class: loser.class.clone(),
                name: loser.name.clone(),
                descriptor: loser.descriptor.clone(),
                shadowed_at: loser.registered_by.clone(),
                shadowed_kind: loser.kind,
                winner_at: winner.registered_by.clone(),
                winner_kind: winner.kind,
            });
        }
    }
    out
}

/// Registry of native method implementations.
///
/// Maps (class, method, descriptor) triples to Rust function callbacks.
/// Uses pre-computed 128-bit hash keys (`(u64, u64)` pair from two
/// independent FNV-1a passes) for zero-allocation lookups. With a
/// 128-bit keyspace the birthday-collision probability for the ~3,000
/// registrations we do at boot is on the order of 1e-32, so no runtime
/// collision check is needed on the hot path.
/// Width of the [`NativeMethodRegistry::generation`] band reserved for one
/// registry. Far larger than the ~3,100 natives registered at boot, so
/// `registry_epoch + slots.len()` never leaves its own band; ~4,096 registries
/// can be constructed in a process before the `u32` counter wraps and bands
/// could alias (a test binary building thousands of VMs would, at worst, see a
/// stale memo re-validated against the wrong registry — hence the additional
/// full-name verification on every resolve).
pub(in crate::registry) const REGISTRY_EPOCH_STRIDE: u32 = 1 << 20;

/// Hands each `NativeMethodRegistry` its own generation band. Starts at one
/// stride so generation `0` is never a live value and stays usable as the
/// "never resolved" sentinel in `NativeCallSite`.
pub(in crate::registry) static NEXT_REGISTRY_EPOCH: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(REGISTRY_EPOCH_STRIDE);

/// The one package whose registrations can be retired at runtime — see
/// [`NativeMethodRegistry::mute_netty_tcnative_stubs`].
pub(in crate::registry) const NETTY_TCNATIVE_PACKAGE: &str = "io/netty/internal/tcnative/";

/// One entry of the registry's dense slot table — the single place a resolved
/// native lives.
///
/// A [`NativeMethodId`](crate::NativeMethodId) is an index into
/// `NativeMethodRegistry::slots`. Redeeming a handle is therefore a
/// bounds-checked array load, with none of the three string hashes `find`
/// pays. `reg_index` points back at the `registrations` entry that owns this
/// slot; it is what makes the **full-name verification** on every digest hit
/// possible (see `slot_index_for_key`), which is the property the deleted
/// `class_manager::name_to_id` map lacked.
#[derive(Copy, Clone)]
pub(in crate::registry) struct NativeSlot {
    pub(in crate::registry) callback: NativeCallback,
    pub(in crate::registry) kind: NativeKind,
    /// Index into `registrations` of the triple that currently owns this slot.
    /// Re-registration of the same triple rewrites this in place, so the slot
    /// index (and thus any handle already handed out) stays valid.
    pub(in crate::registry) reg_index: u32,
    /// Whether this callback satisfies the **leaf** contract — see
    /// [`NativeMethodRegistry::set_leaf`]. Dispatch paths that hold the
    /// `NativeMethodId` may then invoke it through `safe_native_call_leaf`
    /// instead of the full funnel.
    ///
    /// Carried on the slot rather than derived from the triple so that
    /// re-registration cannot leave a stale claim behind: the `Some(idx)` arm
    /// of [`register`](NativeMethodRegistry::register) overwrites this with
    /// whatever the *new* registration declared, and the default is `false`.
    /// A phase that re-registers a triple without opting in therefore
    /// demotes it back to the funnel, which is the safe direction.
    pub(in crate::registry) leaf: bool,
}

