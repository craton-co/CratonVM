// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Event type and `Stream::record_event` / `Stream::wait_event`.
//!
//! A CUDA event is a synchronisation point recorded on one stream that
//! other streams (or the CPU) can wait on. Events are how Phase 2
//! expresses happens-before relations between concurrent streams.
//!
//! In stub mode the type is a tiny state-machine — every event carries
//! a unique id (atomic counter) and an `Option<u32>` that becomes
//! `Some(stream_id)` once a `Stream::record_event` runs. The stream
//! itself also appends `StreamOp::EventRecord` / `StreamOp::EventWait`
//! entries to its op log so tests can inspect cross-stream
//! dependencies without a real GPU.
//!
//! In `cuda` mode `Event` wraps a raw `sys::CUevent` created via
//! `cudarc::driver::result::event::create` and forwards `synchronize`
//! / `query` / `destroy` to the same `result::event` namespace.
//! `Stream::record_event` calls `result::event::record(event, stream)`
//! and `Stream::wait_event` calls `result::stream::wait_event(stream,
//! event, CU_EVENT_WAIT_DEFAULT)` on the raw `sys::CUstream` exposed
//! by `Stream::raw()`.

use crate::{DeviceContext, DeviceError, Result, Stream};
use std::sync::atomic::{AtomicU32, Ordering};

/// Monotonic event-id counter. Each `Event::new` bumps this; the value
/// is independent of the cuda backend (used by stub op logs and as a
/// generic test/debug aid).
static NEXT_EVENT_ID: AtomicU32 = AtomicU32::new(1);

