use super::*;
use crate::test_utils::mock_ctx;
use crate::xnio_conduits::{
    drop_source_channel, Pipe, SRC_FIELD_READ_LISTENER, SRC_FIELD_READ_READY_FLAG,
    SRC_FIELD_READ_SUSPENDED,
};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::sync::atomic::{AtomicI32, AtomicUsize};

static NATIVE_EXECUTE_EXPECTED_RUNNABLE: AtomicUsize = AtomicUsize::new(0);
static NATIVE_EXECUTE_SEEN_RUNNABLE: AtomicUsize = AtomicUsize::new(0);

fn native_execute_runnable_hook(
    _ctx: &mut crate::test_utils::MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    let expected = NATIVE_EXECUTE_EXPECTED_RUNNABLE.load(Ordering::SeqCst);
    if expected == 0 {
        return None;
    }
    let receiver_matches = receiver.as_ptr() as usize == expected;
    let arg_matches = matches!(
        args.first(),
        Some(Value::Object(Some(arg))) if arg.as_ptr() as usize == expected
    );
    if receiver_matches && method_name == "run" && descriptor == "()V" {
        NATIVE_EXECUTE_SEEN_RUNNABLE.fetch_add(1, Ordering::SeqCst);
        return Some(Ok(None));
    }
    if arg_matches && method_name == "execute" && descriptor == "(Ljava/lang/Runnable;)V" {
        NATIVE_EXECUTE_SEEN_RUNNABLE.fetch_add(1, Ordering::SeqCst);
        return Some(Ok(None));
    }
    None
}

#[test]
fn t19_7_b_xnio_get_instance_returns_singleton() {
    let a = Xnio::get_instance();
    let b = Xnio::get_instance();
    let c = Xnio::get_instance_named("nio");
    assert_eq!(a.name, "nio");
    assert!(Arc::ptr_eq(&a, &b));
    assert!(Arc::ptr_eq(&a, &c));
}

#[test]
fn t19_7_b_create_worker_returns_worker_with_default_options() {
    let x = Xnio::get_instance();
    let w = x.create_worker(OptionMap::default());
    assert_eq!(w.io_thread_count(), DEFAULT_IO_THREADS);
    assert_eq!(w.task_thread_count(), DEFAULT_TASK_THREADS);
    w.shutdown_now();
    assert!(w.await_termination(Duration::from_secs(2)));
}

#[test]
fn t19_7_b_worker_io_thread_count_respects_options() {
    // Mid-range value is honoured.
    let w = XnioWorker::new(
        "t19_7_b_sized",
        OptionMap {
            worker_io_threads: Some(7),
            worker_task_core_threads: Some(9),
        },
    );
    assert_eq!(w.io_thread_count(), 7);
    assert_eq!(w.task_thread_count(), 9);
    w.shutdown_now();
    assert!(w.await_termination(Duration::from_secs(2)));

    // Over-clamp gets pinned to the hard max.
    let w2 = XnioWorker::new(
        "t19_7_b_clamp",
        OptionMap {
            worker_io_threads: Some(MAX_IO_THREADS + 100),
            worker_task_core_threads: Some(MAX_TASK_THREADS + 100),
        },
    );
    assert_eq!(w2.io_thread_count(), MAX_IO_THREADS);
    assert_eq!(w2.task_thread_count(), MAX_TASK_THREADS);
    w2.shutdown_now();
    assert!(w2.await_termination(Duration::from_secs(5)));

    // Under-clamp gets pinned to the hard min.
    let w3 = XnioWorker::new(
        "t19_7_b_under",
        OptionMap {
            worker_io_threads: Some(0),
            worker_task_core_threads: Some(0),
        },
    );
    assert_eq!(w3.io_thread_count(), MIN_IO_THREADS);
    assert_eq!(w3.task_thread_count(), MIN_TASK_THREADS);
    w3.shutdown_now();
    assert!(w3.await_termination(Duration::from_secs(2)));
}

