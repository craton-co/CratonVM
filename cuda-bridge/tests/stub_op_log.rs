// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Stub-mode integration tests for the Phase 2 async surfaces.
//!
//! These tests exercise the cross-module surfaces (`Stream`, `Event`,
//! async memcpy, and `launch_on_stream`) against the stub-mode op log.
//!
//! The file is feature-gated with `#![cfg(not(feature = "cuda"))]` so
//! it is only compiled in stub mode — under the real `cuda` feature
//! the op log doesn't exist and these surfaces hit the driver.
//!
//! Run with `cargo test -p cuda-bridge` (the default build). Under
//! `--features cuda` this file is excluded from the build; the
//! corresponding driver-bound behaviour is verified by a separate
//! GPU-required suite (`gpu-it`).
//!
//! Why these tests don't gate on `DeviceContext::probe()`:
//! historically the suite started every test with a `probe()`-or-return
//! prelude that — on the no-GPU dev box — caused every body to
//! early-return without exercising the op log. That made the suite
//! look like an integration suite that executed nothing on default CI.
//! The stub backend is purely an in-memory event recorder: it does not
//! need a real GPU, and its async surfaces (`Stream::new`,
//! `Event::new`, `DeviceBuffer::{from,to}_host_async`,
//! `DeviceModule::{from_ptx,launch_on_stream}`) all return `Ok` in
//! stub mode and record the corresponding `StreamOp` variants. So we
//! drive them directly here via the public
//! [`cuda_bridge::DeviceContext::stub_for_testing`] constructor and
//! assert on the exact recorded sequence.

#![cfg(not(feature = "cuda"))]

use cuda_bridge::{
    DeviceBuffer, DeviceContext, DeviceModule, Event, KernelArgs, LaunchConfig, Stream, StreamOp,
};

/// Build a stub `DeviceContext` for the integration tests.
///
/// Wraps `DeviceContext::stub_for_testing()` — a stub-only public
/// constructor for tests of the op-log surface — in a single helper
/// so the test bodies read naturally.
fn ctx() -> DeviceContext {
    DeviceContext::stub_for_testing()
}

