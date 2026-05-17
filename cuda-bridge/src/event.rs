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
//! PHASE2-CUDA-TODO: in `cuda` mode every entry point currently
//! returns `DeviceError::NoDriver`. The pre-existing `backend_cuda.rs`
//! in this workspace was written against a cudarc API surface (named
//! `CudaContext`, `result::event::*`, `cudarc::driver::sys::CUevent`)
//! that does not exist on the pinned `cudarc = "0.13"`; once the
//! backend is ported, this module should grow a real `EventCuda`
//! holding `cudarc::driver::sys::CUevent` and route through cudarc's
//! `result::event::create/destroy/synchronize/query` and
//! `result::event::record` / `result::stream::wait_event`.

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

// PHASE2-CUDA-TODO: `EventCuda` should hold a `cudarc::driver::sys::CUevent`
// (or an `Arc<cudarc::driver::CudaEvent>` if a future cudarc adds a
// safe wrapper) once `backend_cuda.rs` is ported to the cudarc 0.13
// API. Today it is a zero-sized marker so the module compiles under
// the `cuda` feature.
#[cfg(feature = "cuda")]
struct EventCuda {
    _marker: (),
}

#[cfg(feature = "cuda")]
unsafe impl Send for EventCuda {}
#[cfg(feature = "cuda")]
unsafe impl Sync for EventCuda {}

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
    pub fn new(_ctx: &DeviceContext) -> Result<Self> {
        // PHASE2-CUDA-TODO: cuEventCreate via cudarc 0.13's
        // `result::event::create`. Stubbed until `backend_cuda.rs`
        // exposes a working device context.
        Err(DeviceError::NoDriver)
    }

    /// Unique id (test / debug aid). Stable for the lifetime of the
    /// `Event`. Independent of the cuda handle.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Block until this event is reached on whatever stream recorded
    /// it. Returns `Err(DeviceError::NoDriver)` in stub mode if the
    /// event was never recorded.
    #[cfg(not(feature = "cuda"))]
    pub fn synchronize(&self) -> Result<()> {
        let guard = self.inner.recorded_on.lock().map_err(|_| {
            DeviceError::Driver("event recorded_on mutex poisoned".to_string())
        })?;
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
        // PHASE2-CUDA-TODO: cuEventSynchronize via
        // `cudarc::driver::result::event::synchronize`.
        Err(DeviceError::NoDriver)
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
        let guard = self.inner.recorded_on.lock().map_err(|_| {
            DeviceError::Driver("event recorded_on mutex poisoned".to_string())
        })?;
        Ok(guard.is_some())
    }

    #[cfg(feature = "cuda")]
    pub fn query(&self) -> Result<bool> {
        // PHASE2-CUDA-TODO: cuEventQuery via
        // `cudarc::driver::result::event::query`.
        Err(DeviceError::NoDriver)
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
        let mut guard = event.inner.recorded_on.lock().map_err(|_| {
            DeviceError::Driver("event recorded_on mutex poisoned".to_string())
        })?;
        *guard = Some(self.id());
        drop(guard);
        self.record_op(crate::StreamOp::EventRecord {
            event_id: event.id(),
        });
        Ok(())
    }

    #[cfg(feature = "cuda")]
    pub fn record_event(&self, _event: &Event) -> Result<()> {
        // PHASE2-CUDA-TODO: cuEventRecord via
        // `cudarc::driver::result::event::record(event_handle, stream_handle)`.
        Err(DeviceError::NoDriver)
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
    pub fn wait_event(&self, _event: &Event) -> Result<()> {
        // PHASE2-CUDA-TODO: cuStreamWaitEvent via
        // `cudarc::driver::result::stream::wait_event(stream, event, flags)`.
        Err(DeviceError::NoDriver)
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
        let recorded = ev
            .inner
            .recorded_on
            .lock()
            .unwrap()
            .clone();
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
