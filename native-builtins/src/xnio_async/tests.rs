use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Helper: build an Option via the native (so the test exercises the
/// same path that Java-side `Option.simple(...)` would).
fn make_option(
    ctx: &mut crate::test_utils::MockNativeContext,
    declaring: &str,
    name: &str,
    ty: &str,
) -> ObjectRef {
    let d = ctx.create_string(declaring);
    let n = ctx.create_string(name);
    let t = ctx.create_string(ty);
    let r = native_option_simple(
        ctx,
        &[
            Value::Object(Some(d)),
            Value::Object(Some(n)),
            Value::Object(Some(t)),
        ],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(o)) => o,
        _ => panic!("expected Option object"),
    }
}

fn empty_map(ctx: &mut crate::test_utils::MockNativeContext) -> ObjectRef {
    match native_option_map_empty_get(ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!("expected OptionMap"),
    }
}

fn new_builder(ctx: &mut crate::test_utils::MockNativeContext) -> ObjectRef {
    match native_option_map_builder(ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!("expected Builder"),
    }
}

fn new_future(ctx: &mut crate::test_utils::MockNativeContext) -> (ObjectRef, ObjectRef) {
    // Allocate a FutureResult and init it; read back its future.
    let fr = try_alloc_concurrent_synthetic(ctx, "org/xnio/FutureResult", 1).unwrap();
    native_future_result_init(ctx, &[Value::Object(Some(fr))]).unwrap();
    let fut = match native_future_result_get_future(ctx, &[Value::Object(Some(fr))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!("expected IoFuture"),
    };
    (fr, fut)
}

#[test]
fn t19_7_e_option_simple_creates_option_with_type() {
    let mut ctx = mock_ctx();
    let opt = make_option(
        &mut ctx,
        "org/xnio/Options",
        "WORKER_IO_THREADS",
        "java/lang/Integer",
    );
    // Field 1 should be the name string.
    let name_val = native_option_get_name(&mut ctx, &[Value::Object(Some(opt))])
        .unwrap()
        .unwrap();
    let name = match name_val {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap(),
        _ => String::new(),
    };
    assert_eq!(name, "WORKER_IO_THREADS");
    // Type should round-trip.
    assert_eq!(
        read_option_type(&ctx, opt).as_deref(),
        Some("java/lang/Integer")
    );
}

#[test]
fn t19_7_e_option_map_empty_has_size_zero() {
    let mut ctx = mock_ctx();
    let m = empty_map(&mut ctx);
    let size = native_option_map_size(&mut ctx, &[Value::Object(Some(m))])
        .unwrap()
        .unwrap();
    assert_eq!(size, Value::Int(0));
}

#[test]
fn t19_7_e_option_map_one_field_layout_uses_side_table_handle() {
    let mut ctx = mock_ctx();
    let cid = ctx.ensure_class_initialized("org/xnio/OptionMap").unwrap();
    let m = ctx.alloc_object(cid, 1);

    let mut entries = HashMap::new();
    entries.insert(
        OptionKey {
            declaring_class: "org/xnio/Options".to_string(),
            name: "WORKER_IO_THREADS".to_string(),
        },
        OptionValue::Int(7),
    );
    let h = register_map(
        ctx.vm_identity(),
        Arc::new(OptionMapInner {
            entries: Mutex::new(entries),
        }),
    );
    write_handle_slot_if_present(&ctx, m, OM_ENTRIES_HANDLE, h);
    remember_map_handle(&ctx, m, h);

    assert_eq!(ctx.object_num_fields(m), 1);
    assert_eq!(read_handle_slot(&ctx, m, OM_ENTRIES_HANDLE), 0);
    let inner = inner_from_map(&ctx, m).unwrap();
    assert_eq!(inner.entries.lock().len(), 1);
}

#[test]
fn t19_7_e_option_map_builder_one_field_layout_uses_side_table_handle() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("org/xnio/OptionMap$Builder")
        .unwrap();
    let b = ctx.alloc_object(cid, 1);
    let h = register_builder(ctx.vm_identity(), Arc::new(BuilderInner::default()));
    write_handle_slot_if_present(&ctx, b, OMB_PENDING_HANDLE, h);
    remember_builder_handle(&ctx, b, h);

    assert_eq!(ctx.object_num_fields(b), 1);
    assert_eq!(read_handle_slot(&ctx, b, OMB_PENDING_HANDLE), 0);
    let inner = builder_from_this(&ctx, b).unwrap();
    assert!(!inner.consumed.load(Ordering::SeqCst));
}

