// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Async stream abstraction (Phase 2, Item P2-1).
//!
//! A `Stream` is a per-context queue of asynchronous GPU work: uploads,
//! downloads, kernel launches, event records/waits, and synchronization
//! barriers. The bridge exposes the same public surface in both build
//! modes:
//!
//! - `cuda` feature: wraps an `Arc<cudarc::driver::CudaStream>` created
//!   via `CudaDevice::fork_default_stream`. `Stream::synchronize`
//!   forwards to `result::stream::synchronize` on the underlying
//!   `sys::CUstream`; `Stream::raw()` exposes that raw handle for
//!   `launch.rs` / `async_memcpy.rs` to drive launches and copies
//!   against it.
//! - stub (no `cuda` feature): records operations into an in-memory log
//!   so tests and host-side orchestration code (e.g. dependency-graph
//!   construction in `gpu-offload`) can exercise the API without a
//!   driver. Every `Stream::new` in stub mode also returns an Ok value
//!   bypassing [`crate::probe`], since the stream type is the seam
//!   that downstream Phase 2 items build on and we want their unit
//!   tests to run on machines without CUDA.
//!
//! Construction note: `Stream::new` takes a `&DeviceContext`. In stub
//! mode `DeviceContext::new` itself returns `NoDriver`, so production
//! code paths only reach `Stream::new` via the `cuda` backend.
//! Stub-mode tests have two entry points:
//!
//! - Crate-internal unit tests (`#[cfg(test)]`) use the
//!   [`Stream::for_test`] constructor below — no `DeviceContext`
//!   required.
//! - Integration tests under `tests/` use the public
//!   [`crate::DeviceContext::stub_for_testing`] constructor to obtain
//!   a stub `&DeviceContext` and then call `Stream::new(&ctx)`
//!   normally; the public stub-mode `Stream::new` ignores the
//!   `&DeviceContext` argument (it only exists for API parity with
//!   the cuda backend) and returns `Ok` with a fresh logging stream.

use crate::{DeviceContext, Result};

#[cfg(not(feature = "cuda"))]
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex,
};

// ── Operation log ─────────────────────────────────────────────────────

/// One unit of async work recorded on a [`Stream`] (stub mode) or
/// describable in the dependency-graph layer (both modes).
///
/// In stub mode the variants are appended to the stream's internal log
/// every time the corresponding `Stream` method is called, so tests can
/// assert on the exact submitted sequence. In `cuda` mode the variants
/// are not used by the stream itself (the driver owns the queue) but
/// remain part of the public type so callers can describe planned work
/// uniformly across both backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOp {
    /// Async host-to-device upload of `bytes` bytes.
    UploadAsync { bytes: usize },
    /// Async device-to-host download of `bytes` bytes.
    DownloadAsync { bytes: usize },
    /// Kernel launch with the given name and launch geometry.
    Launch {
        kernel: String,
        grid: (u32, u32, u32),
        block: (u32, u32, u32),
    },
    /// Record an event into the stream.
    EventRecord { event_id: u32 },
    /// Wait for an event on this stream.
    EventWait { event_id: u32 },
    /// Synchronize: drain all in-flight work.
    Synchronize,
    /// A host-side callback was enqueued via [`Stream::add_host_callback`].
    ///
    /// The callback closure itself is not (and cannot be, since
    /// `Box<dyn FnOnce() + Send>` is neither `Clone` nor `PartialEq`)
    /// stored in the log — this variant only records that a callback
    /// was submitted at this point in the stream's op sequence, e.g.
    /// for dependency-graph / test introspection.
    HostCallback,
}

// ── Stub-mode internals ───────────────────────────────────────────────

/// Atomic id counter for stub streams. Real streams get their id from
/// the same counter so id-equality semantics match across backends.
#[cfg(not(feature = "cuda"))]
static STREAM_ID_COUNTER: AtomicU32 = AtomicU32::new(0);

#[cfg(not(feature = "cuda"))]
pub(crate) struct StreamStub {
    ops: Mutex<Vec<StreamOp>>,
    id: u32,
}

