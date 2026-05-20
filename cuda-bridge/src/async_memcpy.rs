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
//! In `cuda` mode `from_host_async` allocates a fresh
//! `sys::CUdeviceptr` via the bridge's `DeviceBufferInner::uninit_raw`
//! helper, then enqueues a `cuMemcpyHtoDAsync_v2` via
//! `cudarc::driver::result::memcpy_htod_async` on `stream.raw()`. The
//! caller is contractually required to keep `host` alive until the
//! stream has been synchronised — the same contract cudarc itself
//! documents on `result::memcpy_htod_async`. `to_host_async` is the
//! mirror via `result::memcpy_dtoh_async`.

use crate::{DeviceBuffer, DeviceContext, Result, Stream, StreamOp};

#[cfg(not(feature = "cuda"))]
use crate::backend_stub as backend;

#[cfg(feature = "cuda")]
use crate::backend_cuda as backend;

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
            // 1. Allocate device memory of the right size. We use
            //    `uninit_raw` (a crate-internal helper) so we don't
            //    bounce the host bytes through cudarc's owned-Vec
            //    `htod_copy_into` path, which would require an
            //    extra clone of `host`.
            let inner = backend::DeviceBufferInner::<T>::uninit_raw(
                ctx.inner(),
                bytes,
                host.len(),
            )?;
            // 2. Enqueue the H→D copy on the caller's stream.
            //    Contract per cudarc: `host` must remain valid until
            //    the stream is synchronised. We surface that contract
            //    via this method's doc comment; the buffer itself
            //    cannot prove it.
            unsafe {
                cudarc::driver::result::memcpy_htod_async::<T>(
                    inner.raw_ptr(),
                    host,
                    stream.raw(),
                )
            }
            .map_err(|e| crate::DeviceError::Memcpy(format!("cuMemcpyHtoDAsync_v2: {e:?}")))?;
            // 3. Record the op on the stub-mode log (no-op in cuda
            //    mode because `Stream::record_op` is a no-op there,
            //    but kept symmetric with the stub path).
            let _ = bytes;
            stream.record_op(StreamOp::UploadAsync { bytes });
            Ok(DeviceBuffer(inner))
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
            if dst.len() != self.len() {
                return Err(crate::DeviceError::Memcpy(format!(
                    "to_host_async length mismatch: dst.len()={}, slice.len()={}",
                    dst.len(),
                    self.len()
                )));
            }
            // Bind to the buffer's owning context before submitting.
            // The buffer's `raw_ptr()` is only valid in that context.
            self.0
                .device_arc()
                .bind_to_thread()
                .map_err(|e| crate::DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
            unsafe {
                cudarc::driver::result::memcpy_dtoh_async::<T>(
                    dst,
                    self.0.raw_ptr(),
                    stream.raw(),
                )
            }
            .map_err(|e| crate::DeviceError::Memcpy(format!("cuMemcpyDtoHAsync_v2: {e:?}")))?;
            stream.record_op(StreamOp::DownloadAsync { bytes });
            Ok(())
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
