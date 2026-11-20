use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test uses them.
const W23B_VM_A: usize = 0x0B23_7A08;
const W23B_VM_B: usize = 0x0B23_7B08;
const W23B_VM_C: usize = 0x0B23_7C08;
const W23B_VM_D: usize = 0x0B23_7D08;
const W23B_VM_E: usize = 0x0B23_7E08;

// Each test forgets only its own VMs: the tests run in parallel, and a
// guard that forgot all five wiped a still-running test's rows.
struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_xnio_conduit_rows(vm);
        }
        W23B_FIRED.with(|f| f.borrow_mut().clear());
    }
}

thread_local! {
    /// The channel every `handleEvent` was dispatched for.
    static W23B_FIRED: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn record_handle_event(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "handleEvent" || descriptor != CHANNEL_LISTENER_HANDLE_EVENT_DESC {
        return None;
    }
    if let Some(Value::Object(Some(channel))) = args.first() {
        W23B_FIRED.with(|f| f.borrow_mut().push(channel.as_ptr() as usize));
    }
    Some(Ok(None))
}

/// Global roots one connection files: its source channel object, the two
/// IO-thread bindings, the sink's paired source and the two ready
/// handlers.
const ROOTS_PER_CONNECTION: usize = 6;

struct Conn {
    source_id: u64,
    sink_id: u64,
    source: ObjectRef,
    sink: ObjectRef,
    source_pipe: Arc<Pipe>,
}

/// A connection registered the way `xnio_worker::build_stream_connection`
/// registers one, plus the ready handlers XNIO installs on it.
fn open_conn(ctx: &mut MockNativeContext) -> Conn {
    let source_pipe = Arc::new(Pipe::new(1024));
    let source_id = register_source_channel(ConduitTransport::Pipe(source_pipe.clone()));
    let sink_id = register_sink_channel(ConduitTransport::Pipe(Arc::new(Pipe::new(1024))));
    let source = alloc_source_channel_obj(ctx, source_id).unwrap();
    let sink = alloc_sink_channel_obj(ctx, sink_id).unwrap();
    let io_thread = ctx.fresh_object_ref();
    remember_source_io_thread(ctx, source, io_thread);
    remember_sink_io_thread(ctx, sink, io_thread);
    remember_sink_paired_source(ctx, sink, source);
    let read_handler = ctx.fresh_object_ref();
    let write_handler = ctx.fresh_object_ref();
    native_source_set_read_ready_handler(
        ctx,
        &[
            Value::Object(Some(source)),
            Value::Object(Some(read_handler)),
        ],
    )
    .unwrap();
    native_sink_set_write_ready_handler(
        ctx,
        &[
            Value::Object(Some(sink)),
            Value::Object(Some(write_handler)),
        ],
    )
    .unwrap();
    Conn {
        source_id,
        sink_id,
        source,
        sink,
        source_pipe,
    }
}

/// Every row VM `vm` holds in this module's registries.
fn vm_rows(vm: usize) -> usize {
    let sources = source_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .vm_row_count(vm);
    let sinks = sink_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .vm_row_count(vm);
    let handles = source_obj_handle_by_id_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&vm)
        .map_or(0, |rows| rows.len());
    let mut n = sources + sinks + handles;
    for registry in [
        sink_paired_source_registry(),
        channel_io_thread_registry(),
        read_ready_handler_registry(),
        write_ready_handler_registry(),
    ] {
        let rows = registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .filter(|k| k.vm == vm)
            .count();
        n += rows;
    }
    n
}

fn live_source_ids(ctx: &MockNativeContext) -> Vec<u64> {
    let mut ids: Vec<u64> = source_obj_handles_by_id(ctx)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    ids.sort_unstable();
    ids
}

