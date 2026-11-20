use super::*;
use crate::test_support::{confine_test_lock, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// No private slot in this module may be indexed RAW — every access goes
/// through `aio_get`/`aio_set`/`aio_has`, which add `aio_base`.
///
/// This is a source guard rather than a behavioural one because the defect
/// it catches is invisible on the receiver shape the unit tests build. A
/// synthetic 3-field carrier has `base == 0`, so `ctx.get_field(this, 2)`
/// and `aio_get(ctx, this, F_REG_ID)` are the SAME slot and every test
/// passes either way. They diverge only on a concrete receiver — a real JDK
/// `AsynchronousSocketChannel` implementation object, which carries the
/// JDK's own fields first and this module's private slots after them.
///
/// That is not hypothetical. `aio_asc_read` indexed slot 2 directly, and on
/// a concrete channel (`CRATONVM_DBG_AIO=1` reports `this_fields=52`) it
/// read an unrelated real field and failed `read: bad fd for tcp clone`.
/// Tomcat's `WsFrameClient` reads that as a dropped connection and closes
/// the session immediately after `onOpen` — the 19-class WebSocket cluster,
/// from one missing `aio_base`.
#[test]
fn every_private_slot_access_goes_through_the_base_aware_accessor() {
    let src = include_str!("../async_socket.rs");
    // Split so this test's OWN lines do not contain the pattern it hunts —
    // a self-matching guard reports itself and nothing else.
    let getter = concat!("ctx.get_", "field(this, ");
    let setter = concat!("ctx.set_", "field(this, ");
    let offenders: Vec<(usize, &str)> = src
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            // Skip comments, which quote the bad form deliberately.
            !t.starts_with("//")
                && (t.contains(getter) || t.contains(setter))
                && !t.contains("F_")
        })
        .map(|(i, l)| (i + 1, l.trim()))
        .collect();
    assert!(
        offenders.is_empty(),
        "private slot(s) indexed without `aio_base` — on a CONCRETE receiver \
         these do not address the slot they name, they address whatever real \
         JDK field happens to sit at that index. Use `aio_get`/`aio_set`. \
         Offenders: {offenders:?}"
    );
}

/// AUDIT 2026-07-26 (native-io-audit): the handler form of
/// `AsynchronousSocketChannel.write` reported a bare `IntCount` and never
/// advanced the source `ByteBuffer` — the same defect already fixed for
/// the Future form (see `FutureOutcome::Count`). A conforming caller
/// (`while (buf.hasRemaining()) channel.write(buf, att, handler)`,
/// re-armed from `completed()`) therefore resubmitted the identical slice
/// forever, physically resending the frame on the wire.
#[test]
fn audit_handler_write_completion_advances_source_buffer() {
    let _g = confine_test_lock().lock();
    // Each mock is its own VM, so its completion state starts empty.
    let mut ctx = MockNativeContext::new();

    let bb = ctx.alloc_object(8);
    ctx.set_field_by_name(bb, "position", Value::Int(5));
    let buffer_gref = ctx.add_global_root(bb);
    let handler = ctx.alloc_object(1);
    let roots = HandlerRoots::new(&mut ctx, Some(handler), None);

    push_handler_completion(
        &aio_sink(ctx.vm_identity()),
        Completion {
            roots,
            outcome: Ok(CompletionKind::WriteCount { n: 7, buffer_gref }),
        },
    );
    drain_completions(&mut ctx);

    assert_eq!(
        ctx.get_field_by_name(bb, "position"),
        Value::Int(12),
        "position must advance by the bytes actually written"
    );
    assert_eq!(
        ctx.global_root_count(),
        0,
        "the buffer AND handler global roots must be released after delivery"
    );
}

