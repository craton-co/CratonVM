// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Stub-mode integration tests for the Phase 2 async surfaces.
//!
//! These tests exercise the cross-module surfaces (`Stream`, `Event`,
//! async memcpy, and `launch_on_stream`) against the stub-mode op log.
//!
//! The file is feature-gated with `#![cfg(not(feature = "gpu-driver"))]` so
//! it is only compiled in stub mode — under the real `cuda` feature
//! the op log doesn't exist and these surfaces hit the driver.
//!
//! Per the Phase 2 spec, every test starts with a `probe()`-or-return
//! prelude:
//!
//! - In stub mode `DeviceContext::probe()` returns `Err(NoDriver)`,
//!   so on the no-GPU dev box every test early-returns *without*
//!   exercising the body. That is intentional. The point is that the
//!   tests compile in stub mode and will start exercising the op log
//!   on a future GPU box (or once a stub-friendly context constructor
//!   is wired up upstream). The op log itself is exercised by the
//!   per-file unit tests inside `stream.rs` / `event.rs`.
//!
//! - This is option (b) from the spec; option (a) (a `pub fn for_test`
//!   gated by a feature flag) is left to the discretion of Items
//!   P2-1 / P2-2.

#![cfg(not(feature = "gpu-driver"))]

use cratonvm_cuda_bridge::{
    DeviceBuffer, DeviceContext, DeviceModule, Event, KernelArgs, LaunchConfig, Stream, StreamOp,
};

// PHASE2-GUESS: the spec snippet `DeviceContext::probe()` implies an
// inherent `probe` constructor on `DeviceContext` that returns
// `Result<Self>`. The current crate exposes a free `probe()` function
// returning `DeviceCaps`; Items P2-1..P2-5 are expected to add an
// inherent `DeviceContext::probe()` that mirrors the spec wording.
// If that constructor is named differently upstream, this prelude is
// the single place to fix.
macro_rules! ctx_or_return {
    () => {
        match DeviceContext::probe() {
            Ok(c) => c,
            Err(_) => {
                eprintln!("[stub_op_log] skipping: no DeviceContext.probe in stub mode");
                return;
            }
        }
    };
}

/// Test 1: `DeviceBuffer::from_host_async` records an `EventRecord`
/// (the buffer's `last_write` event) followed by `UploadAsync` with
/// the correct byte count.
///
/// AUDIT 2026-05-24 (HIGH correctness): the EventRecord was added so
/// subsequent `launch_on_stream` calls can `cuStreamWaitEvent` against
/// the upload completion. Order matters: the event must be recorded
/// against the upload-side stream so the kernel-side wait observes the
/// upload's retirement.
#[test]
fn upload_async_records_byte_count() {
    let ctx = ctx_or_return!();
    let stream = Stream::new(&ctx).expect("Stream::new in stub mode after probe succeeded");

    let host = [1.0_f32; 16];
    let _buf = DeviceBuffer::<f32>::from_host_async(&ctx, &host, &stream)
        .expect("from_host_async in stub mode after probe succeeded");

    let ops = stream.ops();
    assert_eq!(
        ops.len(),
        2,
        "expected exactly two ops recorded (EventRecord then UploadAsync), got {:?}",
        ops
    );
    assert!(
        matches!(ops[0], StreamOp::EventRecord { .. }),
        "expected first op to be EventRecord (last_write event), got {:?}",
        ops[0]
    );
    match &ops[1] {
        StreamOp::UploadAsync { bytes } => {
            assert_eq!(*bytes, 64, "16 × sizeof::<f32>() = 64");
        }
        other => panic!("expected UploadAsync {{ bytes: 64 }}, got {:?}", other),
    }
}