#[test]
fn t19_7_b_get_io_thread_round_robin() {
    let w = XnioWorker::new(
        "t19_7_b_rr",
        OptionMap {
            worker_io_threads: Some(4),
            worker_task_core_threads: Some(2),
        },
    );
    let mut ids = Vec::new();
    for _ in 0..12 {
        ids.push(w.get_io_thread().id);
    }
    // 0,1,2,3,0,1,2,3,0,1,2,3 — strict round-robin across 4 threads.
    for i in 0..12 {
        assert_eq!(
            ids[i],
            i % 4,
            "round-robin expected at index {i}: got {ids:?}"
        );
    }
    w.shutdown_now();
    assert!(w.await_termination(Duration::from_secs(2)));
}

#[test]
fn t19_7_b_execute_runs_runnable_on_task_thread() {
    let w = XnioWorker::new(
        "t19_7_b_exec",
        OptionMap {
            worker_io_threads: Some(1),
            worker_task_core_threads: Some(2),
        },
    );
    let counter = Arc::new(AtomicI32::new(0));
    let c1 = counter.clone();
    w.execute(move || {
        c1.fetch_add(1, Ordering::SeqCst);
    })
    .expect("submit");
    // Wait for the task to run. Generous deadline so the test is robust to
    // OS-thread scheduling delays when the suite runs under heavy parallel
    // load (a healthy worker runs the task in milliseconds).
    let deadline = Instant::now() + Duration::from_secs(30);
    while counter.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    w.shutdown_now();
    assert!(w.await_termination(Duration::from_secs(30)));
}

#[test]
fn t19_7_b_native_execute_runs_java_runnable() {
    let w = XnioWorker::new(
        "t19_7_b_native_exec",
        OptionMap {
            worker_io_threads: Some(1),
            worker_task_core_threads: Some(1),
        },
    );
    let mut ctx = mock_ctx();
    let mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &w).unwrap();
    register_worker(w.clone());
    let runnable = ctx.fresh_object_ref();
    NATIVE_EXECUTE_EXPECTED_RUNNABLE.store(runnable.as_ptr() as usize, Ordering::SeqCst);
    NATIVE_EXECUTE_SEEN_RUNNABLE.store(0, Ordering::SeqCst);
    ctx.set_invoke_virtual_hook(native_execute_runnable_hook);

    native_worker_execute(
        &mut ctx,
        &[Value::Object(Some(mirror)), Value::Object(Some(runnable))],
    )
    .expect("native execute should accept runnable");

    assert_eq!(NATIVE_EXECUTE_SEEN_RUNNABLE.load(Ordering::SeqCst), 1);
    NATIVE_EXECUTE_EXPECTED_RUNNABLE.store(0, Ordering::SeqCst);
    w.shutdown_now();
    assert!(w.await_termination(Duration::from_secs(2)));
}

#[test]
fn t19_7_b_execute_after_shutdown_rejects_with_exception() {
    let w = XnioWorker::new("t19_7_b_rej", OptionMap::default());
    w.shutdown();
    let res = w.execute(|| {});
    assert_eq!(res, Err(ExecuteError::Rejected));
    // And through the Java mirror path:
    let mut ctx = mock_ctx();
    let mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &w).unwrap();
    register_worker(w.clone());
    let err = native_worker_execute(&mut ctx, &[Value::Object(Some(mirror))]);
    assert!(err.is_err(), "execute on shutdown worker must fail");
    w.shutdown_now();
    assert!(w.await_termination(Duration::from_secs(2)));
}

#[test]
fn t19_7_b_shutdown_then_await_termination_returns_true() {
    let w = XnioWorker::new(
        "t19_7_b_shut",
        OptionMap {
            worker_io_threads: Some(2),
            worker_task_core_threads: Some(2),
        },
    );
    w.shutdown();
    assert!(w.is_shutdown());
    assert!(w.await_termination(Duration::from_secs(2)));
    assert!(w.is_terminated());
}