#[cfg(not(feature = "cuda"))]
impl StreamStub {
    fn new() -> Self {
        Self {
            ops: Mutex::new(Vec::new()),
            id: STREAM_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }
}

// ── cuda-mode internals ───────────────────────────────────────────────
//
// Holds a real `Arc<cudarc::driver::CudaStream>` for the bridge. cudarc's
// `CudaStream` is `pub struct CudaStream { pub stream: sys::CUstream,
// device: Arc<CudaDevice> }`; we wrap it in an `Arc` so cloning the
// `Stream` shares the underlying handle (cudarc itself doesn't impl
// `Clone` on `CudaStream` — its `Drop` records a wait-on-default
// event, so duplicating handles is unsound). The `id` is a process-
// local counter for dependency-graph cross-referencing; not the same
// number CUDA itself uses internally.

#[cfg(feature = "cuda")]
pub(crate) struct StreamCuda {
    /// Owned cudarc stream. On drop cudarc synchronises this stream
    /// with the device's default stream (`wait_for` plus
    /// `cuStreamDestroy_v2`), so we don't have to add manual
    /// teardown here.
    pub(crate) stream: std::sync::Arc<cudarc::driver::safe::CudaStream>,
    /// AUDIT 2026-05-29 (SOUND-1 / H10c): retained owning device so
    /// `Stream::synchronize` can `bind_to_thread` before driving the
    /// raw stream handle from a possibly-different thread. Required for
    /// the `unsafe impl Send + Sync` on `Stream` to be sound.
    device: std::sync::Arc<cudarc::driver::safe::CudaDevice>,
    id: u32,
}

// cudarc 0.13's `CudaStream` does not impl `Send`/`Sync` because it
// holds a raw `sys::CUstream` (a `*mut CUstream_st`). The CUDA driver
// docs explicitly permit using a stream from any thread that has
// initialized the context (`bind_to_thread` handles that for us), so
// the raw pointer is logically thread-safe. cudarc itself impls
// `Send`/`Sync` for `CudaDevice`, `CudaModule`, and `CudaFunction` on
// the same grounds; the missing impls on `CudaStream` are simply an
// upstream oversight (filed: `coreylowman/cudarc#318`). The bridge
// asserts the same safety condition here so downstream `Arc<Stream>`
// can be stored in `Sync` statics (e.g. `vm/src/runtime/offload.rs`'s
// `SUBMISSIONS` map).
//
// # Safety
//
// AUDIT 2026-05-24 (C32, SOUND-1): this impl is sound ONLY when
// every thread that drives the `Stream` (`synchronize`,
// `record_event`, `wait_event`, or any of the bridge calls that
// pull `raw()` out and submit work to it — `DeviceModule::
// launch_on_stream`, `DeviceBuffer::to_host_async`) has first
// called `bind_to_thread` on the underlying `Arc<CudaDevice>`
// (typically reachable through the `DeviceContext` the stream was
// constructed from). The bridge does not enforce this; the CUDA
// driver returns an error or invokes undefined behaviour on an
// unbound thread. Future code that hands a `Stream` to a worker
// thread should bind the device on that worker before issuing any
// stream operation, or use the `bind_to_thread`-on-entry pattern
// `EventCuda` already follows.
#[cfg(feature = "cuda")]
// SAFETY: every stream-driving public method binds the retained device on the
// current thread before using the raw handle.
unsafe impl Send for Stream {}
#[cfg(feature = "cuda")]
// SAFETY: CUDA serializes stream operations; the retained device keeps the
// context alive and each driving thread binds it before access.
unsafe impl Sync for Stream {}

#[cfg(feature = "cuda")]
static CUDA_STREAM_ID_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

// ── Host callback trampoline (cuda mode) ────────────────────────────────
//
// `cuLaunchHostFunc` (see [`Stream::add_host_callback`]) takes a raw
// `CUhostFn = Option<unsafe extern "C" fn(*mut c_void)>` plus a single
// `void*` of user data — there is no room for a Rust closure directly.
// `Box<dyn FnOnce() + Send>` is a *fat* pointer (data ptr + vtable), so
// it does not fit in that one `void*` either; we box it a second time
// to get a thin pointer to the fat pointer, and this trampoline is the
// `extern "C"` function the driver actually calls, which unpacks it.
//
// cudarc 0.13 has no safe wrapper for `cuLaunchHostFunc` — unlike
// `cuEventRecord` / `cuEventQuery` (wrapped under
// `cudarc::driver::result::event`), the raw binding only exists in
// `cudarc::driver::sys::Lib` (see `cudarc-0.13.9/src/driver/sys/sys_12060.rs`,
// symbol resolved at `cudarc::driver::sys::lib()`). `Stream::add_host_callback`
// therefore calls the raw function table directly, exactly the way
// cudarc's own `result::event::query` does internally for `cuEventQuery`.
#[cfg(feature = "cuda")]
unsafe extern "C" fn host_callback_trampoline(user_data: *mut std::ffi::c_void) {
    // SAFETY: `user_data` was produced by `Stream::add_host_callback`
    // via `Box::into_raw` on a `Box<Box<dyn FnOnce() + Send>>`. The CUDA
    // driver invokes a given `cuLaunchHostFunc` submission's trampoline
    // exactly once and never again afterwards (per the `cuLaunchHostFunc`
    // docs), so reclaiming ownership here with `Box::from_raw` is sound
    // and cannot double-free or run twice.
    let f = unsafe { Box::from_raw(user_data as *mut Box<dyn FnOnce() + Send>) };
    // CUDA CALLBACK RULE (also documented loudly on `add_host_callback`
    // itself): `f` must NOT call any CUDA Driver or Runtime API —
    // no `Stream`/`Event`/`DeviceBuffer`/`DeviceModule` method on this
    // crate, no raw `cudarc` call, nothing. This trampoline runs on an
    // internal CUDA driver callback thread; re-entering the driver from
    // it is undefined behaviour per NVIDIA's docs and can deadlock the
    // driver's callback dispatch machinery for the whole process.
    //
    // Guard against unwinding across this `extern "C"` boundary: a
    // panic inside `f` would otherwise try to unwind into driver-owned
    // code, which is undefined behaviour. `cuLaunchHostFunc` is a
    // `void`-returning callback ABI with no channel to surface an
    // error, so a panicking callback is silently swallowed here rather
    // than propagated.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
}

// ── Public Stream type ────────────────────────────────────────────────

/// A CUDA stream: a per-context queue of asynchronous GPU work.
///
/// `Stream` is the seam used by the rest of the Phase 2 GPU-offload
/// pipeline (async memcpy, kernel pipelining, event-based dependency
/// tracking). It is intentionally `!Sync`-by-default — wrap in `Arc`
/// to share across threads after construction.
#[cfg(feature = "cuda")]
pub struct Stream {
    inner: StreamCuda,
    capturing: std::sync::atomic::AtomicBool,
}

#[cfg(not(feature = "cuda"))]
pub struct Stream {
    inner: StreamStub,
    capturing: std::sync::atomic::AtomicBool,
}

impl Stream {
    /// Whether a graph capture is open on this stream.
    ///
    /// A plain atomic rather than a `cuStreamIsCapturing` call, because
    /// the launch path reads it on every launch and the answer is
    /// something this crate already knows: `begin_capture` set it.
    ///
    /// The launch path needs it because the per-buffer `last_write`
    /// event discipline is wrong inside a capture in both directions.
    /// The completion event a captured launch records exists only
    /// inside the graph, so a later `cuStreamWaitEvent` on it from a
    /// download stream fails outright with `CUDA_ERROR_INVALID_VALUE` --
    /// and the ordering it would have provided is redundant anyway,
    /// since a single-stream capture becomes a linear chain of nodes
    /// whose dependencies the driver derives from submission order.
    pub(crate) fn is_capturing(&self) -> bool {
        self.capturing.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Record that a capture opened or closed on this stream.
    pub(crate) fn set_capturing(&self, on: bool) {
        self.capturing
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Stream {
    /// Create a new stream bound to `ctx`.
    ///
    /// In `cuda` mode this creates a fresh `CudaStream` via
    /// `CudaDevice::fork_default_stream`, which under the hood
    /// allocates a `CU_STREAM_NON_BLOCKING` stream and inserts a
    /// wait on the device's default stream so the new stream is
    /// ordered after any work already queued there at construction
    /// time. In stub mode this returns a logging stream with a
    /// fresh id; however, since [`DeviceContext::new`] itself
    /// returns `NoDriver` in stub mode, this entry point is only
    /// reachable from real (`cuda`-feature) code paths in
    /// production. Tests can use the crate-internal `for_test()`
    /// constructor on the stub backend.
    #[cfg(feature = "cuda")]
    pub fn new(ctx: &DeviceContext) -> Result<Self> {
        // `fork_default_stream` is cudarc 0.13's only public path to a
        // non-default `CudaStream`. It internally creates a
        // `CU_STREAM_NON_BLOCKING` stream and immediately records a
        // wait on the device's default stream so this stream is
        // ordered after any work already submitted to the default
        // stream at construction time — matching the documented
        // CUDA-runtime stream-fork semantics.
        let device = ctx.inner().device().clone();
        let cuda_stream = device
            .fork_default_stream()
            .map_err(|e| crate::DeviceError::Driver(format!("fork_default_stream: {e:?}")))?;
        let id = CUDA_STREAM_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Self {
            inner: StreamCuda {
                // cudarc's `CudaStream` is not `Send`/`Sync`, but the
                // wrapping `Stream` carries an explicit `unsafe impl
                // Send + Sync` (the CUDA driver permits stream use from
                // any thread once the primary context is bound — see
                // the impls below `StreamCuda`). The `Arc` only ever
                // travels inside that wrapper, so the lint's premise
                // does not hold here.
                #[allow(clippy::arc_with_non_send_sync)]
                stream: std::sync::Arc::new(cuda_stream),
                device,
                id,
            },
            capturing: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Create a new stream bound to `ctx` (stub mode).
    ///
    /// In stub mode the public constructor still consumes `ctx` for
    /// API parity with the real backend, but never reaches this code
    /// in practice because `DeviceContext::new` returns `NoDriver`.
    #[cfg(not(feature = "cuda"))]
    pub fn new(_ctx: &DeviceContext) -> Result<Self> {
        Ok(Self {
            inner: StreamStub::new(),
            capturing: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Unique id of this stream within the current process. Useful for
    /// dependency-graph nodes that need to refer to a stream without
    /// holding a reference.
    pub fn id(&self) -> u32 {
        self.inner.id
    }

    /// Wait until all previously submitted work on this stream
    /// completes.
    ///
    /// In stub mode this records [`StreamOp::Synchronize`] in the log
    /// and returns `Ok(())` — there is no real driver to call. In
    /// `cuda` mode this forwards to `cuStreamSynchronize` via
    /// `cudarc::driver::result::stream::synchronize`.
    #[cfg(feature = "cuda")]
    pub fn synchronize(&self) -> Result<()> {
        // AUDIT 2026-05-29 (SOUND-1 / H10c): bind the owning primary
        // context to this thread before driving the raw stream handle.
        // The `unsafe impl Send + Sync` on `Stream` is sound only if
        // every thread that drives the stream has first bound the
        // device; this prelude enforces it. Cheap per-thread TLS check.
        self.inner
            .device
            .bind_to_thread()
            .map_err(|e| crate::DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
        // cudarc 0.13's `CudaStream` does not expose a `synchronize`
        // method directly — the safe wrapper assumes drop-time
        // synchronisation only. We drop down to
        // `result::stream::synchronize` on the raw `sys::CUstream`,
        // mirroring what `CudaDevice::synchronize` does for the
        // default stream.
        // SAFETY: the retained device was bound above and owns the live stream
        // for the duration of this synchronous wait.
        unsafe { cudarc::driver::result::stream::synchronize(self.raw()) }
            .map_err(|e| crate::DeviceError::Driver(format!("cuStreamSynchronize: {e:?}")))
    }

    /// Crate-internal accessor returning the raw cudarc stream handle.
    /// Used by `launch.rs` and `async_memcpy.rs` to submit launches
    /// and copies onto this stream via the `result::*` namespace.
    #[cfg(feature = "cuda")]
    pub(crate) fn raw(&self) -> cudarc::driver::sys::CUstream {
        self.inner.stream.stream
    }

    /// Stub-mode counterpart: there is no real CUstream to expose, but
    /// we provide the method so cuda-mode and stub-mode callers can
    /// share a single import path. Returns the null pointer (cudarc's
    /// `result::stream::null()` equivalent), which the rest of the
    /// crate must NEVER actually pass to cudarc — stub-mode callers
    /// gate on `cfg(feature = "cuda")` before touching streams.
    #[cfg(not(feature = "cuda"))]
    #[allow(dead_code)]
    pub(crate) fn raw(&self) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }

    /// Access to the underlying `Arc<CudaStream>`. Used by
    /// `launch.rs` (`DeviceModule::launch_on_stream`) to hand the
    /// caller's `Stream` to `backend_cuda::DeviceModuleInner::
    /// launch_raw_on_stream` for true per-stream kernel submission
    /// (AUDIT 2026-05-24 C32 stream-port fix).
    /// The device this stream belongs to, for the `bind_to_thread` prelude
    /// every raw-handle use in this crate shares.
    #[cfg(feature = "cuda")]
    pub(crate) fn device_arc(&self) -> &std::sync::Arc<cudarc::driver::safe::CudaDevice> {
        &self.inner.device
    }

    #[cfg(feature = "cuda")]
    pub(crate) fn cuda_stream_arc(&self) -> &std::sync::Arc<cudarc::driver::safe::CudaStream> {
        &self.inner.stream
    }

    #[cfg(not(feature = "cuda"))]
    pub fn synchronize(&self) -> Result<()> {
        self.record_op(StreamOp::Synchronize);
        Ok(())
    }

    /// Return a cloned snapshot of the operation log.
    ///
    /// In stub mode this is the full record of `record_op` calls (in
    /// submission order). In `cuda` mode the driver owns the queue
    /// and we have no log to expose, so this returns an empty
    /// `Vec` — callers that want to inspect submitted work should do
    /// so at the dependency-graph layer, not via the stream itself.
    #[cfg(feature = "cuda")]
    pub fn ops(&self) -> Vec<StreamOp> {
        Vec::new()
    }

    #[cfg(not(feature = "cuda"))]
    pub fn ops(&self) -> Vec<StreamOp> {
        // Cloning the inner Vec — NOT draining — so repeated calls
        // observe the same history.
        self.inner
            .ops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Append an operation to the stub-mode log. No-op in `cuda` mode.
    ///
    /// Crate-private: callers in the bridge (e.g. async memcpy and
    /// launch helpers added by sibling Phase 2 items) invoke this to
    /// keep the stub-mode log faithful to the work submitted.
    #[cfg(feature = "cuda")]
    pub(crate) fn record_op(&self, _op: StreamOp) {
        // No-op: the driver owns the queue.
    }

    #[cfg(not(feature = "cuda"))]
    pub(crate) fn record_op(&self, op: StreamOp) {
        self.inner
            .ops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(op);
    }

    /// Enqueue a host-side callback that the CUDA driver runs once all
    /// work submitted to this stream *before this call* has completed.
    ///
    /// This is the non-blocking completion primitive
    /// `GpuFuture::isDone()` needs: unlike [`Event::synchronize`][sync]
    /// (blocks the calling thread) or [`Event::query`][query] (the
    /// caller must poll it), a host callback lets the driver notify the
    /// caller exactly once, from a driver-owned thread, with no
    /// busy-waiting on either side. The intended usage from a VM-side
    /// completion poller: record the buffer/kernel work as usual, then
    /// `add_host_callback` a closure that flips an atomic, completes a
    /// channel, or wakes a parked task — and let the driver do the
    /// waiting internally instead of a VM thread blocking in
    /// `Event::synchronize`.
    ///
    /// [sync]: crate::Event::synchronize
    /// [query]: crate::Event::query
    ///
    /// # CUDA callback rules
    ///
    /// The underlying `cuLaunchHostFunc` driver call imposes hard
    /// requirements on `f` (see the [CUDA docs][culaunchhostfunc]):
    ///
    /// - **`f` must not call any CUDA Driver or Runtime API function**
    ///   — no `Stream` / `Event` / `DeviceBuffer` / `DeviceModule`
    ///   method from this crate, no raw `cudarc` call, nothing. `f`
    ///   runs on an internal CUDA driver callback thread; re-entering
    ///   the driver from it is undefined behaviour and can deadlock the
    ///   driver's callback dispatch for the whole process.
    /// - `f` should be short: it blocks forward progress of any work
    ///   queued after the callback point on this stream (and, per the
    ///   CUDA docs, MAY delay host-callback delivery on other streams
    ///   too, depending on driver version) until it returns.
    /// - `f` runs exactly once, on a thread the caller does not
    ///   control — hence the `Send` bound. A panic inside `f` is caught
    ///   and discarded rather than propagated (there is no channel to
    ///   surface it through `cuLaunchHostFunc`'s `void`-returning ABI).
    ///
    /// In stub mode (no driver) every stream operation is modelled as
    /// completing instantaneously, so `f` runs synchronously, inline,
    /// on the calling thread, before this call returns.
    ///
    /// [culaunchhostfunc]: https://docs.nvidia.com/cuda/cuda-driver-api/group__CUDA__EXEC.html#group__CUDA__EXEC_1g05841eaa5f90f27124241baafb3e856f
    #[cfg(feature = "cuda")]
    pub fn add_host_callback(&self, f: Box<dyn FnOnce() + Send>) -> Result<()> {
        // AUDIT (SOUND-1 / H10c pattern): bind the owning primary
        // context to this thread before driving the stream handle —
        // same prelude every other stream-driving method in this file
        // uses.
        self.inner
            .device
            .bind_to_thread()
            .map_err(|e| crate::DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
        // Double-box: see the `host_callback_trampoline` module note
        // above for why. `user_data` is handed to the driver as an
        // opaque `void*`; ownership transfers to the trampoline on
        // success (reclaimed there via `Box::from_raw`), or is
        // reclaimed right here on a synchronous submission failure.
        let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(f);
        let user_data = Box::into_raw(boxed) as *mut std::ffi::c_void;
        // SAFETY: the stream is live in the bound context; `user_data` is a
        // thin Box allocation transferred to the exactly-once trampoline.
        let result = unsafe {
            cudarc::driver::sys::lib().cuLaunchHostFunc(
                self.raw(),
                Some(host_callback_trampoline),
                user_data,
            )
        };
        match result.result() {
            Ok(()) => {
                self.record_op(StreamOp::HostCallback);
                Ok(())
            }
            Err(e) => {
                // `cuLaunchHostFunc` failed synchronously — the driver
                // will never call the trampoline for this submission,
                // so it will never reclaim `user_data`. Reclaim (and
                // drop) it here or it leaks forever.
                // SAFETY: submission failed synchronously, so ownership never
                // transferred and this is the original Box::into_raw pointer.
                unsafe {
                    drop(Box::from_raw(user_data as *mut Box<dyn FnOnce() + Send>));
                }
                Err(crate::DeviceError::Driver(format!(
                    "cuLaunchHostFunc: {e:?}"
                )))
            }
        }
    }

    /// Stub-mode counterpart. There is no driver queue and stub-mode
    /// streams model all work as completing instantaneously (see the
    /// module doc), so `f` runs synchronously on the calling thread
    /// before this call returns — this always succeeds. Also records
    /// [`StreamOp::HostCallback`] so op-log-driven tests can assert a
    /// callback was submitted at the right point in the sequence.
    #[cfg(not(feature = "cuda"))]
    pub fn add_host_callback(&self, f: Box<dyn FnOnce() + Send>) -> Result<()> {
        self.record_op(StreamOp::HostCallback);
        f();
        Ok(())
    }
}

// ── Test-only stub constructor ────────────────────────────────────────

#[cfg(all(test, not(feature = "cuda")))]
impl Stream {
    /// Construct a stub-mode `Stream` without going through
    /// `DeviceContext::new` (which returns `NoDriver` in stub mode).
    ///
    /// This is the entry point unit tests use to exercise the
    /// recording semantics without a CUDA toolkit. Not exposed
    /// outside the crate; sibling Phase 2 items that need a stub
    /// stream in their own tests should add their own analogous
    /// test-only constructor or take an injected stream.
    pub(crate) fn for_test() -> Self {
        Self {
            inner: StreamStub::new(),
            capturing: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

// ── Tests (stub mode only) ────────────────────────────────────────────

#[cfg(all(test, not(feature = "cuda")))]
mod tests {
    use super::*;

    #[test]
    fn stream_new_assigns_unique_ids() {
        // We bypass `Stream::new(&DeviceContext)` because
        // `DeviceContext::new` returns `NoDriver` in stub mode. The
        // `for_test` constructor exercises the same id-allocation
        // path, which is what we actually care about.
        let a = Stream::for_test();
        let b = Stream::for_test();
        assert_ne!(a.id(), b.id(), "ids must be unique per construction");
    }

    #[test]
    fn record_op_appends_to_log() {
        let s = Stream::for_test();
        s.record_op(StreamOp::UploadAsync { bytes: 128 });
        s.record_op(StreamOp::Launch {
            kernel: "add".to_string(),
            grid: (4, 1, 1),
            block: (32, 1, 1),
        });
        s.record_op(StreamOp::DownloadAsync { bytes: 128 });
        let ops = s.ops();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0], StreamOp::UploadAsync { bytes: 128 });
        assert!(matches!(
            ops[1],
            StreamOp::Launch { ref kernel, grid: (4, 1, 1), block: (32, 1, 1) } if kernel == "add"
        ));
        assert_eq!(ops[2], StreamOp::DownloadAsync { bytes: 128 });
    }

    #[test]
    fn synchronize_records_op() {
        let s = Stream::for_test();
        s.synchronize().expect("stub synchronize must be Ok");
        let ops = s.ops();
        assert_eq!(ops, vec![StreamOp::Synchronize]);
    }

    #[test]
    fn ops_returns_clone_not_drain() {
        let s = Stream::for_test();
        s.record_op(StreamOp::EventRecord { event_id: 7 });
        s.record_op(StreamOp::EventWait { event_id: 7 });
        let snapshot_a = s.ops();
        let snapshot_b = s.ops();
        // Calling ops() twice must yield the same history — i.e. it
        // does not drain the underlying log.
        assert_eq!(snapshot_a, snapshot_b);
        assert_eq!(snapshot_a.len(), 2);
        // And further recording must still work afterward.
        s.record_op(StreamOp::Synchronize);
        assert_eq!(s.ops().len(), 3);
    }

    #[test]
    fn add_host_callback_runs_synchronously_in_stub_mode() {
        // Stub-mode contract: "stream is always drained" — the closure
        // must have already run by the time `add_host_callback`
        // returns, with no thread hop and no polling required.
        let s = Stream::for_test();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_clone = ran.clone();
        s.add_host_callback(Box::new(move || {
            ran_clone.store(true, Ordering::Relaxed);
        }))
        .expect("add_host_callback must succeed in stub mode");
        assert!(
            ran.load(Ordering::Relaxed),
            "callback must have run by the time add_host_callback returns in stub mode"
        );
    }

    #[test]
    fn add_host_callback_records_op() {
        let s = Stream::for_test();
        s.add_host_callback(Box::new(|| {}))
            .expect("add_host_callback");
        let ops = s.ops();
        assert_eq!(
            ops,
            vec![StreamOp::HostCallback],
            "stub op log should contain exactly one HostCallback"
        );
    }

    #[test]
    fn add_host_callback_runs_exactly_once() {
        // Guard against a trampoline/box-reclamation bug that would
        // either drop the closure without calling it or call it twice.
        let s = Stream::for_test();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = count.clone();
        s.add_host_callback(Box::new(move || {
            count_clone.fetch_add(1, Ordering::Relaxed);
        }))
        .expect("add_host_callback");
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn add_host_callback_interleaves_with_other_ops_in_log_order() {
        // The op log should reflect submission order across mixed op
        // kinds, the same way EventRecord/EventWait do today.
        let s = Stream::for_test();
        s.record_op(StreamOp::UploadAsync { bytes: 64 });
        s.add_host_callback(Box::new(|| {}))
            .expect("add_host_callback");
        s.record_op(StreamOp::DownloadAsync { bytes: 64 });
        assert_eq!(
            s.ops(),
            vec![
                StreamOp::UploadAsync { bytes: 64 },
                StreamOp::HostCallback,
                StreamOp::DownloadAsync { bytes: 64 },
            ]
        );
    }
}