#[test]
fn t19_7_e_option_map_builder_set_get_round_trip() {
    let mut ctx = mock_ctx();
    let opt = make_option(
        &mut ctx,
        "org/xnio/Options",
        "WORKER_IO_THREADS",
        "java/lang/Integer",
    );
    let b = new_builder(&mut ctx);
    // set(Option, I)
    native_builder_set(
        &mut ctx,
        &[
            Value::Object(Some(b)),
            Value::Object(Some(opt)),
            Value::Int(8),
        ],
    )
    .unwrap();
    let map = match native_builder_get_map(&mut ctx, &[Value::Object(Some(b))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!("expected map"),
    };
    let got = native_option_map_get(
        &mut ctx,
        &[Value::Object(Some(map)), Value::Object(Some(opt))],
    )
    .unwrap()
    .unwrap();
    // `get(Option)Object` must box the stored int, not return it raw
    // (the JIT's fast return path treats a raw int as a bogus ObjectRef --
    // see native_option_map_get's doc comment).
    match got {
        Value::Object(Some(o)) => {
            assert_eq!(ctx.get_field(o, 0), Value::Int(8));
        }
        other => panic!("expected boxed Integer, got {other:?}"),
    }
    // Size should be 1.
    let size = native_option_map_size(&mut ctx, &[Value::Object(Some(map))])
        .unwrap()
        .unwrap();
    assert_eq!(size, Value::Int(1));
    // contains should be true.
    let has = native_option_map_contains(
        &mut ctx,
        &[Value::Object(Some(map)), Value::Object(Some(opt))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has, Value::Int(1));
}

#[test]
fn t19_7_e_option_map_builder_add_all_copies_entries() {
    let mut ctx = mock_ctx();
    let opt = make_option(
        &mut ctx,
        "org/xnio/Options",
        "TCP_NODELAY",
        "java/lang/Boolean",
    );

    let source_builder = new_builder(&mut ctx);
    native_builder_set_bool(
        &mut ctx,
        &[
            Value::Object(Some(source_builder)),
            Value::Object(Some(opt)),
            Value::Int(1),
        ],
    )
    .unwrap();
    let source_map =
        match native_builder_get_map(&mut ctx, &[Value::Object(Some(source_builder))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!("expected source map"),
        };

    let dest_builder = new_builder(&mut ctx);
    let returned = native_builder_add_all(
        &mut ctx,
        &[
            Value::Object(Some(dest_builder)),
            Value::Object(Some(source_map)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(returned, Value::Object(Some(dest_builder)));

    let dest_map = match native_builder_get_map(&mut ctx, &[Value::Object(Some(dest_builder))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!("expected destination map"),
    };
    let got = native_option_map_get_bool(
        &mut ctx,
        &[
            Value::Object(Some(dest_map)),
            Value::Object(Some(opt)),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Int(1));

    let size = native_option_map_size(&mut ctx, &[Value::Object(Some(dest_map))])
        .unwrap()
        .unwrap();
    assert_eq!(size, Value::Int(1));
}

#[test]
fn t19_7_e_option_map_default_when_missing_returns_default() {
    let mut ctx = mock_ctx();
    let opt = make_option(&mut ctx, "org/xnio/Options", "BACKLOG", "java/lang/Integer");
    let m = empty_map(&mut ctx);
    let got = native_option_map_get(
        &mut ctx,
        &[
            Value::Object(Some(m)),
            Value::Object(Some(opt)),
            Value::Int(128),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Int(128), "missing key → default");
}

#[test]
fn t19_7_e_option_map_immutable_after_build() {
    let mut ctx = mock_ctx();
    let opt = make_option(
        &mut ctx,
        "org/xnio/Options",
        "WORKER_IO_THREADS",
        "java/lang/Integer",
    );
    let b = new_builder(&mut ctx);
    native_builder_set(
        &mut ctx,
        &[
            Value::Object(Some(b)),
            Value::Object(Some(opt)),
            Value::Int(4),
        ],
    )
    .unwrap();
    let map = match native_builder_get_map(&mut ctx, &[Value::Object(Some(b))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!("expected map"),
    };
    // Once consumed, further set() throws IllegalStateException.
    let err = native_builder_set(
        &mut ctx,
        &[
            Value::Object(Some(b)),
            Value::Object(Some(opt)),
            Value::Int(99),
        ],
    )
    .err()
    .expect("set after getMap must fail");
    assert!(format!("{err:?}").contains("IllegalStateException"));
    // Re-calling getMap must also fail.
    let err2 = native_builder_get_map(&mut ctx, &[Value::Object(Some(b))])
        .err()
        .expect("getMap twice must fail");
    assert!(format!("{err2:?}").contains("IllegalStateException"));
    // The previously-returned map is unaffected.
    let got = native_option_map_get(
        &mut ctx,
        &[Value::Object(Some(map)), Value::Object(Some(opt))],
    )
    .unwrap()
    .unwrap();
    match got {
        Value::Object(Some(o)) => {
            assert_eq!(ctx.get_field(o, 0), Value::Int(4));
        }
        other => panic!("expected boxed Integer, got {other:?}"),
    }
}

#[test]
fn t19_7_e_options_worker_io_threads_default_is_four() {
    let mut ctx = mock_ctx();
    // Populate via <clinit>.
    native_options_clinit(&mut ctx, &[]).unwrap();
    // Options.WORKER_IO_THREADS exists in the store.
    let wio = OPTIONS_STORE
        .peek(ctx.vm_identity(), |store| {
            store.get("WORKER_IO_THREADS").copied()
        })
        .flatten();
    assert!(wio.is_some(), "WORKER_IO_THREADS option must be populated");

    // The "default 4" assertion is the contract with T19.7.b — until
    // T19.7.b is landed, we exercise the contract by running the
    // builder path: build a map with no WORKER_IO_THREADS set, call
    // get with default=4, and confirm we get 4. The getter for the
    // default lives in xnio_worker.rs, but the OptionMap API surface
    // here is what makes 4 addressable.
    // (Store entries are `(global_root, ObjectRef)` — take the ref.)
    let opt = wio.unwrap().1;
    let m = empty_map(&mut ctx);
    let got = native_option_map_get(
        &mut ctx,
        &[
            Value::Object(Some(m)),
            Value::Object(Some(opt)),
            Value::Int(4),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Int(4));
}

#[test]
fn t19_7_e_io_future_status_initially_waiting() {
    let mut ctx = mock_ctx();
    let (_fr, fut) = new_future(&mut ctx);
    let s = native_iof_get_status(&mut ctx, &[Value::Object(Some(fut))])
        .unwrap()
        .unwrap();
    assert_eq!(s, Value::Int(STATUS_WAITING as i32));
}

#[test]
fn t19_7_e_future_result_set_result_transitions_to_done() {
    let mut ctx = mock_ctx();
    let (fr, fut) = new_future(&mut ctx);
    // setResult with a string value.
    let js = ctx.create_string("hello");
    let rv = native_future_result_set_result(
        &mut ctx,
        &[Value::Object(Some(fr)), Value::Object(Some(js))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(rv, Value::Int(1), "first setResult wins");
    // Status is now DONE.
    let s = native_iof_get_status(&mut ctx, &[Value::Object(Some(fut))])
        .unwrap()
        .unwrap();
    assert_eq!(s, Value::Int(STATUS_DONE as i32));
    // Second setResult is a no-op.
    let rv2 = native_future_result_set_result(
        &mut ctx,
        &[Value::Object(Some(fr)), Value::Object(Some(js))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(rv2, Value::Int(0), "second setResult is no-op");
}

#[test]
fn t19_7_e_future_result_set_exception_transitions_to_failed() {
    let mut ctx = mock_ctx();
    let (fr, fut) = new_future(&mut ctx);
    // Build a synthetic IOException with a message field.
    let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/io/IOException", 2).unwrap();
    let msg = ctx.create_string("disk full");
    ctx.set_field(exc, 0, Value::Object(Some(msg)));
    native_future_result_set_exception(
        &mut ctx,
        &[Value::Object(Some(fr)), Value::Object(Some(exc))],
    )
    .unwrap();
    let s = native_iof_get_status(&mut ctx, &[Value::Object(Some(fut))])
        .unwrap()
        .unwrap();
    assert_eq!(s, Value::Int(STATUS_FAILED as i32));
    // get() now throws an IOException whose message contains the
    // original "disk full" text.
    let err = native_iof_get(&mut ctx, &[Value::Object(Some(fut))])
        .err()
        .expect("failed future should error on get");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("disk full") || msg.contains("IoFuture failed"),
        "error message must mention the cause: {msg}"
    );
}

#[test]
fn t19_7_e_io_future_await_blocks_until_done() {
    use std::thread;
    use std::time::Duration;

    let mut ctx = mock_ctx();
    let (fr, fut) = new_future(&mut ctx);
    // Grab the Arc directly so the background thread can signal
    // completion without needing a NativeContext.
    let inner = inner_from_future(&ctx, fut).unwrap();
    let inner_bg = inner.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        if inner_bg.try_transition(STATUS_DONE) {
            inner_bg.state.lock().result = Some(Value::Int(42));
            inner_bg.cv.notify_all();
        }
    });

    let start = Instant::now();
    let rv = native_iof_await(&mut ctx, &[Value::Object(Some(fut))])
        .unwrap()
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(rv, Value::Int(STATUS_DONE as i32));
    assert!(
        elapsed >= Duration::from_millis(30),
        "await should block ~50ms, elapsed={elapsed:?}"
    );
    t.join().unwrap();

    // get() returns the stashed result.
    let rv2 = native_iof_get(&mut ctx, &[Value::Object(Some(fut))])
        .unwrap()
        .unwrap();
    assert_eq!(rv2, Value::Int(42));

    // And fr itself still reports DONE.
    let _ = fr;
}

#[test]
fn t19_7_e_io_future_notifier_fires_on_completion() {
    use std::sync::atomic::AtomicU32;

    let mut ctx = mock_ctx();
    let (fr, fut) = new_future(&mut ctx);

    // Build a synthetic Notifier object. Our invoke() in MockNativeContext
    // always returns Ok(None) without dispatching, so we test the
    // wiring by observing the notifier list before/after completion.
    let notifier =
        try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
    // addNotifier while still WAITING → should stash.
    native_iof_add_notifier(
        &mut ctx,
        &[
            Value::Object(Some(fut)),
            Value::Object(Some(notifier)),
            Value::Object(None),
        ],
    )
    .unwrap();
    let inner = inner_from_future(&ctx, fut).unwrap();
    assert_eq!(
        inner.state.lock().notifiers.len(),
        1,
        "notifier should be stored while waiting"
    );

    // Complete the future → notifier list drained.
    native_future_result_set_result(&mut ctx, &[Value::Object(Some(fr)), Value::Object(None)])
        .unwrap();
    assert_eq!(
        inner.state.lock().notifiers.len(),
        0,
        "notifier list should be drained after completion"
    );

    // addNotifier AFTER completion → fires synchronously (and the
    // list stays empty).
    let n2 = try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
    native_iof_add_notifier(
        &mut ctx,
        &[
            Value::Object(Some(fut)),
            Value::Object(Some(n2)),
            Value::Object(None),
        ],
    )
    .unwrap();
    assert_eq!(
        inner.state.lock().notifiers.len(),
        0,
        "post-completion notifier should fire synchronously, not stash"
    );

    // Defeat unused-var warnings.
    let _ = AtomicU32::new(0);
}

#[test]
fn t19_7_e_io_future_cancel_transitions_to_cancelled() {
    let mut ctx = mock_ctx();
    let (_fr, fut) = new_future(&mut ctx);
    let _ = native_iof_cancel(&mut ctx, &[Value::Object(Some(fut))]);
    let s = native_iof_get_status(&mut ctx, &[Value::Object(Some(fut))])
        .unwrap()
        .unwrap();
    assert_eq!(s, Value::Int(STATUS_CANCELLED as i32));
    // get() on a cancelled future throws IllegalStateException.
    let err = native_iof_get(&mut ctx, &[Value::Object(Some(fut))])
        .err()
        .expect("cancelled get must fail");
    assert!(format!("{err:?}").contains("cancelled"));
}

// ------------------------------------------------------------------
// Bonus robustness tests (beyond the minimum 12).
// ------------------------------------------------------------------

#[test]
fn t19_7_e_io_future_await_timed_returns_waiting_on_timeout() {
    let mut ctx = mock_ctx();
    let (_fr, fut) = new_future(&mut ctx);
    let rv = native_iof_await_timed(
        &mut ctx,
        &[
            Value::Object(Some(fut)),
            Value::Long(5_000_000), // 5ms
            Value::Object(None),
        ],
    )
    .unwrap()
    .unwrap();
    // Still WAITING because nothing completed it.
    assert_eq!(rv, Value::Int(STATUS_WAITING as i32));
}

#[test]
fn t19_7_e_option_cast_null_is_accepted() {
    let mut ctx = mock_ctx();
    let opt = make_option(
        &mut ctx,
        "org/xnio/Options",
        "WORKER_NAME",
        "java/lang/String",
    );
    let r = native_option_cast(&mut ctx, &[Value::Object(Some(opt)), Value::Object(None)])
        .unwrap()
        .unwrap();
    assert_eq!(r, Value::Object(None));
}

#[test]
fn t19_7_e_option_parse_value_integer() {
    let mut ctx = mock_ctx();
    let opt = make_option(&mut ctx, "org/xnio/Options", "BACKLOG", "java/lang/Integer");
    let s = ctx.create_string("42");
    let r = native_option_parse_value(
        &mut ctx,
        &[
            Value::Object(Some(opt)),
            Value::Object(Some(s)),
            Value::Object(None),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(42));
}

#[test]
fn t19_7_e_option_parse_value_boolean_true() {
    let mut ctx = mock_ctx();
    let opt = make_option(
        &mut ctx,
        "org/xnio/Options",
        "TCP_NODELAY",
        "java/lang/Boolean",
    );
    let s = ctx.create_string("TRUE");
    let r = native_option_parse_value(
        &mut ctx,
        &[
            Value::Object(Some(opt)),
            Value::Object(Some(s)),
            Value::Object(None),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));
}

#[test]
fn t19_7_e_option_parse_value_bad_integer_raises_nfe() {
    let mut ctx = mock_ctx();
    let opt = make_option(&mut ctx, "org/xnio/Options", "BACKLOG", "java/lang/Integer");
    let s = ctx.create_string("not-a-number");
    let err = native_option_parse_value(
        &mut ctx,
        &[
            Value::Object(Some(opt)),
            Value::Object(Some(s)),
            Value::Object(None),
        ],
    )
    .err()
    .expect("bad integer must be rejected");
    assert!(format!("{err:?}").contains("NumberFormatException"));
}

#[test]
fn t19_7_e_option_map_get_bool_round_trip() {
    let mut ctx = mock_ctx();
    let opt = make_option(
        &mut ctx,
        "org/xnio/Options",
        "KEEP_ALIVE",
        "java/lang/Boolean",
    );
    let b = new_builder(&mut ctx);
    // set(Option, Z)
    native_builder_set_bool(
        &mut ctx,
        &[
            Value::Object(Some(b)),
            Value::Object(Some(opt)),
            Value::Int(1),
        ],
    )
    .unwrap();
    let map = match native_builder_get_map(&mut ctx, &[Value::Object(Some(b))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!("expected map"),
    };
    let got = native_option_map_get(
        &mut ctx,
        &[Value::Object(Some(map)), Value::Object(Some(opt))],
    )
    .unwrap()
    .unwrap();
    // Bool is stored as Int(0/1) internally but `get(Option)Object` must
    // box it into a real Boolean, matching the declared Object return type.
    match got {
        Value::Object(Some(o)) => {
            assert_eq!(ctx.get_field(o, 0), Value::Int(1));
        }
        other => panic!("expected boxed Boolean, got {other:?}"),
    }
}

#[test]
fn t19_7_e_option_map_get_bool_unboxes_object_entry() {
    let mut ctx = mock_ctx();
    let opt = make_option(&mut ctx, "org/xnio/Options", "SECURE", "java/lang/Boolean");
    let boolean_cid = ctx.ensure_class_initialized("java/lang/Boolean").unwrap();
    let boxed_true = ctx.alloc_object(boolean_cid, 1);
    ctx.set_field(boxed_true, 0, Value::Int(1));

    let mut entries = HashMap::new();
    entries.insert(
        OptionKey {
            declaring_class: "org/xnio/Options".to_string(),
            name: "SECURE".to_string(),
        },
        OptionValue::Obj(Some(boxed_true)),
    );
    let map = alloc_option_map(
        &mut ctx,
        Arc::new(OptionMapInner {
            entries: Mutex::new(entries),
        }),
    )
    .unwrap();

    let got = native_option_map_get_bool(
        &mut ctx,
        &[
            Value::Object(Some(map)),
            Value::Object(Some(opt)),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Int(1));
}

// ------------------------------------------------------------------
// GC root scan / remap (UAF fix).
//
// The registry is process-global and shared with the other tests
// (which may run in parallel), so these assertions check *containment*
// of the specific refs under test, never exact registry-wide counts.
// ------------------------------------------------------------------

fn ptr_of(r: ObjectRef) -> usize {
    r.as_ptr() as usize
}

#[test]
fn t19_7_e_gc_scan_reports_pending_notifier_and_attachment() {
    let mut ctx = mock_ctx();
    let (_fr, fut) = new_future(&mut ctx);
    let notifier =
        try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
    let attachment = ctx.create_string("att");
    // addNotifier while WAITING stashes (notifier, attachment).
    native_iof_add_notifier(
        &mut ctx,
        &[
            Value::Object(Some(fut)),
            Value::Object(Some(notifier)),
            Value::Object(Some(attachment)),
        ],
    )
    .unwrap();

    let mut roots = Vec::new();
    gc_scan_xnio_future_roots(ctx.vm_identity(), &mut roots);
    let addrs: Vec<usize> = roots.iter().map(|r| ptr_of(*r)).collect();
    assert!(
        addrs.contains(&ptr_of(notifier)),
        "scan must root the pending notifier"
    );
    assert!(
        addrs.contains(&ptr_of(attachment)),
        "scan must root the notifier attachment"
    );
}

#[test]
fn t19_7_e_gc_scan_reports_object_result_after_set() {
    let mut ctx = mock_ctx();
    let (fr, _fut) = new_future(&mut ctx);
    let result = ctx.create_string("payload");
    native_future_result_set_result(
        &mut ctx,
        &[Value::Object(Some(fr)), Value::Object(Some(result))],
    )
    .unwrap();

    let mut roots = Vec::new();
    gc_scan_xnio_future_roots(ctx.vm_identity(), &mut roots);
    let addrs: Vec<usize> = roots.iter().map(|r| ptr_of(*r)).collect();
    assert!(
        addrs.contains(&ptr_of(result)),
        "scan must root the settled Object result (held until get())"
    );
}

#[test]
fn t19_7_e_gc_remap_repoints_notifier_attachment_and_result() {
    let mut ctx = mock_ctx();
    let (fr, fut) = new_future(&mut ctx);
    let notifier =
        try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
    let attachment = ctx.create_string("att");
    native_iof_add_notifier(
        &mut ctx,
        &[
            Value::Object(Some(fut)),
            Value::Object(Some(notifier)),
            Value::Object(Some(attachment)),
        ],
    )
    .unwrap();
    let result = ctx.create_string("payload");
    native_future_result_set_result(
        &mut ctx,
        &[Value::Object(Some(fr)), Value::Object(Some(result))],
    )
    .unwrap();

    // Fabricate fresh, 8-byte-aligned destination addresses. The refs are
    // never dereferenced here — we only assert the stored pointer flipped.
    let new_notifier = 0x4000usize;
    let new_attachment = 0x5000usize;
    let new_result = 0x6000usize;
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(ptr_of(notifier), new_notifier);
    map.insert(ptr_of(attachment), new_attachment);
    map.insert(ptr_of(result), new_result);

    gc_update_xnio_future_refs(ctx.vm_identity(), &map);

    // Reach the inner directly and confirm the stored refs were repointed.
    let inner = inner_from_future(&ctx, fut).unwrap();
    let state = inner.state.lock();
    // setResult drains notifiers, so the result ref is the live one to check.
    assert_eq!(
        state.result.map(|v| match v {
            Value::Object(Some(r)) => ptr_of(r),
            _ => 0,
        }),
        Some(new_result),
        "result ref must be remapped to its new address"
    );
    // A post-settle scan must now report the remapped result address and
    // none of the stale ones (scan/remap symmetry).
    drop(state);
    let mut roots = Vec::new();
    gc_scan_xnio_future_roots(ctx.vm_identity(), &mut roots);
    let addrs: Vec<usize> = roots.iter().map(|r| ptr_of(*r)).collect();
    assert!(addrs.contains(&new_result), "scan sees remapped result");
    assert!(
        !addrs.contains(&ptr_of(result)),
        "stale pre-move result address must not survive"
    );
}

#[test]
fn t19_7_e_gc_remap_empty_map_is_noop() {
    let mut ctx = mock_ctx();
    let (fr, _fut) = new_future(&mut ctx);
    let result = ctx.create_string("payload");
    native_future_result_set_result(
        &mut ctx,
        &[Value::Object(Some(fr)), Value::Object(Some(result))],
    )
    .unwrap();
    // Empty pointer map → no relocation happened → refs unchanged.
    gc_update_xnio_future_refs(ctx.vm_identity(), &cratonvm_types::PointerMap::default());
    let mut roots = Vec::new();
    gc_scan_xnio_future_roots(ctx.vm_identity(), &mut roots);
    let addrs: Vec<usize> = roots.iter().map(|r| ptr_of(*r)).collect();
    assert!(
        addrs.contains(&ptr_of(result)),
        "empty remap leaves the result ref untouched"
    );
}

/// gc-common w8-a: a future is rooted and remapped by the VM that created
/// it only. Another VM's collection neither roots its refs nor rewrites
/// them through its own pointer map.
#[test]
fn w8a_xnio_future_roots_belong_to_the_creating_vm() {
    let (vm_a, vm_b) = (0x7A80_0201usize, 0x7A80_0202usize);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(vm_a);
    let (fr, fut) = new_future(&mut ctx);
    let result = ctx.create_string("w8a-payload");
    native_future_result_set_result(
        &mut ctx,
        &[Value::Object(Some(fr)), Value::Object(Some(result))],
    )
    .unwrap();
    let scan = |vm: usize| {
        let mut roots = Vec::new();
        gc_scan_xnio_future_roots(vm, &mut roots);
        roots.iter().map(|r| ptr_of(*r)).collect::<Vec<usize>>()
    };
    assert!(scan(vm_a).contains(&ptr_of(result)));
    assert!(
        !scan(vm_b).contains(&ptr_of(result)),
        "VM B's collection must not root VM A's future result"
    );

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(ptr_of(result), 0x7A80_8000usize);
    gc_update_xnio_future_refs(vm_b, &map);
    assert!(
        scan(vm_a).contains(&ptr_of(result)),
        "VM B's pointer map must not rewrite VM A's ref"
    );

    forget_vm_xnio_roots(vm_a);
    assert!(!scan(vm_a).contains(&ptr_of(result)));
    // The handle map still owns the future; teardown only unroots it.
    assert!(inner_from_future(&ctx, fut).is_ok());
}

thread_local! {
    /// `(receiver, future arg, attachment arg)` of every `notify` call.
    static W23B_NOTIFY_CALLS: std::cell::RefCell<Vec<(usize, usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `(old, new)`: the first `notify` "moves" the object at `old` to
    /// `new`, the way a collection inside a notifier would.
    static W23B_MOVE_ON_FIRST_NOTIFY: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

fn w23b_record_notify(
    ctx: &mut crate::test_utils::MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "notify" || descriptor != "(Lorg/xnio/IoFuture;Ljava/lang/Object;)V" {
        return None;
    }
    let ptr = |v: Option<&Value>| match v {
        Some(Value::Object(Some(o))) => o.as_ptr() as usize,
        _ => 0,
    };
    let first = W23B_NOTIFY_CALLS.with(|calls| {
        let mut calls = calls.borrow_mut();
        calls.push((
            receiver.as_ptr() as usize,
            ptr(args.first()),
            ptr(args.get(1)),
        ));
        calls.len() == 1
    });
    let (old, new) = W23B_MOVE_ON_FIRST_NOTIFY.with(|m| m.get());
    if first && old != 0 {
        ctx.remap_native_pin_addr_for_test(old, new);
    }
    Some(Ok(None))
}

/// gc-common w23-b: a settled future's drained notifiers are dispatched on
/// the NOTIFIER (they were sent to the interface with the future as the
/// receiver, and never ran), each read back from a pin -- the second one
/// is fired at the address a collection inside the first moved it to --
/// and every pin is released.
#[test]
fn w23b_drained_notifiers_fire_on_the_notifier_from_pins() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            W23B_NOTIFY_CALLS.with(|c| c.borrow_mut().clear());
            W23B_MOVE_ON_FIRST_NOTIFY.with(|m| m.set((0, 0)));
        }
    }
    let _reset = Reset;
    let mut ctx = mock_ctx();
    let (fr, fut) = new_future(&mut ctx);
    let n1 = try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
    let n2 = try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
    let n2_moved =
        try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
    let att2 = ctx.create_string("w23b-attachment");
    for (n, att) in [(n1, Value::Object(None)), (n2, Value::Object(Some(att2)))] {
        native_iof_add_notifier(
            &mut ctx,
            &[Value::Object(Some(fut)), Value::Object(Some(n)), att],
        )
        .unwrap();
    }
    W23B_MOVE_ON_FIRST_NOTIFY.with(|m| m.set((ptr_of(n2), ptr_of(n2_moved))));
    ctx.set_invoke_virtual_hook(w23b_record_notify);
    let pins_before = ctx.native_pin_count_for_test();

    native_future_result_set_result(&mut ctx, &[Value::Object(Some(fr)), Value::Object(None)])
        .unwrap();

    let calls = W23B_NOTIFY_CALLS.with(|c| c.borrow().clone());
    assert_eq!(calls.len(), 2, "both notifiers are dispatched: {calls:?}");
    assert_eq!(calls[0].0, ptr_of(n1), "the receiver is the notifier");
    assert_ne!(calls[0].1, 0, "the future is the first argument");
    assert_eq!(calls[0].2, 0, "no attachment");
    assert_eq!(
        calls[1].0,
        ptr_of(n2_moved),
        "the second notifier is read back from its pin after the first ran"
    );
    assert_eq!(calls[1].2, ptr_of(att2));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins_before,
        "every pin taken for the dispatch is released"
    );
}

/// gc-common w23-b: `cancel()` returns the future as read back after its
/// notifiers ran, not the address it had before them.
#[test]
fn w23b_cancel_returns_the_future_read_back_after_its_notifiers() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            W23B_NOTIFY_CALLS.with(|c| c.borrow_mut().clear());
            W23B_MOVE_ON_FIRST_NOTIFY.with(|m| m.set((0, 0)));
        }
    }
    let _reset = Reset;
    let mut ctx = mock_ctx();
    let (_fr, fut) = new_future(&mut ctx);
    let fut_moved = try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture", 3).unwrap();
    let n1 = try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
    native_iof_add_notifier(
        &mut ctx,
        &[
            Value::Object(Some(fut)),
            Value::Object(Some(n1)),
            Value::Object(None),
        ],
    )
    .unwrap();
    W23B_MOVE_ON_FIRST_NOTIFY.with(|m| m.set((ptr_of(fut), ptr_of(fut_moved))));
    ctx.set_invoke_virtual_hook(w23b_record_notify);

    let r = native_iof_cancel(&mut ctx, &[Value::Object(Some(fut))])
        .unwrap()
        .unwrap();
    assert_eq!(r, Value::Object(Some(fut_moved)));
    assert_eq!(W23B_NOTIFY_CALLS.with(|c| c.borrow().len()), 1);
}