#[test]
fn t19_7_b_shutdown_now_interrupts_blocked_tasks() {
    let w = XnioWorker::new(
        "t19_7_b_int",
        OptionMap {
            worker_io_threads: Some(1),
            worker_task_core_threads: Some(2),
        },
    );
    // Submit a long-running task; shutdown_now should drain queued
    // tasks (so the queue is empty) and let the worker loop exit
    // after the current task finishes.  We sleep briefly inside
    // the task so there's a window where it's executing.
    let counter = Arc::new(AtomicI32::new(0));
    for _ in 0..4 {
        let c = counter.clone();
        let _ = w.execute(move || {
            thread::sleep(Duration::from_millis(10));
            c.fetch_add(1, Ordering::SeqCst);
        });
    }
    // Submit a task that will be dropped by shutdown_now (pending
    // in the queue).
    let dropped = Arc::new(AtomicI32::new(0));
    let d = dropped.clone();
    // Fill the queue enough that at least one sits behind the
    // running task.
    for _ in 0..1024 {
        let d2 = d.clone();
        let _ = w.execute(move || {
            d2.fetch_add(1, Ordering::SeqCst);
        });
    }
    w.shutdown_now();
    // Generous deadline: robust to OS-thread scheduling delays under heavy
    // parallel test load (worker shutdown completes near-instantly otherwise).
    assert!(w.await_termination(Duration::from_secs(30)));
    // Not every submitted task must have run — that's the point of
    // shutdown_now.  The queued tasks beyond what in-flight workers
    // had already picked up were cleared.
    let after = dropped.load(Ordering::SeqCst);
    assert!(
        after < 1024,
        "shutdown_now must drop queued tasks; ran {after}/1024"
    );
}

#[test]
fn t19_7_b_is_shutdown_reflects_state_transitions() {
    let w = XnioWorker::new("t19_7_b_state", OptionMap::default());
    assert!(!w.is_shutdown());
    assert!(!w.is_terminated());
    w.shutdown();
    assert!(w.is_shutdown());
    // Not yet terminated until the threads exit.
    assert!(w.await_termination(Duration::from_secs(2)));
    assert!(w.is_terminated());
}

#[test]
fn t19_7_b_worker_drop_joins_all_threads() {
    // Tight loop: create + drop many workers. If Drop leaks any
    // thread the total process thread count would balloon quickly.
    for _ in 0..10 {
        let w = XnioWorker::new(
            "t19_7_b_drop",
            OptionMap {
                worker_io_threads: Some(2),
                worker_task_core_threads: Some(2),
            },
        );
        // Verify we can still interact with it.
        let _ = w.get_io_thread();
        // Drop forces shutdown_now + join.
    }
    // If we reach here without hanging or OOM-ing threads, we pass.
    // Assert one final sanity check: a freshly-built worker still
    // works.
    let w = XnioWorker::new("t19_7_b_drop_final", OptionMap::default());
    let _ = w.get_io_thread();
    drop(w);
}

#[test]
fn t19_7_b_panicking_task_is_caught_and_pool_continues() {
    let w = XnioWorker::new(
        "t19_7_b_panic",
        OptionMap {
            worker_io_threads: Some(1),
            worker_task_core_threads: Some(1),
        },
    );
    let counter = Arc::new(AtomicI32::new(0));
    let c1 = counter.clone();
    w.execute(move || {
        c1.fetch_add(1, Ordering::SeqCst);
        panic!("deliberate test panic");
    })
    .expect("submit");
    // After the panicking task, the worker must still accept new
    // work — catch_unwind must have preserved the pool.
    let c2 = counter.clone();
    let deadline = Instant::now() + Duration::from_secs(2);
    while counter.load(Ordering::SeqCst) < 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    w.execute(move || {
        c2.fetch_add(1, Ordering::SeqCst);
    })
    .expect("submit after panic");
    let deadline = Instant::now() + Duration::from_secs(2);
    while counter.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    w.shutdown_now();
    assert!(w.await_termination(Duration::from_secs(2)));
}

#[test]
fn wf_domain_real_nio_worker_is_adopted_without_slot_handle() {
    let mut ctx = mock_ctx();
    let realish =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_XNIO_WORKER, WORKER_NUM_SLOTS + 1)
            .unwrap();
    ctx.set_field(realish, WORKER_FIELD_OPTIONS_HANDLE, Value::Object(None));

    let worker = read_worker(&ctx, realish).expect("adopt real worker");
    let same_worker = read_worker(&ctx, realish).expect("read adopted worker");

    assert!(Arc::ptr_eq(&worker, &same_worker));
    assert!(matches!(
        ctx.get_field(realish, WORKER_FIELD_OPTIONS_HANDLE),
        Value::Object(None)
    ));
    worker.shutdown_now();
    assert!(worker.await_termination(Duration::from_secs(2)));
}

