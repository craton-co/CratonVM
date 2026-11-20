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