/// The worker-parked release path (handler-less write, and every error
/// exit) must also drop the root — otherwise every async write leaks one.
#[test]
fn audit_parked_root_release_advances_then_frees() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();
    let sink = aio_sink(ctx.vm_identity());

    let advanced = ctx.alloc_object(8);
    ctx.set_field_by_name(advanced, "position", Value::Int(2));
    let g1 = ctx.add_global_root(advanced);
    queue_write_advance(&sink, g1, 3);

    let untouched = ctx.alloc_object(8);
    ctx.set_field_by_name(untouched, "position", Value::Int(9));
    let g2 = ctx.add_global_root(untouched);
    queue_root_release(&sink, g2);

    assert_eq!(ctx.global_root_count(), 2);
    flush_pending_root_releases(&mut ctx);

    assert_eq!(ctx.get_field_by_name(advanced, "position"), Value::Int(5));
    assert_eq!(
        ctx.get_field_by_name(untouched, "position"),
        Value::Int(9),
        "a failed write must not advance the buffer"
    );
    assert_eq!(ctx.global_root_count(), 0, "both roots must be freed");
}

// -----------------------------------------------------------------
// Root audit 2026-08-01 — handler/attachment/channel remapping.
//
// Before this wave, `Completion` carried the `CompletionHandler` and its
// attachment as bare `ObjectRef`s captured on a worker thread *before* a
// blocking syscall. They were in no root set, so a moving collection was
// free to relocate them and nothing repointed the copies. Each test below
// relocates the root between park and drain — the mock's
// `relocate_global_root` models exactly what the collector's remap pass
// does — and asserts the delivered reference is the POST-move one. Against
// the old bare-`ObjectRef` shape the assertion fails with the pre-move
// address, which is the dangling read this fixes.
// -----------------------------------------------------------------

/// `completed()` must be dispatched on the handler's post-relocation
/// address, and both roots must be released exactly once.
#[test]
fn audit_completion_handler_is_remapped_before_delivery() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();

    let handler_before = ctx.alloc_object(1);
    let attachment_before = ctx.alloc_object(1);
    let roots = HandlerRoots::new(&mut ctx, Some(handler_before), Some(attachment_before));
    push_handler_completion(
        &aio_sink(ctx.vm_identity()),
        Completion {
            roots,
            outcome: Ok(CompletionKind::Void),
        },
    );

    // A moving collection runs while the completion sits in the queue.
    let handler_after = ctx.alloc_object(1);
    let attachment_after = ctx.alloc_object(1);
    ctx.relocate_global_root(roots.handler, handler_after);
    ctx.relocate_global_root(roots.attachment, attachment_after);
    assert_ne!(handler_before, handler_after);

    drain_completions(&mut ctx);

    let delivered = ctx
        .recorded_calls()
        .iter()
        .find(|c| c.method_name == "completed")
        .expect("completed must be dispatched")
        .clone();
    assert_eq!(
        delivered.args[0],
        Value::Object(Some(handler_after)),
        "the handler must be resolved through its global root, not from \
         the pre-move ObjectRef the worker captured"
    );
    assert_eq!(
        delivered.args[2],
        Value::Object(Some(attachment_after)),
        "the attachment must be remapped too"
    );
    assert_eq!(
        ctx.global_root_count(),
        0,
        "handler and attachment roots must both be released after delivery"
    );
}

/// The `failed()` path takes a different branch (it allocates an
/// IOException and a message String first, either of which can move the
/// handler again) and must remap just the same.
#[test]
fn audit_failed_delivery_is_remapped_and_releases_roots() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();

    let handler_before = ctx.alloc_object(1);
    let roots = HandlerRoots::new(&mut ctx, Some(handler_before), None);
    push_handler_completion(
        &aio_sink(ctx.vm_identity()),
        Completion {
            roots,
            outcome: Err("connect failed: refused".to_string()),
        },
    );

    let handler_after = ctx.alloc_object(1);
    ctx.relocate_global_root(roots.handler, handler_after);

    drain_completions(&mut ctx);

    let delivered = ctx
        .recorded_calls()
        .iter()
        .find(|c| c.method_name == "failed")
        .expect("failed must be dispatched")
        .clone();
    assert_eq!(
        delivered.args[0],
        Value::Object(Some(handler_after)),
        "failed() must dispatch on the post-move handler"
    );
    // The Throwable itself matters: the arm that builds it is also the arm
    // that can bail out and deliver nothing, so assert what arrived rather
    // than only that something did.
    let Value::Object(Some(thrown)) = delivered.args[1] else {
        panic!("failed() was passed no Throwable: {:?}", delivered.args[1]);
    };
    assert_eq!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(thrown))
            .as_deref(),
        Some("java/io/IOException"),
        "failed() must be handed a java.io.IOException"
    );
    let Value::Object(Some(message)) = ctx.get_field_by_name(thrown, "detailMessage") else {
        panic!("the IOException carries no detailMessage");
    };
    assert_eq!(
        ctx.read_string(message).as_deref(),
        Some("connect failed: refused"),
        "the worker's failure message must reach the handler"
    );
    assert_eq!(
        ctx.global_root_count(),
        0,
        "the handler root must be released even on the failure path"
    );
}