/// A connection goes -- rows, roots and channels -- once BOTH halves are
/// shut down, in either order; one half alone keeps it. Afterwards the
/// channel objects still answer as closed.
#[test]
fn w23b_closing_both_halves_unregisters_the_connection_and_releases_its_roots() {
    let _forget = Forget(&[W23B_VM_A]);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(W23B_VM_A);
    let roots_before = ctx.global_root_count();

    let conns: Vec<Conn> = (0..3).map(|_| open_conn(&mut ctx)).collect();
    assert_eq!(
        ctx.global_root_count(),
        roots_before + 3 * ROOTS_PER_CONNECTION
    );
    assert_eq!(live_source_ids(&ctx).len(), 3);

    // Source first: the half-closed connection stays.
    native_source_shutdown_reads(&mut ctx, &[Value::Object(Some(conns[0].source))]).unwrap();
    assert!(get_source_channel(conns[0].source_id).is_some());
    assert!(get_sink_channel(conns[0].sink_id).is_some());
    assert_eq!(
        ctx.global_root_count(),
        roots_before + 3 * ROOTS_PER_CONNECTION
    );
    native_sink_shutdown_writes(&mut ctx, &[Value::Object(Some(conns[0].sink))]).unwrap();
    assert!(get_source_channel(conns[0].source_id).is_none());
    assert!(get_sink_channel(conns[0].sink_id).is_none());
    assert_eq!(
        ctx.global_root_count(),
        roots_before + 2 * ROOTS_PER_CONNECTION
    );
    assert_eq!(live_source_ids(&ctx).len(), 2);

    // Sink first, then source (terminateWrites / truncateWrites / terminateReads).
    native_sink_truncate_writes(&mut ctx, &[Value::Object(Some(conns[1].sink))]).unwrap();
    assert!(get_source_channel(conns[1].source_id).is_some());
    native_source_shutdown_reads(&mut ctx, &[Value::Object(Some(conns[1].source))]).unwrap();
    native_sink_shutdown_writes(&mut ctx, &[Value::Object(Some(conns[2].sink))]).unwrap();
    native_source_shutdown_reads(&mut ctx, &[Value::Object(Some(conns[2].source))]).unwrap();

    assert_eq!(
        ctx.global_root_count(),
        roots_before,
        "no root outlives its connection"
    );
    assert!(live_source_ids(&ctx).is_empty());
    assert_eq!(vm_rows(W23B_VM_A), 0, "no row outlives its connection");
    for conn in &conns {
        assert!(get_source_channel(conn.source_id).is_none());
        assert!(get_sink_channel(conn.sink_id).is_none());
    }

    // The closed channels still answer as closed channels.
    let c = &conns[0];
    assert_eq!(
        native_source_is_read_shutdown(&mut ctx, &[Value::Object(Some(c.source))]).unwrap(),
        Some(Value::Int(1))
    );
    assert_eq!(
        native_sink_is_write_shutdown(&mut ctx, &[Value::Object(Some(c.sink))]).unwrap(),
        Some(Value::Int(1))
    );
    let any = ctx.fresh_object_ref();
    assert_eq!(source_channel_read(&mut ctx, c.source_id, any).unwrap(), -1);
    assert!(sink_channel_write(&mut ctx, c.sink_id, any).is_err());
    c.source_pipe.push(b"late");
    assert!(!source_may_be_notified(c.source_id));

    // `drop_source_channel` releases the root it forgets (it took no
    // `ctx` and released nothing).
    let lone_id = register_source_channel(ConduitTransport::Pipe(Arc::new(Pipe::new(16))));
    let _lone = alloc_source_channel_obj(&mut ctx, lone_id).unwrap();
    assert_eq!(ctx.global_root_count(), roots_before + 1);
    drop_source_channel(&mut ctx, lone_id);
    assert_eq!(ctx.global_root_count(), roots_before);
    assert_eq!(vm_rows(W23B_VM_A), 0);
}

