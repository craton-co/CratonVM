use super::*;

#[test]
fn stream_new_assigns_unique_ids() {
    // We bypass `Stream::new(&DeviceContext)` because
    // `DeviceContext::new` returns `NoDriver` in stub mode. The
    // `for_test` constructor exercises the same id-allocation
    // path, which is what we actually care about.
    let a = Stream::for_test();
    let b = Stream::for_test();
    assert_ne!(a.id(), b.id(), "ids must be unique per construction");
}

#[test]
fn record_op_appends_to_log() {
    let s = Stream::for_test();
    s.record_op(StreamOp::UploadAsync { bytes: 128 });
    s.record_op(StreamOp::Launch {
        kernel: "add".to_string(),
        grid: (4, 1, 1),
        block: (32, 1, 1),
    });
    s.record_op(StreamOp::DownloadAsync { bytes: 128 });
    let ops = s.ops();
    assert_eq!(ops.len(), 3);
    assert_eq!(ops[0], StreamOp::UploadAsync { bytes: 128 });
    assert!(matches!(
        ops[1],
        StreamOp::Launch { ref kernel, grid: (4, 1, 1), block: (32, 1, 1) } if kernel == "add"
    ));
    assert_eq!(ops[2], StreamOp::DownloadAsync { bytes: 128 });
}

#[test]
fn synchronize_records_op() {
    let s = Stream::for_test();
    s.synchronize().expect("stub synchronize must be Ok");
    let ops = s.ops();
    assert_eq!(ops, vec![StreamOp::Synchronize]);
}

#[test]
fn ops_returns_clone_not_drain() {
    let s = Stream::for_test();
    s.record_op(StreamOp::EventRecord { event_id: 7 });
    s.record_op(StreamOp::EventWait { event_id: 7 });
    let snapshot_a = s.ops();
    let snapshot_b = s.ops();
    // Calling ops() twice must yield the same history — i.e. it
    // does not drain the underlying log.
    assert_eq!(snapshot_a, snapshot_b);
    assert_eq!(snapshot_a.len(), 2);
    // And further recording must still work afterward.
    s.record_op(StreamOp::Synchronize);
    assert_eq!(s.ops().len(), 3);
}

#[test]
fn add_host_callback_runs_synchronously_in_stub_mode() {
    // Stub-mode contract: "stream is always drained" — the closure
    // must have already run by the time `add_host_callback`
    // returns, with no thread hop and no polling required.
    let s = Stream::for_test();
    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran_clone = ran.clone();
    s.add_host_callback(Box::new(move || {
        ran_clone.store(true, Ordering::Relaxed);
    }))
    .expect("add_host_callback must succeed in stub mode");
    assert!(
        ran.load(Ordering::Relaxed),
        "callback must have run by the time add_host_callback returns in stub mode"
    );
}

#[test]
fn add_host_callback_records_op() {
    let s = Stream::for_test();
    s.add_host_callback(Box::new(|| {}))
        .expect("add_host_callback");
    let ops = s.ops();
    assert_eq!(
        ops,
        vec![StreamOp::HostCallback],
        "stub op log should contain exactly one HostCallback"
    );
}

#[test]
fn add_host_callback_runs_exactly_once() {
    // Guard against a trampoline/box-reclamation bug that would
    // either drop the closure without calling it or call it twice.
    let s = Stream::for_test();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let count_clone = count.clone();
    s.add_host_callback(Box::new(move || {
        count_clone.fetch_add(1, Ordering::Relaxed);
    }))
    .expect("add_host_callback");
    assert_eq!(count.load(Ordering::Relaxed), 1);
}

#[test]
fn add_host_callback_interleaves_with_other_ops_in_log_order() {
    // The op log should reflect submission order across mixed op
    // kinds, the same way EventRecord/EventWait do today.
    let s = Stream::for_test();
    s.record_op(StreamOp::UploadAsync { bytes: 64 });
    s.add_host_callback(Box::new(|| {}))
        .expect("add_host_callback");
    s.record_op(StreamOp::DownloadAsync { bytes: 64 });
    assert_eq!(
        s.ops(),
        vec![
            StreamOp::UploadAsync { bytes: 64 },
            StreamOp::HostCallback,
            StreamOp::DownloadAsync { bytes: 64 },
        ]
    );
}
