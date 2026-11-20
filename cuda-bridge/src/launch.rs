// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

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
//! In `cuda` mode this routes the launch through
//! [`backend_cuda::DeviceModuleInner::launch_raw_on_stream`], the real
//! per-stream launch helper, passing the user `Stream`'s underlying
//! cudarc `Arc<CudaStream>` (via `Stream::cuda_stream_arc()`). The
//! kernel therefore actually runs on the caller's stream rather than the
//! context's shared `compute` stream, so two kernels submitted on two
//! different user streams can run concurrently instead of serialising on
//! `ctx.compute`. The kernel-arg packing rules are shared with the
//! default-stream `launch_raw` path (both funnel into
//! `launch_raw_on_stream_inner`).
//!
//! AUDIT 2026-05-29 (HIGH-2 fix — user stream honoured): previously this
//! delegated to `DeviceModuleInner::launch_raw`, which hard-routes the
//! launch onto `ctx.compute`; the user `stream` was used only for event
//! bookkeeping, so cross-stream concurrency was lost and `launch_raw`'s
//! internal per-buffer event stamping collided with the event this
//! function stamps (a second, orphaned `CUevent` per launch). Switching
//! to `launch_raw_on_stream` (which deliberately does NOT touch `e_h2d`,
//! `e_k`, or the `last_write` slots — see its doc) makes this function
//! the single owner of the cross-stream ordering choreography:
//!
//!   for each `KernelArg::DevicePtr` arg:
//!     if buffer.last_write is set:
//!         stream.wait_event(last_write)        // kernel waits on upload
//!   <enqueue cuLaunchKernel on `stream`>
//!   kernel_done = Event::new(ctx)
//!   stream.record_event(kernel_done)           // kernel_done on user stream
//!   for each `KernelArg::DevicePtr` arg:
//!     buffer.last_write := kernel_done          // subsequent D→H waits
//!
//! In stub mode the waits/records show up as `EventWait` / `EventRecord`
//! ops in the user stream's log, which the integration tests assert
//! against. In `cuda` mode the waits/records hit `cuStreamWaitEvent` /
//! `cuEventRecord` on the user `Stream::raw()`, and because the kernel
//! now runs on that same stream, `kernel_done` recorded on `stream`
//! genuinely marks the kernel's retirement.

use crate::{
    DeviceContext, DeviceModule, Event, KernelArg, KernelArgs, LaunchConfig, Result, Stream,
    StreamOp,
};
use std::sync::Arc;

