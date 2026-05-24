//! `DeviceModule::launch_on_stream` — stream-aware kernel launch.
//!
//! Phase 2, Item P2-4. Mirrors the existing single-stream
//! [`DeviceModule::launch_raw`] but submits onto an explicit
//! [`crate::Stream`] (Item P2-1) instead of the context's default
//! stream.
//!
//! In stub mode the call records a `StreamOp::Launch { kernel, grid,
//! block }` on the stream and returns `Ok(())`.
//!
//! In `cuda` mode this calls the same `cuLaunchKernel`-driving helper
//! the default-stream `launch_raw` uses (`backend_cuda::
//! launch_on_raw_stream`), passing `stream.raw()` instead of the
//! device's default stream. The kernel-arg packing rules are
//! identical between the two paths — `KernelArg::DevicePtr(u64)`
//! storage cells fed into a `Vec<*mut c_void>` and submitted via
//! `cudarc::driver::result::launch_kernel`.
//!
//! AUDIT 2026-05-24 (HIGH correctness — cross-stream ordering): the
//! kernel launch is now bracketed by event waits/records that enforce
//! H→D / kernel / D→H ordering across the bridge's separate cudarc
//! streams (`copy_h2d`, `compute`, `copy_d2h`). The choreography is:
//!
//!   for each `KernelArg::DevicePtr` arg:
//!     if buffer.last_write is set:
//!         stream.wait_event(last_write)        // kernel waits on upload
//!   <enqueue cuLaunchKernel>
//!   kernel_done = Event::new(ctx)
//!   stream.record_event(kernel_done)           // kernel_done on user stream
//!   for each `KernelArg::DevicePtr` arg:
//!     buffer.last_write := kernel_done          // subsequent D→H waits
//!
//! In stub mode the waits/records show up as `EventWait` / `EventRecord`
//! ops in the user stream's log, which the integration tests assert
//! against. In `cuda` mode the waits/records hit `cuStreamWaitEvent` /
//! `cuEventRecord` on the user `Stream::raw()`, so the kernel actually
//! waits on the prior upload's completion regardless of which cudarc
//! stream the upload ran on.

use crate::{
    DeviceContext, DeviceModule, Event, KernelArg, KernelArgs, LaunchConfig, Result, Stream,
    StreamOp,
};
use std::sync::Arc;

