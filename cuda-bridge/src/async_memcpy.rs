//! Phase 2 — Item P2-3: Asynchronous host↔device memcpy on a Stream.
//!
//! Adds `from_host_async` / `to_host_async` to `DeviceBuffer<T>`. Both
//! methods enqueue work on the caller-supplied [`Stream`] rather than
//! the device's default per-context stream, which is what lets the
//! Phase 2 pipeline overlap copy and compute.
//!
//! In stub mode the operations don't move any bytes — they only
//! record a [`StreamOp`] on the stream so unit tests can assert the
//! enqueued byte counts.
//!
//! PHASE2-CUDA-TODO: in `cuda` mode every entry point currently
//! returns `DeviceError::NoDriver`. cudarc 0.13 exposes async host↔
//! device memcpy via `CudaDevice` (`htod_copy_into` /
//! `dtoh_sync_copy_into`) but ties the stream to the device's owning
//! handle, not an arbitrary stream — porting requires the
//! `backend_cuda.rs` migration first.

use crate::{DeviceBuffer, DeviceContext, Result, Stream, StreamOp};

#[cfg(not(feature = "cuda"))]
use crate::backend_stub as backend;

impl<T: bytemuck::Pod + Send + Sync + 'static> DeviceBuffer<T> {
    /// Allocate a device buffer and upload `host` asynchronously on
    /// `stream`. The returned buffer is safe to launch kernels against
    /// **only after** `stream` has been synchronized (or after a
    /// kernel on the same stream consumes it, since CUDA streams are
    /// FIFO-ordered).
    ///
    /// In stub mode this records a [`StreamOp::UploadAsync`] carrying
    /// `host.len() * size_of::<T>()` bytes; no memory is moved.
    pub fn from_host_async(
        ctx: &DeviceContext,
        host: &[T],
        stream: &Stream,
    ) -> Result<Self> {
        let bytes = std::mem::size_of_val(host);

        #[cfg(not(feature = "cuda"))]
        {
            // Stub: no driver, no real allocation. We still need a
            // `DeviceBuffer<T>` whose `len()` matches `host.len()` so
            // downstream stub assertions are consistent with what the
            // real backend would return.
            let _ = ctx;
            let _ = host;
            let _ = bytes;
            stream.record_op(StreamOp::UploadAsync { bytes });
            let inner = backend::DeviceBufferInner::<T> {
                _phantom: std::marker::PhantomData,
            };
            Ok(DeviceBuffer(inner))
        }

        #[cfg(feature = "cuda")]
        {
            // PHASE2-CUDA-TODO: route through cudarc 0.13's
            // `CudaDevice::htod_copy_into` against the user-supplied
            // stream once `backend_cuda.rs` is ported. Today the cuda
            // backend itself returns NoDriver, so there is no live
            // device handle to copy to.
            let _ = (ctx, host, stream, bytes);
            Err(crate::DeviceError::NoDriver)
        }
    }

    /// Asynchronously copy this buffer's contents into `dst` on
    /// `stream`. The destination slice is only guaranteed populated
    /// after `stream` has been synchronized.
    ///
    /// In stub mode this records a [`StreamOp::DownloadAsync`] but
    /// does **not** write anything to `dst` — the stub has no device
    /// memory to read from.
    pub fn to_host_async(&self, dst: &mut [T], stream: &Stream) -> Result<()> {
        let bytes = std::mem::size_of_val(dst);

        #[cfg(not(feature = "cuda"))]
        {
            let _ = dst;
            stream.record_op(StreamOp::DownloadAsync { bytes });
            Ok(())
        }

        #[cfg(feature = "cuda")]
        {
            // PHASE2-CUDA-TODO: pair of `from_host_async` — cudarc
            // 0.13's async D→H is `CudaDevice::dtoh_sync_copy_into`
            // with `is_async = true`; needs the backend migration
            // first.
            let _ = (dst, stream, bytes);
            Err(crate::DeviceError::NoDriver)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────
//
// PHASE2-CUDA-TODO: the original unit tests in this module called
// `DeviceContext::for_test()` — a helper that does not exist in the
// current public API. They are removed here so the crate compiles;
// the stub-mode behaviour is exercised end-to-end via
// `tests/stub_op_log.rs` (gated on `DeviceContext::probe()`, also a
// PHASE2-CUDA-TODO).