#[test]
fn t19_7_b_register_natives_registers_all() {
    let mut r = NativeMethodRegistry::new();
    register_xnio_worker_natives(&mut r);
    assert!(r
        .find(CLS_XNIO, "getInstance", "()Lorg/xnio/Xnio;")
        .is_some());
    assert!(r
        .find(
            CLS_XNIO,
            "createWorker",
            "(Lorg/xnio/OptionMap;)Lorg/xnio/XnioWorker;"
        )
        .is_some());
    assert!(r
        .find(CLS_XNIO_WORKER, "getIoThread", "()Lorg/xnio/XnioIoThread;")
        .is_some());
    assert!(r
        .find(CLS_XNIO_WORKER, "chooseThread", "()Lorg/xnio/XnioIoThread;")
        .is_some());
    assert!(r.find(CLS_XNIO_WORKER, "shutdown", "()V").is_some());
    assert!(r
        .find(CLS_XNIO_WORKER, "awaitTermination", "(J)Z")
        .is_some());
    assert!(r.find(CLS_XNIO_WORKER, "isShutdown", "()Z").is_some());
    assert!(r
        .find(
            CLS_NIO_XNIO_WORKER,
            "getIoThread",
            "()Lorg/xnio/XnioIoThread;"
        )
        .is_some());
    assert!(r
        .find(
            CLS_NIO_XNIO_WORKER,
            "chooseThread",
            "()Lorg/xnio/XnioIoThread;"
        )
        .is_some());
    assert!(r
        .find(
            CLS_XNIO_WORKER,
            "createTcpConnectionServer",
            "(Ljava/net/InetSocketAddress;Lorg/xnio/ChannelListener;Lorg/xnio/OptionMap;)Lorg/xnio/channels/AcceptingChannel;"
        )
        .is_some());
    assert!(r
        .find(
            CLS_ACCEPTING_CHANNEL,
            "getAcceptSetter",
            "()Lorg/xnio/ChannelListener$Setter;"
        )
        .is_some());
    assert!(r
        .find(
            "org/xnio/StreamConnection",
            "getWorker",
            "()Lorg/xnio/XnioWorker;"
        )
        .is_some());
}