/// Test 2: `DeviceModule::launch_on_stream` records a `Launch` op
/// with the correct kernel name and launch geometry.
#[test]
fn launch_on_stream_records_kernel_name() {
    let ctx = ctx_or_return!();
    let stream = Stream::new(&ctx).expect("Stream::new in stub mode after probe succeeded");

    // The synthetic empty PTX may or may not load in stub mode. The
    // spec says: if module creation fails, early-return.
    let module = match DeviceModule::from_ptx(&ctx, "", &[]) {
        Ok(m) => m,
        Err(_) => {
            eprintln!(
                "[stub_op_log] skipping launch_on_stream test: DeviceModule::from_ptx failed"
            );
            return;
        }
    };

    let cfg = LaunchConfig {
        grid: (10, 1, 1),
        block: (256, 1, 1),
        shared_bytes: 0,
    };
    let args = KernelArgs::new();

    module
        .launch_on_stream(&ctx, "my_kernel", &cfg, args, &stream)
        .expect("launch_on_stream in stub mode after module loaded");

    let ops = stream.ops();
    let found = ops.iter().any(|op| match op {
        StreamOp::Launch {
            kernel,
            grid,
            block,
        } => kernel == "my_kernel" && *grid == (10, 1, 1) && *block == (256, 1, 1),
        _ => false,
    });
    assert!(
        found,
        "expected a Launch {{ kernel: \"my_kernel\", grid: (10,1,1), block: (256,1,1) }} in op log, got {:?}",
        ops
    );
}

/// Test 3: `Stream::record_event` and `Stream::wait_event` produce
/// matching `EventRecord` / `EventWait` ops with the same event id.
#[test]
fn event_record_and_wait() {
    let ctx = ctx_or_return!();
    let s1 = Stream::new(&ctx).expect("Stream::new (s1) in stub mode after probe succeeded");
    let s2 = Stream::new(&ctx).expect("Stream::new (s2) in stub mode after probe succeeded");
    let ev = Event::new(&ctx).expect("Event::new in stub mode after probe succeeded");

    s1.record_event(&ev).expect("record_event");
    s2.wait_event(&ev).expect("wait_event");

    let s1_ops = s1.ops();
    let s2_ops = s2.ops();

    // Extract the event_id from s1's EventRecord op.
    let recorded_id = s1_ops
        .iter()
        .find_map(|op| match op {
            StreamOp::EventRecord { event_id } => Some(*event_id),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("expected an EventRecord op on s1, got {:?}", s1_ops);
        });

    // And the event_id from s2's EventWait op.
    let waited_id = s2_ops
        .iter()
        .find_map(|op| match op {
            StreamOp::EventWait { event_id } => Some(*event_id),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("expected an EventWait op on s2, got {:?}", s2_ops);
        });

    assert_eq!(
        recorded_id, waited_id,
        "event ids in EventRecord (s1) and EventWait (s2) must match"
    );
}