/// A handler-less job still roots its attachment, so the release path must
/// drop both handles or every such op leaks one root for the life of the VM.
#[test]
fn audit_handler_less_job_releases_its_attachment_root() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();

    let attachment = ctx.alloc_object(1);
    let roots = HandlerRoots::new(&mut ctx, None, Some(attachment));
    assert!(!roots.has_handler());
    assert_eq!(ctx.global_root_count(), 1);

    queue_handler_release(&aio_sink(ctx.vm_identity()), roots);
    flush_pending_root_releases(&mut ctx);
    assert_eq!(
        ctx.global_root_count(),
        0,
        "a null CompletionHandler must not strand the attachment's root"
    );
}

/// `PendingFieldReset` writes `F_CONNECTED = 0` on a *different* thread
/// from the one that parked it. It must resolve the channel through its
/// root: writing through the pre-move `ObjectRef` clears the flag on
/// whatever object now occupies that address.
#[test]
fn audit_pending_field_reset_targets_the_relocated_channel() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();
    let state = aio_vm(ctx.vm_identity());

    let channel_before = ctx.alloc_object(N_FIELDS);
    ctx.set_field(channel_before, F_CONNECTED, Value::Int(1));
    let channel_gref = ctx.add_global_root(channel_before);

    state.field_resets.lock().push(PendingFieldReset {
        target_gref: channel_gref,
        field: F_CONNECTED,
        value: Value::Int(0),
        release_root: true,
    });

    // The collector moves the channel while the reset is parked. The
    // old address is now a different live object.
    let channel_after = ctx.alloc_object(N_FIELDS);
    ctx.set_field(channel_after, F_CONNECTED, Value::Int(1));
    ctx.relocate_global_root(channel_gref, channel_after);

    flush_pending_field_resets(&mut ctx);

    assert_eq!(
        ctx.get_field(channel_after, F_CONNECTED),
        Value::Int(0),
        "the reset must land on the channel's post-move address"
    );
    assert_eq!(
        ctx.get_field(channel_before, F_CONNECTED),
        Value::Int(1),
        "and must NOT be written through the stale pre-move reference"
    );
    assert_eq!(
        ctx.global_root_count(),
        0,
        "the channel root is released by the last reset for that handle"
    );
}

/// The connect-failure path parks TWO resets against one channel root.
/// Only the last carries `release_root`, so the first must still find the
/// root live — a premature release would silently drop the second write.
#[test]
fn audit_two_field_resets_share_one_channel_root() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();
    let state = aio_vm(ctx.vm_identity());

    let channel = ctx.alloc_object(N_FIELDS);
    ctx.set_field(channel, F_CONNECTED, Value::Int(1));
    ctx.set_field(channel, F_REG_ID, Value::Int(7));
    let channel_gref = ctx.add_global_root(channel);

    {
        let mut resets = state.field_resets.lock();
        resets.push(PendingFieldReset {
            target_gref: channel_gref,
            field: F_CONNECTED,
            value: Value::Int(0),
            release_root: false,
        });
        resets.push(PendingFieldReset {
            target_gref: channel_gref,
            field: F_REG_ID,
            value: Value::Int(-1),
            release_root: true,
        });
    }
    flush_pending_field_resets(&mut ctx);

    assert_eq!(ctx.get_field(channel, F_CONNECTED), Value::Int(0));
    assert_eq!(
        ctx.get_field(channel, F_REG_ID),
        Value::Int(-1),
        "the second reset must still resolve — the root outlives it"
    );
    assert_eq!(ctx.global_root_count(), 0);
}