/// The per-write walk (`notify_registered_sources_readable`) visits the
/// VM's open connections only: a closed connection with pending bytes and
/// a listener is neither listed nor fired.
#[test]
fn w23b_the_write_walk_sees_only_open_connections() {
    let _forget = Forget(&[W23B_VM_B]);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(W23B_VM_B);
    let closed = open_conn(&mut ctx);
    let open = open_conn(&mut ctx);
    for conn in [&closed, &open] {
        ctx.set_field(conn.source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
        let listener = ctx.fresh_object_ref();
        native_source_set_read_listener(
            &mut ctx,
            &[
                Value::Object(Some(conn.source)),
                Value::Object(Some(listener)),
            ],
        )
        .unwrap();
        conn.source_pipe.push(b"x");
    }
    native_source_shutdown_reads(&mut ctx, &[Value::Object(Some(closed.source))]).unwrap();
    native_sink_shutdown_writes(&mut ctx, &[Value::Object(Some(closed.sink))]).unwrap();

    assert_eq!(live_source_ids(&ctx), vec![open.source_id]);
    ctx.set_invoke_virtual_hook(record_handle_event);
    notify_registered_sources_readable(&mut ctx, false, None);
    let fired = W23B_FIRED.with(|f| f.borrow().clone());
    assert_eq!(fired, vec![open.source.as_ptr() as usize]);

    // A suspended source is skipped before its root is even resolved.
    native_source_suspend_reads(&mut ctx, &[Value::Object(Some(open.source))]).unwrap();
    assert!(!source_may_be_notified(open.source_id));
    W23B_FIRED.with(|f| f.borrow_mut().clear());
    notify_registered_sources_readable(&mut ctx, false, None);
    assert!(W23B_FIRED.with(|f| f.borrow().is_empty()));
}

/// Two VMs' connections stay separate: closing VM C's releases exactly its
/// roots -- although the mock numbers both VMs' global roots from the
/// same start, so VM D's rows carry the same handle numbers (the old
/// any-VM orphan check kept C's handles rooted because of them) -- and
/// leaves every one of VM D's rows and roots in place.
#[test]
fn w23b_two_vms_connections_stay_separate() {
    let _forget = Forget(&[W23B_VM_C, W23B_VM_D]);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(W23B_VM_C);
    let mut d = crate::test_utils::mock_ctx();
    d.set_vm_identity(W23B_VM_D);
    let c_before = c.global_root_count();
    let d_before = d.global_root_count();
    let conn_c = open_conn(&mut c);
    let conn_d = open_conn(&mut d);
    let d_rows = vm_rows(W23B_VM_D);

    native_source_shutdown_reads(&mut c, &[Value::Object(Some(conn_c.source))]).unwrap();
    native_sink_shutdown_writes(&mut c, &[Value::Object(Some(conn_c.sink))]).unwrap();

    assert_eq!(c.global_root_count(), c_before);
    assert_eq!(vm_rows(W23B_VM_C), 0);
    assert_eq!(d.global_root_count(), d_before + ROOTS_PER_CONNECTION);
    assert_eq!(vm_rows(W23B_VM_D), d_rows);
    assert_eq!(live_source_ids(&d), vec![conn_d.source_id]);
    assert!(ready_handler_of(read_ready_handler_registry(), &d, conn_d.source).is_some());
    assert!(get_source_channel(conn_d.source_id).is_some());
    assert!(get_sink_channel(conn_d.sink_id).is_some());
}

/// `notify_source_readable` releases its pin on every exit. It released
/// none, so the never-returning source poller grew its pin stack by one
/// per registered source per tick.
#[test]
fn w23b_notify_releases_its_pin() {
    let _forget = Forget(&[W23B_VM_E]);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(W23B_VM_E);
    let conn = open_conn(&mut ctx);
    let pins = ctx.native_pin_count_for_test();

    // Suspended (the allocation default): the loop skips.
    notify_source_readable(&mut ctx, conn.source, false);
    assert_eq!(ctx.native_pin_count_for_test(), pins);

    // Resumed with pending bytes and a listener: it fires.
    ctx.set_field(conn.source, SRC_FIELD_READ_SUSPENDED, Value::Int(0));
    let listener = ctx.fresh_object_ref();
    native_source_set_read_listener(
        &mut ctx,
        &[
            Value::Object(Some(conn.source)),
            Value::Object(Some(listener)),
        ],
    )
    .unwrap();
    conn.source_pipe.push(b"x");
    ctx.set_invoke_virtual_hook(record_handle_event);
    notify_source_readable(&mut ctx, conn.source, false);
    assert_eq!(W23B_FIRED.with(|f| f.borrow().len()), 1);
    assert_eq!(ctx.native_pin_count_for_test(), pins);

    // The walk, many times over (the poller's shape).
    for _ in 0..16 {
        notify_registered_sources_readable(&mut ctx, false, None);
    }
    assert_eq!(ctx.native_pin_count_for_test(), pins);
}
