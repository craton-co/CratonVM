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
//! mode `DeviceContext::new` itself returns `NoDriver`, so the public
//! constructor here can only ever be reached via the `cuda` backend in
//! real use. To let the stub-mode unit tests (and downstream items'
//! tests) build a `Stream` without going through `DeviceContext`, we
//! expose a `#[cfg(test)] for_test()` constructor on the stub backend.

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
#[cfg(feature = "cuda")]
unsafe impl Send for Stream {}
#[cfg(feature = "cuda")]
unsafe impl Sync for Stream {}

#[cfg(feature = "cuda")]
static CUDA_STREAM_ID_COUNTER: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

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
}

#[cfg(not(feature = "cuda"))]
pub struct Stream {
    inner: StreamStub,
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
        let id = CUDA_STREAM_ID_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                id,
            },
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
        // cudarc 0.13's `CudaStream` does not expose a `synchronize`
        // method directly — the safe wrapper assumes drop-time
        // synchronisation only. We drop down to
        // `result::stream::synchronize` on the raw `sys::CUstream`,
        // mirroring what `CudaDevice::synchronize` does for the
        // default stream.
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

    /// Access to the underlying `Arc<CudaStream>`. Reserved for any
    /// future bridge code that needs to retain a reference to the
    /// stream that produced a buffer (cudarc allocations are
    /// stream-affine). Currently unused; `#[allow(dead_code)]` keeps
    /// the lint clean.
    #[cfg(feature = "cuda")]
    #[allow(dead_code)]
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
            .expect("stream op log poisoned")
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
            .expect("stream op log poisoned")
            .push(op);
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
}