/// A zero handle is the "no root" sentinel and must never be queued.
#[test]
fn audit_zero_gref_is_not_queued() {
    let _g = confine_test_lock().lock();
    let ctx = MockNativeContext::new();
    let sink = aio_sink(ctx.vm_identity());
    queue_root_release(&sink, 0);
    queue_write_advance(&sink, 0, 4);
    assert!(sink.as_ref().unwrap().root_releases.lock().is_empty());
    forget_vm_aio_state(ctx.vm_identity());
}

#[test]
fn registers_without_panic() {
    let mut r = NativeMethodRegistry::new();
    register_async_socket_real(&mut r);
    assert!(r
        .find(
            "java/nio/channels/AsynchronousSocketChannel",
            "open",
            "()Ljava/nio/channels/AsynchronousSocketChannel;"
        )
        .is_some());
    assert!(r
        .find(
            "java/nio/channels/AsynchronousServerSocketChannel",
            "bind",
            "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;"
        )
        .is_some());
    assert!(r
        .find(
            "java/nio/channels/AsynchronousChannelGroup",
            "withFixedThreadPool",
            "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"
        )
        .is_some());
    assert!(r
        .find(
            "java/nio/channels/AsynchronousSocketChannel",
            "read",
            "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"
        )
        .is_some());
    assert!(r
        .find(
            "java/nio/channels/AsynchronousSocketChannel",
            "write",
            "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"
        )
        .is_some());
}

#[test]
fn worker_pool_runs_real_connect() {
    // Spawn a server, kick a Connect job at the pool, watch the
    // completion appear in the queue.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let _t = std::thread::spawn(move || {
        // Accept once and immediately close.
        let _ = listener.accept();
    });

    // The job's VM must have a live state row, or (since gc-common w11-b)
    // the worker treats the VM as disposed and closes the socket instead
    // of registering it; `submit_job` creates the row.
    let ctx = MockNativeContext::new();
    let vm = ctx.vm_identity();
    let id = aio_register(vm, AioHandle::Pending);
    let _ = submit_job(
        vm,
        Job::Connect {
            id,
            addr: format!("127.0.0.1:{port}"),
            roots: HandlerRoots::default(),
            channel_gref: 0,
        },
    );

    // Wait for the registry to flip to Stream.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        {
            let map = aio_registry().read();
            if matches!(map.get(&id), Some(AioHandle::Stream(_))) {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("connect didn't complete in time");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Cleanup.
    aio_remove(id);
    forget_vm_aio_state(vm);
}

// -----------------------------------------------------------------
// gc-common w11-b (`common-w10c-aio-dispatcher-is-one-per-process`):
// completions are queued, waited for, delivered and forgotten per VM.
// Every mock is its own VM (`vm_identity`) with its own global-root
// table, which is exactly the property the defect broke: a completion
// resolved in the wrong VM's table.
// -----------------------------------------------------------------

/// Two VMs' handler completions land in two queues; draining one VM
/// delivers only its own, through its own global-root table, and leaves
/// the other's queued with its roots still held.
#[test]
fn w11b_completions_are_queued_and_delivered_per_vm() {
    let _g = confine_test_lock().lock();
    let mut ctx_a = MockNativeContext::new();
    let mut ctx_b = MockNativeContext::new();
    let (vm_a, vm_b) = (ctx_a.vm_identity(), ctx_b.vm_identity());
    assert_ne!(vm_a, vm_b);

    let handler_a = ctx_a.alloc_object(1);
    let roots_a = HandlerRoots::new(&mut ctx_a, Some(handler_a), None);
    push_handler_completion(
        &aio_sink(vm_a),
        Completion {
            roots: roots_a,
            outcome: Ok(CompletionKind::Void),
        },
    );
    let handler_b = ctx_b.alloc_object(1);
    let roots_b = HandlerRoots::new(&mut ctx_b, Some(handler_b), None);
    push_handler_completion(
        &aio_sink(vm_b),
        Completion {
            roots: roots_b,
            outcome: Ok(CompletionKind::Void),
        },
    );
    assert!(wait_for_pending(vm_a, Duration::ZERO));
    assert!(wait_for_pending(vm_b, Duration::ZERO));

    drain_completions(&mut ctx_a);
    assert!(
        ctx_a.recorded_calls().iter().any(
            |c| c.method_name == "completed" && c.args[0] == Value::Object(Some(handler_a))
        ),
        "VM A's completion must be delivered on VM A's context"
    );
    assert_eq!(
        ctx_a.global_root_count(),
        0,
        "A's roots released in A's table"
    );
    assert!(
        !ctx_b
            .recorded_calls()
            .iter()
            .any(|c| c.method_name == "completed"),
        "draining A must not deliver B's completion"
    );
    assert_eq!(
        ctx_b.global_root_count(),
        1,
        "B's handler root is untouched"
    );
    assert!(
        !wait_for_pending(vm_a, Duration::ZERO),
        "A's queue is empty"
    );
    assert!(wait_for_pending(vm_b, Duration::ZERO), "B's is not");

    drain_completions(&mut ctx_b);
    assert!(ctx_b
        .recorded_calls()
        .iter()
        .any(|c| c.method_name == "completed" && c.args[0] == Value::Object(Some(handler_b))));
    assert_eq!(ctx_b.global_root_count(), 0);

    forget_vm_aio_state(vm_a);
    forget_vm_aio_state(vm_b);
}