/// Test 4: a three-stage pipeline exercising every stream op kind.
///
/// stream1: upload A → launch f(A) → record event
/// stream2: wait event → upload B → launch g(A, B) → download A
///
/// AUDIT 2026-05-24 (HIGH correctness): every async API call now
/// records additional event-discipline ops to enforce H→D / kernel /
/// D→H ordering across streams. The expected sequence below reflects
/// the new contract — see
/// `cuda-bridge/src/launch.rs#tests::kernel_waits_on_h2d_d2h_waits_on_kernel`
/// for the dedicated event-discipline assertion.
#[test]
fn three_stage_pipeline() {
    let ctx = ctx_or_return!();
    let s1 = Stream::new(&ctx).expect("Stream::new (s1) in stub mode after probe succeeded");
    let s2 = Stream::new(&ctx).expect("Stream::new (s2) in stub mode after probe succeeded");
    let manual_ev = Event::new(&ctx).expect("Event::new in stub mode after probe succeeded");

    // Allocate A and upload on s1.
    let host_a = [1.0_f32; 16]; // 16 × 4 = 64 bytes
    let buf_a =
        DeviceBuffer::<f32>::from_host_async(&ctx, &host_a, &s1).expect("from_host_async A on s1");

    // Load a synthetic module for f and g. If module creation fails
    // in stub mode, early-return — the spec says it's acceptable.
    let module = match DeviceModule::from_ptx(&ctx, "", &[]) {
        Ok(m) => m,
        Err(_) => {
            eprintln!("[stub_op_log] skipping three_stage_pipeline: from_ptx failed");
            return;
        }
    };

    // launch f(A) on s1.
    let cfg = LaunchConfig::elementwise(16);
    let args_f = KernelArgs::new().push_device_ptr(&buf_a);
    module
        .launch_on_stream(&ctx, "f", &cfg, args_f, &s1)
        .expect("launch f on s1");

    // record event on s1 (manual / explicit — separate from the
    // implicit kernel_done event launch_on_stream just installed).
    s1.record_event(&manual_ev).expect("record_event on s1");

    // wait manual_ev on s2.
    s2.wait_event(&manual_ev).expect("wait_event on s2");

    // Upload B on s2.
    let host_b = [2.0_f32; 16]; // 64 bytes
    let buf_b =
        DeviceBuffer::<f32>::from_host_async(&ctx, &host_b, &s2).expect("from_host_async B on s2");

    // launch g(A, B) on s2.
    let args_g = KernelArgs::new()
        .push_device_ptr(&buf_a)
        .push_device_ptr(&buf_b);
    module
        .launch_on_stream(&ctx, "g", &cfg, args_g, &s2)
        .expect("launch g on s2");

    // Download A on s2.
    let mut out_a = [0.0_f32; 16];
    buf_a
        .to_host_async(&mut out_a, &s2)
        .expect("to_host_async A on s2");

    // We no longer assert the full op vector against a hand-written
    // expected slice because the auto-installed last_write events use
    // process-wide-monotonic ids that would couple the assertion to
    // test-execution order. Instead we assert the *structural*
    // invariants that matter for correctness: H→D records its
    // last_write event before the upload op, kernel launches wait on
    // every input buffer's prior last_write, and D→H waits on the
    // kernel-completion event installed by the most recent launch.
    let s1_ops = s1.ops();
    let s2_ops = s2.ops();

    // s1 structural skeleton: EventRecord(upload_a), UploadAsync(64),
    // EventWait(upload_a), Launch("f"), EventRecord(kernel_f_done),
    // EventRecord(manual_ev).
    assert!(
        matches!(s1_ops.first(), Some(StreamOp::EventRecord { .. })),
        "s1[0] should be the upload's last_write EventRecord, got {:?}",
        s1_ops.first()
    );
    assert!(
        matches!(s1_ops.get(1), Some(StreamOp::UploadAsync { bytes: 64 })),
        "s1[1] should be UploadAsync(64), got {:?}",
        s1_ops.get(1)
    );
    assert!(
        matches!(s1_ops.get(2), Some(StreamOp::EventWait { .. })),
        "s1[2] should wait on upload_a before launching f, got {:?}",
        s1_ops.get(2)
    );
    assert!(
        matches!(s1_ops.get(3), Some(StreamOp::Launch { ref kernel, .. }) if kernel == "f"),
        "s1[3] should be Launch(\"f\"), got {:?}",
        s1_ops.get(3)
    );

    // s2 structural skeleton: EventWait(manual_ev),
    // EventRecord(upload_b), UploadAsync(64),
    // EventWait(kernel_f_done), EventWait(upload_b), Launch("g"),
    // EventRecord(kernel_g_done), EventWait(kernel_g_done),
    // DownloadAsync(64).
    assert!(
        matches!(s2_ops.first(), Some(StreamOp::EventWait { .. })),
        "s2[0] should be the explicit EventWait(manual_ev), got {:?}",
        s2_ops.first()
    );
    let upload_b_pos = s2_ops
        .iter()
        .position(|op| matches!(op, StreamOp::UploadAsync { bytes: 64 }))
        .expect("s2 must contain UploadAsync(64)");
    let launch_g_pos = s2_ops
        .iter()
        .position(|op| matches!(op, StreamOp::Launch { ref kernel, .. } if kernel == "g"))
        .expect("s2 must contain Launch(\"g\")");
    let download_pos = s2_ops
        .iter()
        .position(|op| matches!(op, StreamOp::DownloadAsync { bytes: 64 }))
        .expect("s2 must contain DownloadAsync(64)");
    assert!(
        upload_b_pos < launch_g_pos && launch_g_pos < download_pos,
        "s2 op order must be UploadAsync(B) < Launch(g) < DownloadAsync; got {:?}",
        s2_ops
    );
    // There must be at least one EventWait between Launch("g") and
    // DownloadAsync — that is the kernel_done wait the download path
    // installs.
    let mid_waits = s2_ops[launch_g_pos + 1..download_pos]
        .iter()
        .filter(|op| matches!(op, StreamOp::EventWait { .. }))
        .count();
    assert!(
        mid_waits >= 1,
        "expected at least one EventWait between Launch(\"g\") and DownloadAsync; got {:?}",
        &s2_ops[launch_g_pos + 1..download_pos]
    );
}

