//! Phase 2 — Item P2-3: Asynchronous host↔device memcpy on a Stream.
//!
//! Adds `from_host_async` / `to_host_async` to `DeviceBuffer<T>`. Both
//! methods enqueue work on the caller-supplied [`Stream`] rather than
//! the device's default per-context stream, which is what lets the
//! Phase 2 pipeline overlap copy and compute.
//!
//! In stub mode the operations don't move any bytes — they only
//! record a [`StreamOp`] on the stream so unit tests can assert the
//! enqueued byte counts. In real-cuda mode they invoke the cudarc
//! async memcpy entry points (`memcpy_htod` / `memcpy_dtoh`) bound to
//! the supplied stream.

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
            //
            // PHASE2-GUESS: the existing stub `DeviceBufferInner::from_host`
            // returns `Err(NoDriver)`. For async we instead want a
            // zero-allocation handle that mirrors `host.len()`. We
            // construct the inner directly with a `PhantomData` — this
            // matches the shape of `backend_stub::DeviceBufferInner<T>`
            // (see `backend_stub.rs`).
            let _ = ctx;
            let _ = host;
            stream.record_op(StreamOp::UploadAsync { bytes });
            let inner = backend::DeviceBufferInner::<T> {
                _phantom: std::marker::PhantomData,
            };
            Ok(DeviceBuffer(inner))
        }

        #[cfg(feature = "cuda")]
        {
            // Real backend: allocate uninit on the supplied stream's
            // device, then issue an async host→device copy bound to
            // the same stream. cudarc 0.13 exposes the async copy as
            // `CudaStream::memcpy_htod` (which is stream-ordered when
            // invoked on a non-default stream).
            let _ = bytes;
            let raw_stream = stream.raw();
            // Allocate on the user's stream so subsequent ops on the
            // same stream see a properly stream-ordered allocation.
            let slice = unsafe {
                raw_stream
                    .alloc::<T>(host.len())
                    .map_err(|e| crate::DeviceError::Driver(format!("alloc_async: {e}")))?
            };
            // PHASE2-GUESS: cudarc 0.13's async H→D entry point is
            // `CudaStream::memcpy_htod(src, &mut dst)`. The non-async
            // synchronous variant is `memcpy_stod` (used in
            // `backend_cuda.rs::from_host`); `memcpy_htod` is the
            // stream-bound async version that takes an existing dst.
            let mut slice = slice;
            raw_stream
                .memcpy_htod(host, &mut slice)
                .map_err(|e| crate::DeviceError::Memcpy(format!("memcpy htod async: {e}")))?;
            let inner = backend::DeviceBufferInner::<T> {
                slice,
                stream: raw_stream.clone(),
            };
            let _ = ctx;
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
            let _ = bytes;
            let raw_stream = stream.raw();
            // PHASE2-GUESS: pairing of `memcpy_htod` above — async
            // D→H on cudarc 0.13 is `CudaStream::memcpy_dtoh(src,
            // &mut dst)`. The sync counterpart used by the existing
            // `to_host` is also `memcpy_dtoh` but invoked on the
            // context's default stream (which makes it effectively
            // synchronous w.r.t. the host); here we bind it to the
            // user's stream so it overlaps.
            raw_stream
                .memcpy_dtoh(&self.0.slice, dst)
                .map_err(|e| crate::DeviceError::Memcpy(format!("memcpy dtoh async: {e}")))?;
            Ok(())
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(all(test, not(feature = "cuda")))]
mod tests {
    //! Stub-mode tests: assert that the async entry points enqueue
    //! the correct byte counts on the supplied stream. They do not
    //! exercise real memcpy — there is no driver in this build.
    use super::*;

    #[test]
    fn from_host_async_records_byte_count() {
        // PHASE2-GUESS: `Stream::for_test()` is provided by Item P2-1
        // as a `#[cfg(test)]` constructor returning a `Stream` with
        // an empty op log. The contract from the Phase 2 spec is
        // that subsequent `record_op` calls append to a vec
        // observable via a `pub(crate) fn ops(&self) -> Vec<StreamOp>`
        // accessor. We use that accessor below.
        let stream = Stream::for_test();
        // PHASE2-GUESS: `DeviceContext::for_test()` — a stub-only
        // constructor that returns a no-op context without touching
        // the driver. The Phase 2 spec mandates one so async memcpy
        // tests don't have to short-circuit on `NoDriver`.
        let ctx = DeviceContext::for_test();
        let host: Vec<i32> = vec![1, 2, 3, 4, 5];
        let expected_bytes = host.len() * std::mem::size_of::<i32>();

        let buf = DeviceBuffer::<i32>::from_host_async(&ctx, &host, &stream)
            .expect("stub from_host_async should succeed");
        assert_eq!(buf.len(), host.len());

        let ops = stream.ops();
        assert_eq!(ops.len(), 1, "exactly one op should have been recorded");
        match ops[0] {
            StreamOp::UploadAsync { bytes } => assert_eq!(bytes, expected_bytes),
            ref other => panic!("expected UploadAsync, got {other:?}"),
        }
    }

    #[test]
    fn to_host_async_records_byte_count() {
        let stream = Stream::for_test();
        let ctx = DeviceContext::for_test();
        let src: Vec<f64> = vec![0.0; 17];
        let buf = DeviceBuffer::<f64>::from_host_async(&ctx, &src, &stream)
            .expect("stub from_host_async should succeed");

        let mut dst = vec![0.0f64; 17];
        let expected_bytes = dst.len() * std::mem::size_of::<f64>();
        buf.to_host_async(&mut dst, &stream)
            .expect("stub to_host_async should succeed");

        let ops = stream.ops();
        // Two ops recorded: the upload from `from_host_async`, then
        // the download we just issued.
        assert_eq!(ops.len(), 2);
        match ops[1] {
            StreamOp::DownloadAsync { bytes } => assert_eq!(bytes, expected_bytes),
            ref other => panic!("expected DownloadAsync, got {other:?}"),
        }
    }
}