#[test]
fn wf_domain_create_tcp_connection_server_returns_accepting_channel() {
    let mut ctx = mock_ctx();
    let worker = XnioWorker::new("wf_domain_tcp_server", OptionMap::default());
    register_worker(worker.clone());
    let worker_mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();
    let bind_addr =
        try_alloc_concurrent_synthetic(&mut ctx, "java/net/InetSocketAddress", 2).unwrap();
    let host = ctx.create_string("127.0.0.1");
    ctx.set_field(bind_addr, 0, Value::Object(Some(host)));
    ctx.set_field(bind_addr, 1, Value::Int(0));
    let listener =
        try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/ChannelListener", 0).unwrap();

    let channel = match native_xnio_create_tcp_connection_server(
        &mut ctx,
        &[
            Value::Object(Some(worker_mirror)),
            Value::Object(Some(bind_addr)),
            Value::Object(Some(listener)),
            Value::Object(None),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected accepting channel, got {:?}", other),
    };
    assert_eq!(
        ctx.get_field(channel, ACCEPT_FIELD_LOCAL_ADDRESS),
        Value::Object(Some(bind_addr))
    );
    assert_eq!(
        ctx.get_field(channel, ACCEPT_FIELD_ACCEPT_LISTENER),
        Value::Object(Some(listener))
    );
    assert_eq!(
        native_accepting_is_open(&mut ctx, &[Value::Object(Some(channel))])
            .unwrap()
            .unwrap(),
        Value::Int(1)
    );

    let setter =
        match native_accepting_get_accept_setter(&mut ctx, &[Value::Object(Some(channel))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected setter, got {:?}", other),
        };
    assert_eq!(
        ctx.get_field(setter, SETTER_FIELD_CHANNEL_HANDLE),
        Value::Object(Some(channel))
    );
    assert_eq!(
        ctx.get_field(setter, SETTER_FIELD_LISTENER_SLOT_INDEX),
        Value::Int(ACCEPT_FIELD_ACCEPT_LISTENER as i32)
    );

    native_accepting_close(&mut ctx, &[Value::Object(Some(channel))]).unwrap();
    assert_eq!(ctx.get_field(channel, ACCEPT_FIELD_OPEN), Value::Int(0));
    worker.shutdown_now();
    assert!(worker.await_termination(Duration::from_secs(2)));
}

fn mark_source_read_listener_invoked(
    ctx: &mut crate::test_utils::MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "handleEvent" && descriptor == "(Ljava/nio/channels/Channel;)V" {
        if let Some(Value::Object(Some(source))) = args.first().copied() {
            ctx.set_field(source, SRC_FIELD_READ_READY_FLAG, Value::Int(2));
        }
        return Some(Ok(None));
    }
    None
}

#[test]
fn wf_domain_accept_pump_sleep_polls_registered_sources() {
    let mut ctx = mock_ctx();
    let pipe = Arc::new(Pipe::new(1024));
    pipe.push(b"x");
    let source_id = register_source_channel(ConduitTransport::Pipe(pipe));
    let source = alloc_source_channel_obj(&mut ctx, source_id).unwrap();
    ctx.set_field(source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    let listener =
        try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/ChannelListener", 0).unwrap();
    ctx.set_field(
        source,
        SRC_FIELD_READ_LISTENER,
        Value::Object(Some(listener)),
    );
    ctx.set_invoke_virtual_hook(mark_source_read_listener_invoked);

    // gc-common w23-b: the helper no longer takes (or hands back) the
    // pump's channel; the pump re-reads it from its pin.
    accept_pump_blocked_sleep(&mut ctx, Duration::from_millis(0));

    assert_eq!(
        ctx.get_field(source, SRC_FIELD_READ_READY_FLAG),
        Value::Int(2)
    );
    drop_source_channel(&mut ctx, source_id);
}

#[test]
fn wf_domain_outbound_tcp_stream_starts_source_poller() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
    let addr = listener.local_addr().expect("listener local addr");
    let accepted = Arc::new(AtomicBool::new(false));
    let accepted_for_thread = accepted.clone();
    let accept_thread = thread::spawn(move || {
        if let Ok((_stream, _peer)) = listener.accept() {
            accepted_for_thread.store(true, Ordering::SeqCst);
        }
    });

    fn synthetic_inet_socket_address_getters(
        ctx: &mut crate::test_utils::MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        match (method_name, descriptor) {
            ("getPort", "()I") => Some(Ok(Some(ctx.get_field(receiver, 1)))),
            ("getHostString", "()Ljava/lang/String;") => {
                Some(Ok(Some(ctx.get_field(receiver, 0))))
            }
            _ => None,
        }
    }

    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(synthetic_inet_socket_address_getters);
    let vm = ctx.vm_identity();
    source_poller_started_vms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&vm);

    let worker = XnioWorker::new(
        "wf_domain_outbound_poller",
        OptionMap {
            worker_io_threads: Some(1),
            worker_task_core_threads: Some(1),
        },
    );
    register_worker(worker.clone());
    let worker_mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();
    let io_thread =
        match native_worker_get_io_thread(&mut ctx, &[Value::Object(Some(worker_mirror))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected XnioIoThread mirror, got {:?}", other),
        };
    let destination =
        try_alloc_concurrent_synthetic(&mut ctx, "java/net/InetSocketAddress", 2).unwrap();
    let host = ctx.create_string("127.0.0.1");
    ctx.set_field(destination, 0, Value::Object(Some(host)));
    ctx.set_field(destination, 1, Value::Int(addr.port() as i32));

    let result = native_iot_open_tcp_stream_connection(
        &mut ctx,
        &[
            Value::Object(Some(io_thread)),
            Value::Object(None),
            Value::Object(Some(destination)),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
        ],
    );
    assert!(
        result.is_ok(),
        "openTcpStreamConnection must succeed: {result:?}"
    );
    assert!(source_poller_started_vms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&vm));

    let _ = accept_thread.join();
    assert!(accepted.load(Ordering::SeqCst));
    worker.shutdown_now();
    assert!(worker.await_termination(Duration::from_secs(2)));
}

/// Declare the synthetic `XnioIoThread` layout
/// (`classloading/src/class_manager.rs`: id, workerHandle, selectorHandle,
/// state) for `sample`'s class on the mock, so `set_field_by_name` lands
/// the way it does in the VM. gc-common w29-d: the mirror's fields are now
/// the only record of its worker.
fn declare_synthetic_iot_layout(ctx: &crate::test_utils::MockNativeContext, sample: ObjectRef) {
    let cid = ctx.class_id_of_object(sample);
    let field = |name: &str, desc: &str, slot: usize| cratonvm_native_api::FieldMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        cid,
        vec![
            field("id", "J", 0),
            field("workerHandle", "Lorg/xnio/XnioWorker;", 1),
            field("selectorHandle", "J", 2),
            field("state", "I", 3),
        ],
    );
}