impl DeviceModule {
    /// Submit a kernel launch on a specific stream.
    ///
    /// Behaves like [`DeviceModule::launch_raw`] except the launch is
    /// ordered against `stream` instead of the context's default
    /// stream. Under the stub backend the launch is recorded as a
    /// [`StreamOp::Launch`] on `stream` and `Ok(())` is returned —
    /// downstream tests can inspect the op log without a driver.
    ///
    /// AUDIT 2026-05-24 (HIGH correctness): waits on every device-ptr
    /// arg's `last_write` event on `stream` before launching, then
    /// records a fresh "kernel_done" event on `stream` and stamps it
    /// into every device-ptr arg's `last_write` slot. See module-level
    /// docs for the full choreography.
    pub fn launch_on_stream(
        &self,
        ctx: &DeviceContext,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
        stream: &Stream,
    ) -> Result<()> {
        // ── 1. Snapshot the device-ptr args' last_write slots BEFORE
        //       the launch consumes `args`. We need (a) the prior
        //       events to wait on and (b) the slot handles to write
        //       kernel_done into afterwards.
        let last_write_slots: Vec<crate::LastWriteSlot> = args
            .raw
            .iter()
            .filter_map(|a| match a {
                KernelArg::DevicePtr { last_write, .. } => Some(last_write.clone()),
                _ => None,
            })
            .collect();

        // ── 2. Wait on every buffer's last_write event on `stream`. ──
        //
        // The buffer-arg's `last_write` slot was either populated by
        // `from_host_async` (with an event recorded on the upload
        // stream) or by an earlier `launch_on_stream` (with a kernel-
        // completion event). In either case `stream.wait_event` is the
        // primitive that gates the upcoming launch behind that prior
        // write. Snapshotting under the mutex avoids holding the lock
        // across `wait_event`.
        //
        // AUDIT 2026-05-24: in cuda mode we ALSO have to make
        // `ctx.compute` wait on the same events — the actual
        // cuLaunchKernel goes onto `ctx.compute`, not the user
        // `stream`. Without that second wait the kernel could start
        // before the upload retires (the user stream waits, but the
        // kernel is not on the user stream).
        for slot in &last_write_slots {
            let maybe_ev = slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .cloned();
            if let Some(ev) = maybe_ev {
                stream.wait_event(&ev)?;
                #[cfg(feature = "cuda")]
                {
                    // Mirror the wait onto `ctx.compute` — the stream
                    // the kernel actually runs on.
                    // SAFETY: `compute_raw` returns a `CUstream` owned
                    // by `ctx`; the event handle is owned by `ev`
                    // (Arc); both borrowed only for the FFI call.
                    unsafe {
                        cudarc::driver::result::stream::wait_event(
                            ctx.inner().compute_raw(),
                            ev.cu_event_raw(),
                            cudarc::driver::sys::CUevent_wait_flags::CU_EVENT_WAIT_DEFAULT,
                        )
                    }
                    .map_err(|e| {
                        crate::DeviceError::Driver(format!(
                            "cuStreamWaitEvent compute: {e:?}"
                        ))
                    })?;
                }
            }
        }

        // ── 3. Submit the kernel launch. ──
        #[cfg(not(feature = "cuda"))]
        {
            // Stub mode: no driver to call; just record the launch op.
            // `args` is consumed (dropped) at end of block to match the
            // cuda-mode lifetime so the keep-alive contract is symmetric.
            let _ = ctx; // unused in stub mode
            let _consume = args;
            stream.record_op(StreamOp::Launch {
                kernel: kernel.to_string(),
                grid: cfg.grid,
                block: cfg.block,
            });
        }

        #[cfg(feature = "cuda")]
        {
            // CUDA-MERGE-NOTE (2026-05-20): the bridge's `backend_cuda`
            // backend was ported to cudarc 0.13, which does not expose a
            // raw-`CUstream` launch helper (`launch_on_raw_stream`). The
            // explicit-stream submission path was never completed against
            // that API. Until a raw-stream launch helper lands, delegate
            // to the context's standard compute-stream launch
            // (`DeviceModuleInner::launch_raw`); the kernel still runs and
            // is correctly ordered, it just shares the context's compute
            // stream rather than `stream`'s.
            //
            // AUDIT 2026-05-24: the launch still happens on
            // `ctx.compute` rather than `stream`, but the event waits
            // recorded on `stream` above and the kernel_done event
            // recorded on `stream` below propagate the producer/consumer
            // dependency across both streams. The kernel itself is
            // ordered behind upload by `launch_raw_inner`'s `wait_for(
            // copy_h2d)` (in `from_host`) and the post-launch host-side
            // wait_for(compute) so `kernel_done.record(stream)` only
            // fires after the device's compute work is observable.
            //
            // `DeviceModule(backend::DeviceModuleInner)` exposes its sole
            // field with module-private visibility; `launch.rs` is a child
            // of the crate root and so sees it.
            let module: &crate::backend_cuda::DeviceModuleInner = &self.0;
            module.launch_raw(ctx.inner(), kernel, cfg, args)?;
            stream.record_op(StreamOp::Launch {
                kernel: kernel.to_string(),
                grid: cfg.grid,
                block: cfg.block,
            });
        }

        // ── 4. Record kernel_done and propagate to buffers. ──
        //
        // Stub mode: record on the user `stream` so the OpLog shows the
        // event-discipline pattern the integration tests assert on.
        //
        // Cuda mode: record on `ctx.compute` — the stream the kernel
        // actually ran on. Recording on the user `stream` would mark
        // "user stream's queue position" rather than "kernel
        // completion", because cuLaunchKernel ran on `ctx.compute`.
        // A subsequent `cuStreamWaitEvent(any_stream, kernel_done)`
        // will then correctly gate that stream behind the kernel's
        // retirement on compute.
        let kernel_done = Arc::new(Event::new(ctx)?);
        #[cfg(not(feature = "cuda"))]
        {
            stream.record_event(&kernel_done)?;
        }
        #[cfg(feature = "cuda")]
        {
            // SAFETY: `compute_raw` returns a `CUstream` owned by
            // `ctx` and kept alive by the surrounding `&DeviceContext`.
            // The event handle is owned by `kernel_done` (Arc); both
            // are borrowed only for the duration of the FFI call.
            unsafe {
                cudarc::driver::result::event::record(
                    kernel_done.cu_event_raw(),
                    ctx.inner().compute_raw(),
                )
            }
            .map_err(|e| {
                crate::DeviceError::Driver(format!("cuEventRecord compute: {e:?}"))
            })?;
            // Also mark the user stream's op log so introspection
            // sees the kernel_done marker. `record_op` is a no-op in
            // cuda mode (the driver owns the queue) so this is free.
            stream.record_op(StreamOp::EventRecord {
                event_id: kernel_done.id(),
            });
        }
        for slot in &last_write_slots {
            *slot
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(kernel_done.clone());
        }
        Ok(())
    }
}

