//! Async stream abstraction (Phase 2, Item P2-1).
//!
//! A `Stream` is a per-context queue of asynchronous GPU work: uploads,
//! downloads, kernel launches, event records/waits, and synchronization
//! barriers. The bridge exposes the same public surface in both build
//! modes:
//!
//! - `cuda` feature: PHASE2-CUDA-TODO — wraps a real cudarc stream
//!   once the cudarc 0.13 `CudaDevice`/`fork_default_stream` migration
//!   in `backend_cuda.rs` lands. Currently every cuda-mode entry point
//!   returns `DeviceError::NoDriver` so the `--features cuda` build
//!   compiles cleanly.
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
// PHASE2-CUDA-TODO: `StreamCuda` should hold an `Arc<cudarc::driver::CudaStream>`
// once `backend_cuda.rs` is migrated to the cudarc 0.13 API. Today it
// is a zero-sized marker so the module compiles under the `cuda`
// feature.

#[cfg(feature = "cuda")]
pub(crate) struct StreamCuda {
    id: u32,
}

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
    /// In `cuda` mode this would create a fresh `CudaStream` on the
    /// device underlying `ctx`. PHASE2-CUDA-TODO: today it returns
    /// `NoDriver` because `backend_cuda` itself does not yet expose
    /// the cudarc 0.13 device handle. In stub mode this returns a
    /// logging stream with a fresh id; however, since
    /// [`DeviceContext::new`] itself returns `NoDriver` in stub mode,
    /// this entry point is only reachable from real (`cuda`-feature)
    /// code paths. Tests can use the crate-internal `for_test()`
    /// constructor on the stub backend.
    #[cfg(feature = "cuda")]
    pub fn new(_ctx: &DeviceContext) -> Result<Self> {
        // PHASE2-CUDA-TODO: implement against cudarc 0.13 `CudaDevice::
        // fork_default_stream` once `backend_cuda.rs` is ported. The
        // current stub returns NoDriver so the build compiles.
        let id = CUDA_STREAM_ID_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = id; // suppress dead_code until the cuda body lands
        Err(crate::DeviceError::Driver(
            "Stream::new: cuda backend not yet implemented (PHASE2-CUDA-TODO)".to_string(),
        ))
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
    /// `cuda` mode this forwards to `cuStreamSynchronize` via cudarc
    /// (PHASE2-CUDA-TODO: currently NoDriver).
    #[cfg(feature = "cuda")]
    pub fn synchronize(&self) -> Result<()> {
        // PHASE2-CUDA-TODO: forward to cudarc's `CudaStream::synchronize`
        // once the cudarc 0.13 migration lands.
        Err(crate::DeviceError::NoDriver)
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
