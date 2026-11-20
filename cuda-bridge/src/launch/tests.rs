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
        StreamOp::Launch {
            kernel,
            grid,
            block,
        } => kernel == "my_kernel" && *grid == cfg.grid && *block == cfg.block,
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
    let upload_event =
        std::sync::Arc::new(crate::Event::new(&ctx).expect("Event::new in stub mode"));
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
    let kernel_waits_on_upload = ops[..launch_pos].iter().any(
        |op| matches!(op, StreamOp::EventWait { event_id } if *event_id == upload_event_id),
    );
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
    let d2h_waits_on_kernel = ops[launch_pos + 1..download_pos].iter().any(
        |op| matches!(op, StreamOp::EventWait { event_id } if *event_id == kernel_event_id),
    );
    assert!(
        d2h_waits_on_kernel,
        "D→H must wait on kernel_done event before downloading; got {:?}",
        ops
    );
}