fn next_event_id() -> u32 {
    NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A CUDA event — a synchronisation point recorded on a stream. Other
/// streams can wait on this event to enforce a happens-before relation
/// across streams (or across CPU and GPU).
///
/// Events are created via [`Event::new`] and recorded by passing them
/// to [`Stream::record_event`]. Recording is not implicit on
/// construction.
pub struct Event {
    #[cfg(feature = "cuda")]
    inner: EventCuda,
    #[cfg(not(feature = "cuda"))]
    inner: EventStub,
    id: u32,
}

#[cfg(not(feature = "cuda"))]
pub(crate) struct EventStub {
    /// `Some(stream_id)` after a successful `Stream::record_event`.
    pub(crate) recorded_on: std::sync::Mutex<Option<u32>>,
}

// cudarc 0.13 has no `CudaEvent` safe wrapper, so we hold the raw
// `sys::CUevent` directly and free it in `Drop`. The event is bound
// to the device's primary context — we retain an `Arc<CudaDevice>`
// to keep that context alive for the event's lifetime.
#[cfg(feature = "cuda")]
struct EventCuda {
    cu_event: cudarc::driver::sys::CUevent,
    device: std::sync::Arc<cudarc::driver::safe::CudaDevice>,
    /// The context free list this handle came from and returns to.
    /// See `backend_cuda::DeviceContextInner::event_pool`.
    pool: std::sync::Arc<crate::backend_cuda::EventPool>,
    /// Raw `CUstream` this event was last recorded on, or `0`.
    ///
    /// Read by [`Stream::wait_event`] to skip a wait that cannot mean
    /// anything: work already queued on a stream is ordered before work
    /// queued after it, so `cuStreamWaitEvent(S, E)` where `E` was
    /// recorded on `S` is a driver round trip for a guarantee the stream
    /// already gives. That is not a rare case -- it is EVERY launch in a
    /// chunked dispatch after the first, and every launch in a
    /// single-stream chain like GPULlama3's 453-kernel forward pass,
    /// because each one waits on the `last_write` its predecessor
    /// stamped onto the same stream.
    recorded_on: std::sync::atomic::AtomicUsize,
}

// # Safety
//
// AUDIT 2026-05-24 (C32, SOUND-1): `EventCuda` holds a raw
// `sys::CUevent` (`*mut CUevent_st`) and an `Arc<CudaDevice>`. The
// CUDA driver permits using a CUevent from any thread that has the
// owning primary context bound. `Event::new` binds the current
// thread (`event.rs:106`) and `EventCuda::drop` re-binds before
// destruction (`event.rs:76`), so creation and teardown are sound.
// However, the cross-thread-callable methods (`Event::synchronize`,
// `Event::query`, and `Stream::record_event`/`wait_event` from a
// worker thread) do NOT call `bind_to_thread` — they assume the
// caller's thread is already bound to the device the event was
// created on. Driving an event from an unbound thread is undefined
// per the CUDA driver model. See `DeviceContext`'s `# Safety`
// paragraph in `lib.rs` for the bridge-wide caller contract.
#[cfg(feature = "cuda")]
// SAFETY: every public operation binds the retained owning device on the
// current thread before using the raw event handle.
unsafe impl Send for EventCuda {}
#[cfg(feature = "cuda")]
// SAFETY: CUDA serializes event operations and the retained device keeps the
// handle's context alive; each driving thread binds that context first.
unsafe impl Sync for EventCuda {}

#[cfg(feature = "cuda")]
impl Drop for EventCuda {
    fn drop(&mut self) {
        // Return the handle to the context's free list rather than
        // destroying it. `EventPool::put` destroys it itself once the
        // pool is full, and the pool destroys everything it holds when
        // the context goes away -- so no handle outlives its context and
        // none leaks. `bind_to_thread` is paid only on those two paths,
        // not on the common one.
        //
        // Safe to recycle even with work in flight: a `cuStreamWaitEvent`
        // already issued against this handle captured the event's
        // contents at the time of that call, so re-recording the handle
        // for a later submission cannot reach back and satisfy it. The
        // handle is uniquely ours here by construction -- `Drop` runs
        // only when the last `Arc<Event>` is gone.
        self.pool.put(self.cu_event);
    }
}

impl Event {
    /// Create a fresh event. The event is NOT yet recorded — call
    /// [`Stream::record_event`] to associate it with a stream.
    #[cfg(not(feature = "cuda"))]
    pub fn new(_ctx: &DeviceContext) -> Result<Self> {
        // Stub mode: no real driver, but event construction is a
        // pure-Rust operation (allocating a Mutex + assigning an id),
        // so it succeeds. The lack of a driver only manifests once
        // the caller actually tries to drive the event from a stream
        // that would need to do work — that's handled at the
        // recording / synchronize site.
        Ok(Self {
            inner: EventStub {
                recorded_on: std::sync::Mutex::new(None),
            },
            id: next_event_id(),
        })
    }

    #[cfg(feature = "cuda")]
    pub fn new(ctx: &DeviceContext) -> Result<Self> {
        let device = ctx.inner().device().clone();
        let pool = ctx.inner().event_pool().clone();
        // Recycled first: a pooled handle costs a `Vec::pop` where a
        // fresh one costs `cuEventCreate` now and `cuEventDestroy`
        // later. This path runs twice per kernel submission and
        // GPULlama3 makes 453 of those per token.
        //
        // `bind_to_thread` stays on BOTH paths deliberately. Callers of
        // `Event::new` are entitled to a bound context afterwards --
        // `backend_cuda::record_compute_event`'s own comment says as much
        // -- and skipping it here would make that guarantee depend on
        // whether the pool happened to have a spare handle, which is the
        // worst possible shape for a latent unbound-context bug.
        device
            .bind_to_thread()
            .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
        if let Some(cu_event) = pool.take() {
            cratonvm_types::gpu_event_census::note_recycled();
            return Ok(Self {
                inner: EventCuda {
                    cu_event,
                    device,
                    pool,
                    recorded_on: std::sync::atomic::AtomicUsize::new(0),
                },
                id: next_event_id(),
            });
        }
        // `CU_EVENT_DISABLE_TIMING` matches cudarc's own per-device
        // sync event: we don't want the (slightly more expensive)
        // timing variant since the bridge never measures elapsed
        // GPU time.
        let cu_event = cudarc::driver::result::event::create(
            cudarc::driver::sys::CUevent_flags::CU_EVENT_DISABLE_TIMING,
        )
        .map_err(|e| DeviceError::Driver(format!("cuEventCreate: {e:?}")))?;
        cratonvm_types::gpu_event_census::note_created();
        Ok(Self {
            inner: EventCuda {
                cu_event,
                device,
                pool,
                recorded_on: std::sync::atomic::AtomicUsize::new(0),
            },
            id: next_event_id(),
        })
    }

    /// Remember which raw stream last recorded this event, so
    /// [`Stream::wait_event`] can elide a same-stream wait. Cuda-mode
    /// only; the stub backend tracks the same thing as a stream id in
    /// `EventStub::recorded_on`.
    #[cfg(feature = "cuda")]
    pub(crate) fn set_recorded_on(&self, stream: cudarc::driver::sys::CUstream) {
        self.inner
            .recorded_on
            .store(stream as usize, Ordering::Relaxed);
    }

    /// The raw stream this event was last recorded on, or `0`.
    #[cfg(feature = "cuda")]
    pub(crate) fn recorded_on_raw(&self) -> usize {
        self.inner.recorded_on.load(Ordering::Relaxed)
    }

    /// Unique id (test / debug aid). Stable for the lifetime of the
    /// `Event`. Independent of the cuda handle.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// AUDIT 2026-05-24 (HIGH correctness): crate-internal accessor
    /// returning the underlying `sys::CUevent` handle so callers in
    /// `lib.rs` can `cuEventRecord` against arbitrary cudarc streams
    /// (specifically `ctx.copy_h2d` for `from_host_async`'s upload-
    /// completion marker, and `ctx.compute` for `launch_on_stream`'s
    /// kernel-completion marker). Cuda-mode only — the stub backend
    /// has no real event handle.
    #[cfg(feature = "cuda")]
    #[allow(dead_code)]
    pub(crate) fn cu_event_raw(&self) -> cudarc::driver::sys::CUevent {
        self.inner.cu_event
    }

    /// Block until this event is reached on whatever stream recorded
    /// it. Returns `Err(DeviceError::NoDriver)` in stub mode if the
    /// event was never recorded.
    #[cfg(not(feature = "cuda"))]
    pub fn synchronize(&self) -> Result<()> {
        let guard = self
            .inner
            .recorded_on
            .lock()
            .map_err(|_| DeviceError::Driver("event recorded_on mutex poisoned".to_string()))?;
        if guard.is_none() {
            // The spec mandates: stub `synchronize` errors with
            // NoDriver if not recorded. Once recorded, the stub
            // considers all work instantaneous — no driver, no wait.
            return Err(DeviceError::NoDriver);
        }
        Ok(())
    }

    #[cfg(feature = "cuda")]
    pub fn synchronize(&self) -> Result<()> {
        // AUDIT 2026-05-29 (SOUND-1 / H10c): bind the owning primary
        // context to this thread before driving the event. The
        // `unsafe impl Send + Sync` on `EventCuda` is sound only if
        // every thread that drives the handle has first bound the
        // device; this prelude enforces it. Cheap per-thread TLS check.
        self.inner
            .device
            .bind_to_thread()
            .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
        // `cuEventSynchronize` is a host-side wait: returns when the
        // event has completed on whatever stream recorded it. If the
        // event was never recorded the call returns immediately (the
        // CUDA driver treats an unrecorded event as already-complete).
        // SAFETY: the owning device was bound above and keeps this event live.
        unsafe { cudarc::driver::result::event::synchronize(self.inner.cu_event) }
            .map_err(|e| DeviceError::Driver(format!("cuEventSynchronize: {e:?}")))
    }

    /// Returns `true` if the recorded work has completed. Returns
    /// `false` if the work is still in flight, OR if the event has
    /// not yet been recorded.
    ///
    /// In stub mode "recorded" is sufficient — there is no driver to
    /// keep work pending — so this returns `Ok(true)` exactly when
    /// the event has been recorded on some stream, and `Ok(false)`
    /// otherwise.
    #[cfg(not(feature = "cuda"))]
    pub fn query(&self) -> Result<bool> {
        let guard = self
            .inner
            .recorded_on
            .lock()
            .map_err(|_| DeviceError::Driver("event recorded_on mutex poisoned".to_string()))?;
        Ok(guard.is_some())
    }

    #[cfg(feature = "cuda")]
    pub fn query(&self) -> Result<bool> {
        // AUDIT 2026-05-29 (SOUND-1 / H10c): bind the owning primary
        // context to this thread before querying the event (see
        // `synchronize`).
        self.inner
            .device
            .bind_to_thread()
            .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
        // cudarc's `result::event::query` returns `Ok(())` when the
        // event has fired and an `Err(CUDA_ERROR_NOT_READY)` when it
        // is still in flight. We map the not-ready code to
        // `Ok(false)` so callers don't have to grep for the specific
        // driver error variant; anything else is a genuine failure.
        // SAFETY: the owning device was bound above and keeps this event live.
        match unsafe { cudarc::driver::result::event::query(self.inner.cu_event) } {
            Ok(()) => Ok(true),
            Err(e) => {
                use cudarc::driver::sys::CUresult;
                if e.0 == CUresult::CUDA_ERROR_NOT_READY {
                    Ok(false)
                } else {
                    Err(DeviceError::Driver(format!("cuEventQuery: {e:?}")))
                }
            }
        }
    }
}

impl Stream {
    /// Record `event` at the current point in this stream's queue.
    ///
    /// In stub mode this:
    /// 1. Writes `event.recorded_on = Some(self.id())`.
    /// 2. Appends `StreamOp::EventRecord { event_id: event.id() }`
    ///    to this stream's op log.
    ///
    /// In cuda mode this would call `cuEventRecord` on the stream's
    /// raw handle. Returns once the *queue* of operations has been
    /// updated — it does not wait for the event to fire.
    #[cfg(not(feature = "cuda"))]
    pub fn record_event(&self, event: &Event) -> Result<()> {
        // Update the event's recorded-on field first so a concurrent
        // observer that sees the op-log entry will also see the
        // event's state.
        let mut guard = event
            .inner
            .recorded_on
            .lock()
            .map_err(|_| DeviceError::Driver("event recorded_on mutex poisoned".to_string()))?;
        *guard = Some(self.id());
        drop(guard);
        self.record_op(crate::StreamOp::EventRecord {
            event_id: event.id(),
        });
        Ok(())
    }

    #[cfg(feature = "cuda")]
    pub fn record_event(&self, event: &Event) -> Result<()> {
        // AUDIT 2026-05-29 (SOUND-1 / H10c): bind the owning primary
        // context to this thread before driving the stream/event. The
        // event carries the `Arc<CudaDevice>`; binding through it also
        // binds the context the stream belongs to (same primary
        // context per device). Cheap per-thread TLS check.
        event
            .inner
            .device
            .bind_to_thread()
            .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
        // `cuEventRecord(event, stream)` enqueues the event onto the
        // stream's command queue. Subsequent `cuEventQuery` /
        // `cuEventSynchronize` calls observe completion of any work
        // ahead of this point on the stream.
        // SAFETY: event and stream share the bound owning context and remain
        // live for this synchronous enqueue.
        unsafe { cudarc::driver::result::event::record(event.inner.cu_event, self.raw()) }
            .map_err(|e| DeviceError::Driver(format!("cuEventRecord: {e:?}")))?;
        event.set_recorded_on(self.raw());
        Ok(())
    }

    /// Make this stream wait for `event`. All subsequent work on this
    /// stream waits until `event` is reached on its recording stream.
    ///
    /// In stub mode this appends `StreamOp::EventWait { event_id }`
    /// to this stream's op log. It does NOT require the event to
    /// have been recorded already — `cuStreamWaitEvent` itself
    /// permits waiting on a not-yet-recorded event (the wait simply
    /// becomes a no-op).
    #[cfg(not(feature = "cuda"))]
    pub fn wait_event(&self, event: &Event) -> Result<()> {
        self.record_op(crate::StreamOp::EventWait {
            event_id: event.id(),
        });
        Ok(())
    }

    #[cfg(feature = "cuda")]
    pub fn wait_event(&self, event: &Event) -> Result<()> {
        // An event recorded on THIS stream needs no wait: everything
        // queued on a stream before a point is already ordered before
        // everything queued after it. Skipping is exactly equivalent and
        // saves a driver round trip on the hottest ordering edge there
        // is -- a chain of kernels on one stream, where each launch would
        // otherwise wait on the `last_write` its predecessor stamped
        // there. See `EventCuda::recorded_on`.
        if event.recorded_on_raw() == self.raw() as usize {
            cratonvm_types::gpu_event_census::note_wait_elided();
            return Ok(());
        }
        cratonvm_types::gpu_event_census::note_wait_issued();
        // AUDIT 2026-05-29 (SOUND-1 / H10c): bind the owning primary
        // context to this thread before driving the stream/event (see
        // `record_event`).
        event
            .inner
            .device
            .bind_to_thread()
            .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
        // `cuStreamWaitEvent(stream, event, CU_EVENT_WAIT_DEFAULT)`
        // inserts a barrier on this stream that blocks all subsequent
        // submissions until `event` fires on whichever stream
        // recorded it. The wait itself does not block the host.
        // SAFETY: event and stream share the bound owning context and remain
        // live for this synchronous enqueue.
        unsafe {
            cudarc::driver::result::stream::wait_event(
                self.raw(),
                event.inner.cu_event,
                cudarc::driver::sys::CUevent_wait_flags::CU_EVENT_WAIT_DEFAULT,
            )
        }
        .map_err(|e| DeviceError::Driver(format!("cuStreamWaitEvent: {e:?}")))
    }
}

// ─────────────────────────── tests ───────────────────────────────────

#[cfg(all(test, not(feature = "cuda")))]
mod tests {
    //! Stub-mode unit tests for `Event` and the new `Stream` methods.
    //!
    //! These run on the default workspace build (no `cuda` feature).
    //! They depend on `Stream::for_test()` — a test-only constructor
    //! supplied by Item P2-1's `stream.rs`. The real-cuda path is
    //! exercised on a GPU host where the driver actually performs
    //! the work; here we just verify the op-log bookkeeping that
    //! drives the stub-mode integration tests under
    //! `tests/stub_op_log.rs`.
    use super::*;
    use crate::StreamOp;

    #[test]
    fn event_new_assigns_unique_ids() {
        let ctx = match crate::DeviceContext::new(0) {
            Ok(c) => c,
            Err(_) => return, // stub path: nothing to verify without a ctx
        };
        let a = Event::new(&ctx).expect("event a");
        let b = Event::new(&ctx).expect("event b");
        assert_ne!(a.id(), b.id(), "event ids must be unique");
    }

    #[test]
    fn record_event_writes_recorded_on_field() {
        // Build a synthetic event by hand. This bypasses the
        // `DeviceContext`-requiring `Event::new` so the test runs
        // under stub mode without depending on probe(). We exercise
        // exactly the state the `record_event` contract mutates.
        let ev = Event {
            inner: EventStub {
                recorded_on: std::sync::Mutex::new(None),
            },
            id: next_event_id(),
        };
        let stream = Stream::for_test();
        // Initial state: not recorded; query returns false.
        assert_eq!(ev.query().unwrap(), false);
        stream.record_event(&ev).expect("record_event");
        // After recording: recorded_on holds the stream's id,
        // and query flips to true.
        let recorded = ev.inner.recorded_on.lock().unwrap().clone();
        assert_eq!(recorded, Some(stream.id()));
        assert_eq!(ev.query().unwrap(), true);
    }

    #[test]
    fn record_event_pushes_op_to_stream_log() {
        let ev = Event {
            inner: EventStub {
                recorded_on: std::sync::Mutex::new(None),
            },
            id: next_event_id(),
        };
        let stream = Stream::for_test();
        stream.record_event(&ev).expect("record_event");
        let ops = stream.ops();
        assert_eq!(
            ops,
            vec![StreamOp::EventRecord { event_id: ev.id() }],
            "stream op log should contain exactly one EventRecord"
        );
    }

    #[test]
    fn wait_event_pushes_op_to_stream_log() {
        let ev = Event {
            inner: EventStub {
                recorded_on: std::sync::Mutex::new(None),
            },
            id: next_event_id(),
        };
        let stream_a = Stream::for_test();
        let stream_b = Stream::for_test();
        // Record on A, wait on B — the typical cross-stream pattern.
        stream_a.record_event(&ev).expect("record_event");
        stream_b.wait_event(&ev).expect("wait_event");
        let ops_b = stream_b.ops();
        assert_eq!(
            ops_b,
            vec![StreamOp::EventWait { event_id: ev.id() }],
            "wait stream should contain exactly one EventWait"
        );
    }

    #[test]
    fn query_unrecorded_returns_false() {
        let ev = Event {
            inner: EventStub {
                recorded_on: std::sync::Mutex::new(None),
            },
            id: next_event_id(),
        };
        assert_eq!(
            ev.query().expect("query"),
            false,
            "an event that has never been recorded must query as false"
        );
        // And `synchronize` on an unrecorded event must surface
        // NoDriver per the spec's stub contract.
        match ev.synchronize() {
            Err(DeviceError::NoDriver) => {}
            other => panic!("expected NoDriver, got {other:?}"),
        }
    }
}