/// A worker job carries its VM: a real loopback connect submitted for VM
/// A completes into A's queue, never into B's.
#[test]
fn w11b_worker_completion_lands_in_the_submitting_vms_queue() {
    let _g = confine_test_lock().lock();
    let mut ctx_a = MockNativeContext::new();
    let ctx_b = MockNativeContext::new();
    let (vm_a, vm_b) = (ctx_a.vm_identity(), ctx_b.vm_identity());
    let _ = aio_vm(vm_b);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let _ = listener.accept();
    });
    let handler = ctx_a.alloc_object(1);
    let roots = HandlerRoots::new(&mut ctx_a, Some(handler), None);
    let id = aio_register(vm_a, AioHandle::Pending);
    assert!(submit_job(
        vm_a,
        Job::Connect {
            id,
            addr: format!("127.0.0.1:{port}"),
            roots,
            channel_gref: 0,
        },
    )
    .is_ok());

    // Bounded: at most 5 s, in 100 ms waits.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !wait_for_pending(vm_a, Duration::from_millis(100)) {
        assert!(
            std::time::Instant::now() < deadline,
            "the connect completion never reached VM A's queue"
        );
    }
    assert!(
        !wait_for_pending(vm_b, Duration::ZERO),
        "VM A's completion must not appear in VM B's queue"
    );
    drain_completions(&mut ctx_a);
    assert!(ctx_a
        .recorded_calls()
        .iter()
        .any(|c| c.method_name == "completed"));
    assert_eq!(ctx_a.global_root_count(), 0);

    let _ = server.join();
    aio_remove(id);
    forget_vm_aio_state(vm_a);
    forget_vm_aio_state(vm_b);
}

/// Disposing a VM stops its dispatcher loop and drops what it had queued,
/// and a completion a worker finishes afterwards is dropped rather than
/// resurrecting the VM's row. Another VM is unaffected.
#[test]
fn w11b_a_disposed_vm_stops_its_dispatcher_and_drops_late_completions() {
    let _g = confine_test_lock().lock();
    let mut ctx_a = MockNativeContext::new();
    let ctx_b = MockNativeContext::new();
    let (vm_a, vm_b) = (ctx_a.vm_identity(), ctx_b.vm_identity());
    let _ = aio_vm(vm_b);
    assert!(
        !aio_dispatcher_should_run(0xDEAD_0000),
        "no state, no dispatcher"
    );

    let handler = ctx_a.alloc_object(1);
    let hg = ctx_a.add_global_root(handler);
    push_read_completion(
        &aio_sink(vm_a),
        ReadCompletion {
            handler_gref: hg,
            attachment_gref: 0,
            buffer_gref: 0,
            outcome: ReadOutcome::Count(0),
            ready_at: None,
        },
    );
    assert!(aio_dispatcher_should_run(vm_a));
    assert!(aio_dispatcher_should_run(vm_b));

    forget_vm_aio_state(vm_a);
    assert!(
        !aio_dispatcher_should_run(vm_a),
        "A's dispatchers must stop"
    );
    assert!(aio_dispatcher_should_run(vm_b), "B's must not");
    let started = std::time::Instant::now();
    assert!(!wait_for_pending(vm_a, Duration::from_secs(5)));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a disposed VM's wait must answer at once, not sleep its timeout"
    );

    // What a worker does when its job ends after the disposal.
    push_read_completion(
        &aio_vm_live(vm_a),
        ReadCompletion {
            handler_gref: hg,
            attachment_gref: 0,
            buffer_gref: 0,
            outcome: ReadOutcome::Eof,
            ready_at: None,
        },
    );
    assert!(
        aio_vm_live(vm_a).is_none(),
        "the late push must not recreate A's row"
    );
    // A dispatcher still draining after the disposal finds nothing and
    // creates nothing.
    drain_completions_pub(&mut ctx_a);
    assert!(aio_vm_live(vm_a).is_none());
    assert!(!ctx_a
        .recorded_calls()
        .iter()
        .any(|c| c.method_name == "completed"));

    forget_vm_aio_state(vm_a); // idempotent
    forget_vm_aio_state(vm_b);
}