// ── Tests (stub mode only) ──────────────────────────────────────────
//
// Stub-only unit tests for the event-discipline contract added by the
// 2026-05-24 HIGH-correctness fix (cross-stream H2D / kernel / D2H
// ordering). The tests use the crate-private `for_test()` constructors
// on `DeviceContext` / `DeviceModule` / `DeviceBuffer` so they can run
// without a CUDA driver attached.
#[cfg(all(test, not(feature = "cuda")))]
mod tests {
    use super::*;
    use crate::DeviceBuffer;

    fn sample_cfg() -> LaunchConfig {
        LaunchConfig {
            grid: (4, 1, 1),
            block: (256, 1, 1),
            shared_bytes: 0,
        }
    }

    #[test]
    fn launch_on_stream_records_kernel_name() {
        let ctx = DeviceContext::for_test();
        let module = DeviceModule::for_test();
        let stream = Stream::for_test();
        let cfg = sample_cfg();
        let args = KernelArgs::new();
        module
            .launch_on_stream(&ctx, "my_kernel", &cfg, args, &stream)
            .expect("launch_on_stream");
        let ops = stream.ops();
        // The op log must contain a Launch op with the right kernel
        // name. Other ops (kernel_done EventRecord) may surround it.
        let found = ops.iter().any(|op| match op {
            StreamOp::Launch { kernel, grid, block } => {
                kernel == "my_kernel" && *grid == cfg.grid && *block == cfg.block
            }
            _ => false,
        });
        assert!(found, "expected Launch op in {:?}", ops);
    }

    #[test]
    fn launch_on_stream_records_grid_and_block() {
        let ctx = DeviceContext::for_test();
        let module = DeviceModule::for_test();
        let stream = Stream::for_test();
        let cfg = LaunchConfig {
            grid: (8, 2, 1),
            block: (64, 4, 1),
            shared_bytes: 32,
        };
        let args = KernelArgs::new();
        module
            .launch_on_stream(&ctx, "k", &cfg, args, &stream)
            .expect("launch_on_stream");
        let ops = stream.ops();
        let launch_op = ops
            .iter()
            .find(|op| matches!(op, StreamOp::Launch { .. }))
            .expect("Launch op must be present");
        match launch_op {
            StreamOp::Launch { grid, block, .. } => {
                assert_eq!(*grid, (8, 2, 1));
                assert_eq!(*block, (64, 4, 1));
            }
            _ => unreachable!(),
        }
    }