/// Test 1: `DeviceBuffer::from_host_async` records a single
/// `UploadAsync` op with the correct byte count.
#[test]
fn upload_async_records_byte_count() {
    let ctx = ctx();
    let stream = Stream::new(&ctx).expect("Stream::new in stub mode");

    let host = [1.0_f32; 16];
    let _buf = DeviceBuffer::<f32>::from_host_async(&ctx, &host, &stream)
        .expect("from_host_async in stub mode");

    let ops = stream.ops();
    assert_eq!(
        ops.len(),
        1,
        "expected exactly one op recorded, got {:?}",
        ops
    );
    match &ops[0] {
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
    let ctx = ctx();
    let stream = Stream::new(&ctx).expect("Stream::new in stub mode");

    // Stub `from_ptx` returns an inert `DeviceModule` — the stub
    // `launch_on_stream` branch only records a `StreamOp::Launch`
    // and never touches the module, so an inert fixture is enough
    // to exercise the op-log surface here.
    let module = DeviceModule::from_ptx(&ctx, "", &[]).expect("from_ptx in stub mode");

    let cfg = LaunchConfig {
        grid: (10, 1, 1),
        block: (256, 1, 1),
        shared_bytes: 0,
    };
    let args = KernelArgs::new();

    module
        .launch_on_stream(&ctx, "my_kernel", &cfg, args, &stream)
        .expect("launch_on_stream in stub mode");

    let ops = stream.ops();
    assert_eq!(
        ops.as_slice(),
        &[StreamOp::Launch {
            kernel: "my_kernel".into(),
            grid: (10, 1, 1),
            block: (256, 1, 1),
        }],
        "expected a single Launch op for 'my_kernel'"
    );
}

/// Test 3: `Stream::record_event` and `Stream::wait_event` produce
/// matching `EventRecord` / `EventWait` ops with the same event id.
#[test]
fn event_record_and_wait() {
    let ctx = ctx();
    let s1 = Stream::new(&ctx).expect("Stream::new (s1) in stub mode");
    let s2 = Stream::new(&ctx).expect("Stream::new (s2) in stub mode");
    let ev = Event::new(&ctx).expect("Event::new in stub mode");

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
    // And the same id must be the event's own id (the op-log captures
    // exactly that field).
    assert_eq!(
        recorded_id,
        ev.id(),
        "captured event_id must equal Event::id()"
    );
}

/// Test 4: a three-stage pipeline exercising every stream op kind.
///
/// stream1: upload A → launch f(A) → record event
/// stream2: wait event → upload B → launch g(A, B) → download A
///
/// We assert the full op log on each stream is exactly the expected
/// sequence (with `event_id` matched up between the two streams).
#[test]
fn three_stage_pipeline() {
    let ctx = ctx();
    let s1 = Stream::new(&ctx).expect("Stream::new (s1) in stub mode");
    let s2 = Stream::new(&ctx).expect("Stream::new (s2) in stub mode");
    let ev = Event::new(&ctx).expect("Event::new in stub mode");

    // Allocate A and upload on s1.
    let host_a = [1.0_f32; 16]; // 16 × 4 = 64 bytes
    let buf_a = DeviceBuffer::<f32>::from_host_async(&ctx, &host_a, &s1)
        .expect("from_host_async A on s1");

    // Stub `from_ptx` succeeds — we just need a `DeviceModule` so
    // `launch_on_stream` can record `Launch` ops on the streams.
    let module = DeviceModule::from_ptx(&ctx, "", &[]).expect("from_ptx in stub mode");

    // launch f(A) on s1.
    let cfg = LaunchConfig::elementwise(16);
    let args_f = KernelArgs::new().push_device_ptr(&buf_a);
    module
        .launch_on_stream(&ctx, "f", &cfg, args_f, &s1)
        .expect("launch f on s1");

    // record event on s1.
    s1.record_event(&ev).expect("record_event on s1");

    // wait event on s2.
    s2.wait_event(&ev).expect("wait_event on s2");

    // Upload B on s2.
    let host_b = [2.0_f32; 16]; // 64 bytes
    let buf_b = DeviceBuffer::<f32>::from_host_async(&ctx, &host_b, &s2)
        .expect("from_host_async B on s2");

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

    // Pull the event id out of s1's log so we can match s2's EventWait
    // against it.
    let s1_ops = s1.ops();
    let s2_ops = s2.ops();
    let event_id = s1_ops
        .iter()
        .find_map(|op| match op {
            StreamOp::EventRecord { event_id } => Some(*event_id),
            _ => None,
        })
        .expect("s1 must have an EventRecord op");

    // Expected s1 sequence: UploadAsync(64), Launch("f"), EventRecord(id).
    let s1_expected: Vec<StreamOp> = vec![
        StreamOp::UploadAsync { bytes: 64 },
        StreamOp::Launch {
            kernel: "f".into(),
            grid: cfg.grid,
            block: cfg.block,
        },
        StreamOp::EventRecord { event_id },
    ];
    assert_eq!(
        s1_ops.as_slice(),
        s1_expected.as_slice(),
        "s1 op log mismatch"
    );

    // Expected s2 sequence: EventWait(id), UploadAsync(64),
    // Launch("g"), DownloadAsync(64).
    let s2_expected: Vec<StreamOp> = vec![
        StreamOp::EventWait { event_id },
        StreamOp::UploadAsync { bytes: 64 },
        StreamOp::Launch {
            kernel: "g".into(),
            grid: cfg.grid,
            block: cfg.block,
        },
        StreamOp::DownloadAsync { bytes: 64 },
    ];
    assert_eq!(
        s2_ops.as_slice(),
        s2_expected.as_slice(),
        "s2 op log mismatch"
    );
}