/// Disposing a VM closes the `aio_registry` sockets it owns (an accept
/// worker parked on a disposed VM's listener used to re-poll it forever),
/// leaves another VM's alone, and a connect that finishes after the
/// disposal closes its socket instead of registering it where nothing can
/// reach it.
#[test]
fn w11b_forgetting_a_vm_closes_its_registry_sockets() {
    let _g = confine_test_lock().lock();
    let ctx_a = MockNativeContext::new();
    let ctx_b = MockNativeContext::new();
    let (vm_a, vm_b) = (ctx_a.vm_identity(), ctx_b.vm_identity());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let id_a = aio_register(
        vm_a,
        AioHandle::Listener(Arc::new(Mutex::new(listener)), addr),
    );
    let id_b = aio_register(vm_b, AioHandle::Pending);

    forget_vm_aio_state(vm_a);
    assert!(aio_registry().read().get(&id_a).is_none());
    assert!(aio_owners().read().get(&id_a).is_none());
    assert!(matches!(
        aio_registry().read().get(&id_b),
        Some(AioHandle::Pending)
    ));

    // A connect job of the disposed VM, finishing now (run inline: the
    // worker thread is not needed to exercise the arm).
    let peer = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = peer.local_addr().unwrap().port();
    let id_late = aio_register(vm_a, AioHandle::Pending);
    aio_remove(id_late); // what the disposal / a close did to the slot
    let _ = handle_job(
        vm_a,
        Job::Connect {
            id: id_late,
            addr: format!("127.0.0.1:{port}"),
            roots: HandlerRoots::default(),
            channel_gref: 0,
        },
    );
    assert!(
        aio_registry().read().get(&id_late).is_none(),
        "a late connect must not re-register its socket"
    );
    drop(peer);

    aio_remove(id_b);
    forget_vm_aio_state(vm_b);
}

/// The launcher runs once per VM, and while it runs it can tell which VM
/// it is launching for (`launching_vm`): that is how the VM side attaches
/// each pool to its own VM rather than to the newest one.
#[test]
fn w11b_dispatcher_is_launched_once_per_vm_and_names_its_vm() {
    let _g = confine_test_lock().lock();
    let ctx_a = MockNativeContext::new();
    let ctx_b = MockNativeContext::new();
    let (vm_a, vm_b) = (ctx_a.vm_identity(), ctx_b.vm_identity());
    let seen: Mutex<Vec<Option<usize>>> = Mutex::new(Vec::new());
    let launch = || seen.lock().push(launching_vm());

    launch_dispatcher_once(vm_a, Some(&launch));
    launch_dispatcher_once(vm_a, Some(&launch));
    launch_dispatcher_once(vm_b, Some(&launch));
    assert_eq!(
        *seen.lock(),
        vec![Some(vm_a), Some(vm_b)],
        "one launch per VM, each naming its own VM"
    );
    assert_eq!(launching_vm(), None, "restored after the launch");

    forget_vm_aio_state(vm_a);
    forget_vm_aio_state(vm_b);
}