fn clear_last_write_slots(slots: &[crate::LastWriteSlot]) {
    for slot in slots {
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

fn recover_after_completion_event_failure(
    stream: &Stream,
    last_write_slots: &[crate::LastWriteSlot],
    #[cfg(feature = "gpu-driver")] keep_alives: &[crate::backend::BufferKeepAlive],
    err: crate::DeviceError,
) -> Result<()> {
    match stream.synchronize() {
        Ok(()) => {
            clear_last_write_slots(last_write_slots);
            #[cfg(feature = "gpu-driver")]
            for ka in keep_alives {
                crate::backend::note_keep_alive_fence(ka, None, None);
            }
            Err(err)
        }
        Err(sync_err) => Err(crate::DeviceError::Launch(format!(
            "kernel completion event recording failed ({err}); stream synchronize cleanup failed \
             ({sync_err})"
        ))),
    }
}

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
        // AUDIT 2026-05-29 (SOUND-1 / H10c): bind the owning primary
        // context to this thread before driving any CUDA handle
        // (streams, events, the kernel launch). The `unsafe impl
        // Send + Sync` blocks across the bridge are sound only when the
        // driving thread has bound the device first; this prelude
        // enforces it for the launch path. Cheap per-thread TLS check.
        #[cfg(feature = "gpu-driver")]
        ctx.inner().bind_to_thread()?;

        // ── 1. Snapshot the device-ptr args' last_write slots BEFORE
        //       the launch consumes `args`. We need (a) the prior
        //       events to wait on and (b) the slot handles to write
        //       kernel_done into afterwards.
        //       Under `cuda`, also retain each arg's `_keep_alive` until
        //       step 4 records `kernel_done` so any buffer whose handle
        //       was dropped before launch gets fenced before retirement.
        #[cfg(feature = "gpu-driver")]
        let mut keep_alives: Vec<crate::backend::BufferKeepAlive> = Vec::with_capacity(args.raw.len());
        let mut last_write_slots: Vec<crate::LastWriteSlot> = Vec::with_capacity(args.raw.len());
        for a in &args.raw {
            if let KernelArg::DevicePtr {
                last_write,
                #[cfg(feature = "gpu-driver")]
                _keep_alive,
                ..
            } = a
            {
                if !last_write_slots.iter().any(|s| Arc::ptr_eq(s, last_write)) {
                    last_write_slots.push(last_write.clone());
                }
                #[cfg(feature = "gpu-driver")]
                if !keep_alives.iter().any(|k| Arc::ptr_eq(k, _keep_alive)) {
                    keep_alives.push(_keep_alive.clone());
                }
            }
        }

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
        // AUDIT 2026-05-29 (HIGH-2 fix): the kernel now actually runs on
        // the user `stream` (step 3 routes through
        // `launch_raw_on_stream`), so gating the user stream behind each
        // input's `last_write` event is sufficient — there is no longer
        // a second, hidden launch on `ctx.compute` to mirror the wait
        // onto. (The previous code launched on `ctx.compute` and had to
        // duplicate every wait there.)
        //
        // Skipped entirely while `stream` is capturing. Inside a
        // capture the ordering these waits provide is already there:
        // the driver derives a linear dependency chain from submission
        // order on a single stream, so node N+1 already depends on
        // node N. The waits would meanwhile be `cuStreamWaitEvent` on
        // events recorded OUTSIDE the capture, which is exactly the
        // class of call that invalidates one.
        let capturing = stream.is_capturing();
        if !capturing {
            for slot in &last_write_slots {
                let maybe_ev = slot
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    .cloned();
                if let Some(ev) = maybe_ev {
                    stream.wait_event(&ev)?;
                }
            }
        }

        // ── 3. Submit the kernel launch. ──
        // Allocate the completion event before submitting the kernel.
        // If event creation fails, no kernel has been queued and the
        // buffer ordering slots still describe the pre-launch state.
        //
        // No event at all while capturing: `cuEventRecord` on a
        // capturing stream records a node in the graph rather than a
        // host-observable event, so the handle it produces cannot be
        // waited on from another stream -- a later `to_host` would fail
        // with `CUDA_ERROR_INVALID_VALUE` rather than reading the
        // buffer. Nothing needs it: the graph orders its own nodes, and
        // a caller waits on the replay's completion event instead.
        let kernel_done = if capturing {
            None
        } else {
            Some(Arc::new(Event::new(ctx)?))
        };

        #[cfg(not(feature = "gpu-driver"))]
        {
            // Stub mode: no driver to call; just record the launch op.
            // `args` is consumed (dropped) at end of block to match the
            // cuda-mode lifetime so the keep-alive contract is symmetric.
            // (`ctx` is still used by step 4's `Event::new(ctx)`.)
            let _consume = args;
            stream.record_op(StreamOp::Launch {
                kernel: kernel.to_string(),
                grid: cfg.grid,
                block: cfg.block,
            });
        }

        #[cfg(feature = "gpu-driver")]
        {
            // AUDIT 2026-05-29 (HIGH-2 fix): launch on the user-supplied
            // `stream`, not `ctx.compute`. `backend_cuda` exposes the
            // real per-stream launch helper `launch_raw_on_stream`, which
            // funnels the same arg-marshalling as the default-stream path
            // but submits `cuLaunchKernel` onto the `Arc<CudaStream>` we
            // hand it. We pass the caller's stream via
            // `Stream::cuda_stream_arc()`.
            //
            // Crucially, `launch_raw_on_stream` does NOT wait on `e_h2d`,
            // record `e_k`, or stamp the `last_write` slots — it leaves
            // all cross-stream ordering to the caller. That makes this
            // function the SOLE owner of the event choreography (step 2's
            // waits and step 4's `kernel_done` record + slot stamping),
            // so there is no longer a duplicate/orphaned completion event
            // per launch (the prior `launch_raw` delegation stamped its
            // own per-buffer event that step 4 then overwrote).
            //
            // `DeviceModule(backend::DeviceModuleInner)` exposes its sole
            // field with module-private visibility; `launch.rs` is a child
            // of the crate root and so sees it.
            let module: &crate::backend::DeviceModuleInner = &self.0;
            module.launch_raw_on_stream(
                ctx.inner(),
                stream.cuda_stream_arc(),
                kernel,
                cfg,
                args,
            )?;
            stream.record_op(StreamOp::Launch {
                kernel: kernel.to_string(),
                grid: cfg.grid,
                block: cfg.block,
            });
        }

        // ── 4. Record kernel_done and propagate to buffers. ──
        //
        // AUDIT 2026-05-29 (HIGH-2 fix): the kernel now runs on the user
        // `stream`, so `kernel_done` is recorded on `stream` itself — its
        // queue position genuinely marks the kernel's retirement. (Under
        // the old `ctx.compute` launch this had to record on
        // `ctx.compute` instead, because the user stream's queue position
        // said nothing about the kernel.) `Stream::record_event` calls
        // `cuEventRecord` in cuda mode and appends `StreamOp::EventRecord`
        // in stub mode, so the same call serves both backends; a
        // subsequent `cuStreamWaitEvent(any_stream, kernel_done)`
        // correctly gates that stream behind this kernel.
        let Some(kernel_done) = kernel_done else {
            // Capturing. There is no event to stamp -- one recorded on a
            // capturing stream lives inside the graph and no other
            // stream can wait on it -- and leaving the OLD event would
            // be worse than clearing, because it describes a write from
            // before this graph and a download released by it would be
            // correctly ordered against the wrong thing.
            //
            // So: clear now, and hand the slots to the stream. The graph
            // takes custody of them at `end_capture`, and every replay
            // stamps them with its own completion event
            // (`GraphExec::launch`). The window in which these buffers
            // have no `last_write` is exactly the window in which
            // nothing has written them -- a capture runs nothing -- so
            // the per-buffer ordering contract holds throughout rather
            // than being suspended for the life of the graph.
            for slot in &last_write_slots {
                *slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
            #[cfg(feature = "cuda")]
            if let Ok(Some(node)) = stream.capturing_node() {
                stream.note_captured_node(node.0 as usize, &keep_alives, &last_write_slots);
            } else {
                stream.note_captured_slots(&last_write_slots);
                stream.note_captured_keep_alives(&keep_alives);
            }
            #[cfg(all(feature = "gpu-driver", not(feature = "cuda")))]
            {
                stream.note_captured_slots(&last_write_slots);
                stream.note_captured_keep_alives(&keep_alives);
            }
            #[cfg(not(feature = "gpu-driver"))]
            stream.note_captured_slots(&last_write_slots);
            return Ok(());
        };
        if let Err(err) = stream.record_event(&kernel_done) {
            return recover_after_completion_event_failure(
                stream,
                &last_write_slots,
                #[cfg(feature = "gpu-driver")]
                &keep_alives,
                err,
            );
        }
        #[cfg(feature = "gpu-driver")]
        {
            let sid = stream.stream_id();
            for ka in &keep_alives {
                crate::backend::note_keep_alive_fence(ka, Some(sid), Some(kernel_done.clone()));
            }
        }
        for slot in &last_write_slots {
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(kernel_done.clone());
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
#[cfg(all(test, not(feature = "gpu-driver")))]
mod tests;