fn get_io_thread_mirror(
    ctx: &mut crate::test_utils::MockNativeContext,
    worker_mirror: ObjectRef,
) -> ObjectRef {
    match native_worker_get_io_thread(ctx, &[Value::Object(Some(worker_mirror))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected XnioIoThread mirror, got {:?}", other),
    }
}

#[test]
fn wf_domain_stream_connection_worker_identity_uses_io_thread_mirror() {
    let mut ctx = mock_ctx();
    let worker = XnioWorker::new("wf_domain_stream_worker_identity", OptionMap::default());
    register_worker(worker.clone());
    let worker_mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();

    // The first mirror only tells us the class the mock allocates it as.
    let sample = get_io_thread_mirror(&mut ctx, worker_mirror);
    assert_ne!(
        ctx.class_id_of_object(sample),
        ctx.class_id_of_object(worker_mirror),
        "premise: the mirror and the worker are distinct mock classes"
    );
    declare_synthetic_iot_layout(&ctx, sample);
    let io_thread = get_io_thread_mirror(&mut ctx, worker_mirror);
    assert_eq!(
        io_thread_worker_mirror(&ctx, io_thread),
        Some(worker_mirror)
    );
    assert!(matches!(
        ctx.get_field_by_name(io_thread, "number"),
        Value::Int(_) | Value::Long(_)
    ));

    worker.shutdown_now();
    assert!(worker.await_termination(Duration::from_secs(2)));
}

/// gc-common w29-d
/// (`common-w28b-xnio-io-thread-mirror-registry-holds-unremapped-worker-refs-FIXED-20260923`):
/// a collection that moves the worker rewrites the mirror's `workerHandle`
/// field; `getWorker()` must answer the NEW address. It used to answer the
/// pre-move address from an unremapped identity-hash registry consulted
/// before the field.
#[test]
fn w29d_io_thread_get_worker_follows_a_moved_worker() {
    let mut ctx = mock_ctx();
    let worker = XnioWorker::new("w29d_moved_worker", OptionMap::default());
    register_worker(worker.clone());
    let worker_mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();
    let sample = get_io_thread_mirror(&mut ctx, worker_mirror);
    declare_synthetic_iot_layout(&ctx, sample);
    let io_thread = get_io_thread_mirror(&mut ctx, worker_mirror);
    assert_eq!(
        io_thread_worker_mirror(&ctx, io_thread),
        Some(worker_mirror)
    );

    // Stand in for the collector: the worker now lives elsewhere and the
    // remap rewrote the mirror's field (slot 1 of the synthetic layout).
    let moved = ctx.fresh_object_ref();
    ctx.set_field_by_name(io_thread, "workerHandle", Value::Object(Some(moved)));
    assert_eq!(io_thread_worker_mirror(&ctx, io_thread), Some(moved));
    assert_eq!(
        crate::xnio_io_thread::iot_worker_mirror(&ctx, io_thread),
        Some(moved),
        "XnioIoThread.getWorker() reads the same field"
    );

    // 10 s, not 2: this test starts the worker's I/O threads (the two
    // `getIoThread` calls), and under the full parallel suite their join
    // overran 2 s about one run in two. Termination is not what it checks.
    worker.shutdown_now();
    assert!(worker.await_termination(Duration::from_secs(10)));
}

#[test]
fn t19_7_b_java_mirror_round_trip_through_registry() {
    // Verify the native surface can build + read a worker mirror,
    // find the backing Arc via the options_handle slot, and call
    // through to isShutdown / shutdown cleanly.
    let mut ctx = mock_ctx();
    let worker = XnioWorker::new("t19_7_b_mirror", OptionMap::default());
    register_worker(worker.clone());
    let mirror = alloc_worker_mirror(&mut ctx, CLS_XNIO_WORKER, &worker).unwrap();
    // isShutdown → false pre-shutdown.
    let r = native_worker_is_shutdown(&mut ctx, &[Value::Object(Some(mirror))])
        .unwrap()
        .unwrap();
    assert_eq!(r, Value::Int(0));
    // Call shutdown through the native, then observe state reflect.
    native_worker_shutdown(&mut ctx, &[Value::Object(Some(mirror))]).unwrap();
    assert_eq!(
        ctx.get_field(mirror, WORKER_FIELD_STATE),
        Value::Int(WORKER_STATE_SHUTDOWN)
    );
    let r2 = native_worker_is_shutdown(&mut ctx, &[Value::Object(Some(mirror))])
        .unwrap()
        .unwrap();
    assert_eq!(r2, Value::Int(1));
    worker.shutdown_now();
    assert!(worker.await_termination(Duration::from_secs(2)));
}

/// gc-common w23-b: a VM's teardown drops its worker-mirror, connection-
/// address, source-poller and accepting-listener rows (closing the
/// listener) and shuts its workers down; another VM's rows and workers
/// are untouched.
#[test]
fn w23b_vm_teardown_drops_worker_rows_and_shuts_its_workers_down() {
    const VM_A: usize = 0x0B23_9A08;
    const VM_B: usize = 0x0B23_9B08;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_xnio_worker_rows(VM_A);
            forget_vm_xnio_worker_rows(VM_B);
        }
    }
    let _forget = Forget;
    let mut a = mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = mock_ctx();
    b.set_vm_identity(VM_B);
    let opts = || OptionMap {
        worker_io_threads: Some(1),
        worker_task_core_threads: Some(1),
    };
    let worker_a = XnioWorker::new("w23b_teardown_a", opts());
    register_worker(worker_a.clone());
    let mirror_a = alloc_worker_mirror(&mut a, CLS_XNIO_WORKER, &worker_a).unwrap();
    let worker_b = XnioWorker::new("w23b_teardown_b", opts());
    register_worker(worker_b.clone());
    let mirror_b = alloc_worker_mirror(&mut b, CLS_XNIO_WORKER, &worker_b).unwrap();
    let conn_a = a.fresh_object_ref();
    remember_stream_connection_addresses(&a, conn_a, None, None);
    let conn_b = b.fresh_object_ref();
    remember_stream_connection_addresses(&b, conn_b, None, None);
    source_poller_started_vms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(VM_A);
    source_poller_started_vms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(VM_B);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
    let listener_a = register_accepting_listener(VM_A, listener);
    assert!(accepting_listener_registered(listener_a));

    forget_vm_xnio_worker_rows(VM_A);

    assert!(lookup_worker(worker_a.id).is_none());
    assert!(worker_a.is_shutdown(), "A's worker is shut down");
    assert!(lookup_worker_object(&a, mirror_a).is_none());
    assert!(stream_connection_addresses(&a, conn_a).is_none());
    let a_poller = source_poller_started_vms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&VM_A);
    assert!(
        !a_poller,
        "a VM reusing A's identity must start its own poller"
    );
    assert!(!accepting_listener_registered(listener_a));

    assert!(lookup_worker_object(&b, mirror_b).is_some());
    assert!(stream_connection_addresses(&b, conn_b).is_some());
    let b_poller = source_poller_started_vms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&VM_B);
    assert!(b_poller);
    assert!(!worker_b.is_shutdown());

    assert!(worker_a.await_termination(Duration::from_secs(2)));
    worker_b.shutdown_now();
    assert!(worker_b.await_termination(Duration::from_secs(2)));
}