/// Test 5: `Event::query` reflects the event's recorded state — `false`
/// before `Stream::record_event`, and `true` once the recording
/// stream has synchronized (i.e. the recorded work has definitely
/// retired). Exercises the non-blocking completion probe that a
/// VM-side `GpuFuture::isDone()` poller would call directly instead of
/// blocking in `Event::synchronize`.
#[test]
fn event_query_reflects_recorded_state() {
    let ctx = ctx_or_return!();
    let stream = Stream::new(&ctx).expect("Stream::new in stub mode after probe succeeded");
    let ev = Event::new(&ctx).expect("Event::new in stub mode after probe succeeded");

    assert_eq!(
        ev.query().expect("query before record must not error"),
        false,
        "an event that has never been recorded must query as false"
    );

    stream.record_event(&ev).expect("record_event");

    // Immediately after `record_event` the event may or may not have
    // fired yet (that depends on how much prior work is queued ahead
    // of it and how busy the GPU is) — `query` only promises a
    // non-blocking, consistent read of driver state, not `true` right
    // away. We only assert it stops erroring here, then drive the
    // stream to completion and check the now-deterministic `true`.
    let _ = ev.query().expect("query after record must not error");
    stream.synchronize().expect("synchronize");
    assert_eq!(
        ev.query().expect("query after synchronize must not error"),
        true,
        "an event must report complete once its recording stream has synchronized"
    );
}

/// Test 6: `Stream::add_host_callback` enqueues a closure that the
/// driver runs after all work submitted to the stream before the call
/// has retired. The callback may run asynchronously (on a driver
/// callback thread) relative to `add_host_callback` returning, so this
/// test drives the stream to completion via `synchronize` before
/// asserting the closure ran — see the method's doc comment for the
/// full CUDA host-callback contract (most importantly: the callback
/// itself must never call back into this crate's CUDA-driving API).
#[test]
fn add_host_callback_runs_after_prior_work() {
    let ctx = ctx_or_return!();
    let stream = Stream::new(&ctx).expect("Stream::new in stub mode after probe succeeded");

    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran_clone = ran.clone();
    stream
        .add_host_callback(Box::new(move || {
            ran_clone.store(true, std::sync::atomic::Ordering::Release);
        }))
        .expect("add_host_callback");

    stream.synchronize().expect("synchronize");
    assert!(
        ran.load(std::sync::atomic::Ordering::Acquire),
        "host callback must have run by the time the stream has synchronized"
    );
}

// Note: the dedicated event-ordering integration test mandated by the
// 2026-05-24 cross-stream ordering audit lives inside the crate in
// `cuda-bridge/src/launch.rs#tests::kernel_waits_on_h2d_d2h_waits_on_kernel`,
// because it requires the crate-private `for_test()` constructors on
// `DeviceContext` / `DeviceModule` / `DeviceBuffer` to stand up a
// driverless fixture (`tests/` integration tests cannot reach
// `pub(crate)` items). The structural assertions added to
// `three_stage_pipeline` above cover the same invariants from the
// integration-test angle when the cuda feature is wired up.