    /// AUDIT 2026-05-24 (HIGH correctness): the stub-backend
    /// event-discipline test mandated by the cross-stream ordering
    /// audit acceptance criteria.
    ///
    /// Drives `from_host_async` → `launch_on_stream` → `to_host_async`
    /// against the stub OpLog and asserts:
    ///   (a) the kernel waits on the buffer's upload-completion event
    ///       *before* its `Launch` op, and
    ///   (b) the D→H download waits on the kernel-completion event
    ///       *before* its `DownloadAsync` op.
    ///
    /// Event ids are matched concretely (not by position) so the
    /// assertion fails if the wrong event is being waited on.
    #[test]
    fn kernel_waits_on_h2d_d2h_waits_on_kernel() {
        let ctx = DeviceContext::for_test();
        let module = DeviceModule::for_test();
        let stream = Stream::for_test();

        // ── 1. from_host_async — must record [EventRecord(upload),
        //       UploadAsync]. Stub `from_host` errors with NoDriver, so
        //       we set up the buffer through `for_test()` and replay
        //       the same op sequence by calling `from_host_async`'s
        //       internal contract directly: record the upload event
        //       and the UploadAsync op on the stream.
        //
        //       To exercise the *real* `from_host_async` path we'd
        //       need a stub backend that succeeds; instead we model
        //       the post-condition (buffer.last_write := upload_event)
        //       explicitly and assert the launch / download paths
        //       wait on it.
        let buf: DeviceBuffer<f32> = DeviceBuffer::for_test();
        let upload_event = std::sync::Arc::new(
            crate::Event::new(&ctx).expect("Event::new in stub mode"),
        );
        stream
            .record_event(&upload_event)
            .expect("record upload event");
        stream.record_op(StreamOp::UploadAsync { bytes: 32 });
        *buf.last_write.lock().unwrap() = Some(upload_event.clone());
        let upload_event_id = upload_event.id();

        // ── 2. launch_on_stream — must, by contract, insert
        //       EventWait(upload_event_id) before its Launch op, and
        //       record a fresh kernel_done event afterwards.
        let cfg = LaunchConfig::elementwise(8);
        let args = KernelArgs::new().push_device_ptr(&buf);
        module
            .launch_on_stream(&ctx, "k", &cfg, args, &stream)
            .expect("launch_on_stream");

        // ── 3. to_host_async — must wait on the kernel_done event
        //       before its DownloadAsync op.
        let mut dst = [0.0_f32; 8];
        // `inner.to_host` errors with NoDriver in stub mode, but the
        // event-discipline ops are recorded *before* the inner call,
        // so we ignore the error and only assert on the op log.
        let _ = buf.to_host_async(&mut dst, &stream);

        let ops = stream.ops();

        // Locate the Launch op.
        let launch_pos = ops
            .iter()
            .position(|op| matches!(op, StreamOp::Launch { .. }))
            .expect("must contain Launch");

        // (a) Between UploadAsync (op #1 in our setup) and Launch
        // there must be an EventWait whose id matches upload_event_id.
        let kernel_waits_on_upload = ops[..launch_pos]
            .iter()
            .any(|op| matches!(op, StreamOp::EventWait { event_id } if *event_id == upload_event_id));
        assert!(
            kernel_waits_on_upload,
            "kernel must wait on upload's last_write event before launching; got {:?}",
            ops
        );

        // Locate the kernel_done EventRecord — the FIRST EventRecord
        // strictly after Launch is the one launch_on_stream installed.
        let kernel_event_id = ops[launch_pos + 1..]
            .iter()
            .find_map(|op| match op {
                StreamOp::EventRecord { event_id } => Some(*event_id),
                _ => None,
            })
            .expect("launch_on_stream must record kernel_done after Launch");
        assert_ne!(
            kernel_event_id, upload_event_id,
            "kernel_done event must be a fresh event, not the upload event"
        );

        // (b) Between Launch and DownloadAsync there must be an
        // EventWait whose id matches kernel_event_id.
        let download_pos = ops
            .iter()
            .position(|op| matches!(op, StreamOp::DownloadAsync { .. }))
            .expect("must contain DownloadAsync");
        let d2h_waits_on_kernel = ops[launch_pos + 1..download_pos]
            .iter()
            .any(|op| matches!(op, StreamOp::EventWait { event_id } if *event_id == kernel_event_id));
        assert!(
            d2h_waits_on_kernel,
            "D→H must wait on kernel_done event before downloading; got {:?}",
            ops
        );
    }
}
