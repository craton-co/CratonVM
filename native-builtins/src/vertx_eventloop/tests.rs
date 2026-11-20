use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::sync::atomic::AtomicI32;
use std::sync::Barrier;
use std::sync::{Mutex as StdMutex, MutexGuard};

// FIX(test-isolation): serialize every test in this module that touches a
// shared process-global (the loop registry, the join-handle map, the id
// counters, the per-mirror group map) on the single module-wide
// `TEST_LOCK`. gcj j3/rt3 residual 5 removed the process-wide dead-thread
// queue, whose colliding per-ctx ids were the reason most of these tests
// took the lock and drained it first: a loop's death is now recorded by
// its own retirement into ITS ctx (`MockNativeThreadBlocker`), so no
// sibling's straggler can reach this ctx's entries. Poisoning is
// recovered (a panicking sibling must not wedge the suite).
//
//   * `VERTX_LOOP_REGISTRY` / `VERTX_LOOP_JOIN_HANDLES`: mutated only via
//     spawn/shutdown, serialized here; tests assert on their own ids.
//   * `NEXT_LOOP_ID` / `LOOP_ID_HWM`: monotonic; tests never assert an
//     exact id, only "> 0" / "rejected past HWM".
//   * `VERTX_GROUPS`: keyed by each test's own object's lock key.
static TEST_LOCK: StdMutex<()> = StdMutex::new(());

/// FIX(test-isolation): acquire the module-wide serialization lock (see
/// above); bind it to a `_guard` local that lives to the end of the test.
#[must_use]
fn isolated_vertx_test() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Poll `ctx` until the entry named `name` reads dead (its loop thread
/// retired it), for up to 1 s.
fn wait_until_retired(ctx: &crate::test_utils::MockNativeContext, name: &str) -> bool {
    let deadline = Instant::now() + Duration::from_millis(1_000);
    loop {
        let alive = ctx
            .registered_native_threads()
            .iter()
            .find(|e| e.0 == name)
            .map(|e| e.2)
            .unwrap_or(true);
        if !alive {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn default_event_loop_has_no_registered_override() {
    let mut registry = NativeMethodRegistry::new();
    register_vertx_eventloop_natives(&mut registry);
    assert_eq!(
        registry.kind_of(CLS_DEFAULT_EVENT_LOOP, "execute", "(Ljava/lang/Runnable;)V"),
        None,
    );
}

// -----------------------------------------------------------------------
// T19.6-v1: vertx_init allocates loops and sets fields
// -----------------------------------------------------------------------
#[test]
fn vertx_init_allocates_loops_and_sets_fields() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let mirror =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    let result = native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(2)]);
    assert!(result.is_ok(), "init must succeed: {:?}", result.err());
    let eid = match ctx.get_field(mirror, VERTX_FIELD_EVENT_LOOP_ID) {
        Value::Long(v) => v,
        other => panic!("expected Long, got {other:?}"),
    };
    assert!(eid > 0, "eventLoopId must be positive, got {eid}");
    let pool = match ctx.get_field(mirror, VERTX_FIELD_POOL_SIZE) {
        Value::Int(v) => v,
        other => panic!("expected Int, got {other:?}"),
    };
    assert_eq!(pool, 2, "pool size must be stored");
    // Clean up.
    native_vertx_close(&mut ctx, &[Value::Object(Some(mirror))]).ok();
}

// -----------------------------------------------------------------------
// T19.6-v2: init rejects invalid pool sizes
// -----------------------------------------------------------------------
#[test]
fn vertx_init_rejects_invalid_pool_size() {
    // FIX(test-isolation): init touches the shared loop registry / id
    // counters on its way to validating the pool size; serialize so a
    // sibling spawn/shutdown can't race the maps mid-call.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let mirror =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    assert!(
        native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(0)]).is_err(),
        "pool size 0 must be rejected"
    );
    assert!(
        native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(-5)]).is_err(),
        "negative pool size must be rejected"
    );
    assert!(
        native_vertx_init(
            &mut ctx,
            &[Value::Object(Some(mirror)), Value::Int(MAX_POOL_SIZE + 1)]
        )
        .is_err(),
        "over-cap pool size must be rejected"
    );
}

// -----------------------------------------------------------------------
// T19.6-v3: getEventLoopId returns stored value
// -----------------------------------------------------------------------
#[test]
fn vertx_get_event_loop_id_returns_stored() {
    let mut ctx = mock_ctx();
    let mirror =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    ctx.set_field(mirror, VERTX_FIELD_EVENT_LOOP_ID, Value::Long(42));
    let res = native_vertx_get_event_loop_id(&mut ctx, &[Value::Object(Some(mirror))]);
    assert_eq!(res.unwrap(), Some(Value::Long(42)));
}

// -----------------------------------------------------------------------
// T19.6-v4: close sets state to TERMINATED
// -----------------------------------------------------------------------
#[test]
fn vertx_close_sets_terminated_state() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let mirror =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(1)]).expect("init");
    native_vertx_close(&mut ctx, &[Value::Object(Some(mirror))]).expect("close");
    let state = match ctx.get_field(mirror, VERTX_FIELD_STATE) {
        Value::Int(v) => v,
        other => panic!("expected Int, got {other:?}"),
    };
    assert_eq!(state, STATE_TERMINATED);    }

// -----------------------------------------------------------------------
// T19.6-v5: NioEventLoop.execute enqueues on the loop (no error)
// -----------------------------------------------------------------------
#[test]
fn nel_execute_enqueues_without_error() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    // Allocate a VertxImpl (starts a loop).
    let vm = try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    native_vertx_init(&mut ctx, &[Value::Object(Some(vm)), Value::Int(1)]).expect("init");
    let eid = match ctx.get_field(vm, VERTX_FIELD_EVENT_LOOP_ID) {
        Value::Long(v) => v,
        other => panic!("expected Long, got {other:?}"),
    };
    // Wire a NioEventLoop mirror to that loop.
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(eid));
    let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
    let res = native_nel_execute(
        &mut ctx,
        &[Value::Object(Some(nel)), Value::Object(Some(runnable))],
    );
    assert!(res.is_ok(), "execute must succeed: {:?}", res.err());
    native_vertx_close(&mut ctx, &[Value::Object(Some(vm))]).ok();
}

// -----------------------------------------------------------------------
// B1 regression: execute() actually drives Runnable.run() (not a no-op).
//
// We can't observe the synthetic-loop side-effect directly in the mock,
// but we CAN prove `native_nel_execute` reached `ctx.invoke_virtual`: if
// it did, the pre-armed one-shot result is consumed (so a *second* read
// sees `None`). If `execute()` still dropped the Runnable, the armed
// result would remain. We arm an Err to additionally assert the
// fire-and-forget contract: a throwing task must NOT fail `execute()`.
// -----------------------------------------------------------------------
#[test]
fn nel_execute_invokes_runnable_and_swallows_task_error() {
    let mut ctx = mock_ctx();
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
    let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
    // Arm the next invoke_virtual (the Runnable.run() call) to throw.
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(RuntimeError::IllegalStateException {
                message: "task boom".to_string(),
            }),
        )));
    }
    let res = native_nel_execute(
        &mut ctx,
        &[Value::Object(Some(nel)), Value::Object(Some(runnable))],
    );
    // Fire-and-forget: a throwing task must not surface to the submitter.
    assert!(
        res.is_ok(),
        "execute() must swallow task error, got {:?}",
        res.err()
    );
    // Proof the Runnable was actually invoked: the one-shot armed result
    // was consumed by `invoke_virtual`, so it is now None.
    let consumed = unsafe { (*ctx.invoke_virtual_result.get()).is_none() };
    assert!(
        consumed,
        "execute() must invoke Runnable.run() (armed invoke_virtual result \
         should have been consumed)"
    );
}

// -----------------------------------------------------------------------
// B1 regression: submit() also drives the Runnable and still returns a
// non-null Future even when the task throws.
// -----------------------------------------------------------------------
#[test]
fn nel_submit_invokes_runnable_and_returns_future_on_task_error() {
    let mut ctx = mock_ctx();
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
    let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(RuntimeError::IllegalStateException {
                message: "task boom".to_string(),
            }),
        )));
    }
    let res = native_nel_submit(
        &mut ctx,
        &[Value::Object(Some(nel)), Value::Object(Some(runnable))],
    );
    assert!(res.is_ok());
    assert!(matches!(res.unwrap(), Some(Value::Object(Some(_)))));
    let consumed = unsafe { (*ctx.invoke_virtual_result.get()).is_none() };
    assert!(consumed, "submit() must invoke Runnable.run()");
}

// -----------------------------------------------------------------------
// T19.6-v6: inEventLoop returns 0 from a non-loop thread
// -----------------------------------------------------------------------
#[test]
fn nel_in_event_loop_false_outside_loop() {
    let mut ctx = mock_ctx();
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
    let res = native_nel_in_event_loop(&mut ctx, &[Value::Object(Some(nel))]);
    assert_eq!(res.unwrap(), Some(Value::Int(0)));
}

// -----------------------------------------------------------------------
// T19.6-v7: isShuttingDown reflects state field
// -----------------------------------------------------------------------
#[test]
fn nel_is_shutting_down_reflects_state() {
    let mut ctx = mock_ctx();
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    for (state, expected) in [
        (STATE_NOT_STARTED, 0),
        (STATE_STARTED, 0),
        (STATE_SHUTTING_DOWN, 1),
        (STATE_TERMINATED, 1),
    ] {
        ctx.set_field(nel, NEL_FIELD_STATE, Value::Int(state));
        let r = native_nel_is_shutting_down(&mut ctx, &[Value::Object(Some(nel))]).unwrap();
        assert_eq!(r, Some(Value::Int(expected)), "state={state}");
    }
}

// -----------------------------------------------------------------------
// T19.6-v8: shutdownGracefully returns non-null Future
// -----------------------------------------------------------------------
#[test]
fn nel_shutdown_gracefully_returns_future() {
    let mut ctx = mock_ctx();
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
    let res = native_nel_shutdown_gracefully(
        &mut ctx,
        &[
            Value::Object(Some(nel)),
            Value::Long(0),
            Value::Long(100),
            Value::Object(None),
        ],
    );
    assert!(res.is_ok());
    assert!(matches!(res.unwrap(), Some(Value::Object(Some(_)))));
    assert_eq!(
        ctx.get_field(nel, NEL_FIELD_STATE),
        Value::Int(STATE_SHUTTING_DOWN)
    );
}

// -----------------------------------------------------------------------
// T19.6-v9: awaitTermination returns 1 only when TERMINATED
// -----------------------------------------------------------------------
#[test]
fn nel_await_termination_reflects_state() {
    let mut ctx = mock_ctx();
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    ctx.set_field(nel, NEL_FIELD_STATE, Value::Int(STATE_STARTED));
    assert_eq!(
        native_nel_await_termination(
            &mut ctx,
            &[
                Value::Object(Some(nel)),
                Value::Long(100),
                Value::Object(None)
            ]
        )
        .unwrap(),
        Some(Value::Int(0))
    );
    ctx.set_field(nel, NEL_FIELD_STATE, Value::Int(STATE_TERMINATED));
    assert_eq!(
        native_nel_await_termination(
            &mut ctx,
            &[
                Value::Object(Some(nel)),
                Value::Long(100),
                Value::Object(None)
            ]
        )
        .unwrap(),
        Some(Value::Int(1))
    );
}

// -----------------------------------------------------------------------
// T19.6-v10: schedule returns non-null ScheduledFuture
// -----------------------------------------------------------------------
#[test]
fn nel_schedule_returns_scheduled_future() {
    let mut ctx = mock_ctx();
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
    let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
    let res = native_nel_schedule(
        &mut ctx,
        &[
            Value::Object(Some(nel)),
            Value::Object(Some(runnable)),
            Value::Long(50),
            Value::Object(None),
        ],
    );
    assert!(res.is_ok());
    assert!(matches!(res.unwrap(), Some(Value::Object(Some(_)))));
}

// -----------------------------------------------------------------------
// T19.6-v11: submit returns non-null Future
// -----------------------------------------------------------------------
#[test]
fn nel_submit_returns_future() {
    let mut ctx = mock_ctx();
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    ctx.set_field(nel, NEL_FIELD_EVENT_LOOP_ID, Value::Long(0));
    let runnable = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runnable", 0).unwrap();
    let res = native_nel_submit(
        &mut ctx,
        &[Value::Object(Some(nel)), Value::Object(Some(runnable))],
    );
    assert!(res.is_ok());
    assert!(matches!(res.unwrap(), Some(Value::Object(Some(_)))));
}

// -----------------------------------------------------------------------
// T19.6-v12: execute with null Runnable returns NullPointerException
// -----------------------------------------------------------------------
#[test]
fn nel_execute_null_runnable_errors() {
    let mut ctx = mock_ctx();
    let nel =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_NIO_EVENT_LOOP, NEL_NUM_SLOTS).unwrap();
    let res = native_nel_execute(&mut ctx, &[Value::Object(Some(nel)), Value::Object(None)]);
    assert!(res.is_err(), "null Runnable must error");
}

// -----------------------------------------------------------------------
// T19.6-v13: event loop runs scheduled task
// -----------------------------------------------------------------------
#[test]
fn vertx_loop_runs_scheduled_task() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry,
    // join-handle map and `NEXT_LOOP_ID`; serialize against siblings.
    let _guard = isolated_vertx_test();
    let el = spawn_vertx_event_loop("test-runs-task").expect("spawn");
    let raw_id = el.id as i64;

    // Wait for the loop thread to record its id.
    for _ in 0..200 {
        if el.loop_thread_id.load(Ordering::Acquire) != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    let ran = Arc::new(AtomicBool::new(false));
    let r2 = Arc::clone(&ran);
    let barrier = Arc::new(Barrier::new(2));
    let b2 = Arc::clone(&barrier);
    el.schedule_task(Box::new(move || {
        r2.store(true, Ordering::Release);
        b2.wait();
    }))
    .unwrap_or_else(|_| panic!("schedule failed"));
    barrier.wait();
    assert!(ran.load(Ordering::Acquire), "task must run in loop thread");

    shutdown_vertx_loop(raw_id);
}

// -----------------------------------------------------------------------
// T19.6-v14: lookup_vertx_loop rejects negative / zero / too-large ids
// -----------------------------------------------------------------------
#[test]
fn lookup_rejects_invalid_ids() {
    // FIX(test-isolation): `lookup_vertx_loop` reads the shared
    // `LOOP_ID_HWM`; serialize so a concurrent sibling spawn bumping the
    // high-water mark can't perturb the bounds this test probes.
    let _guard = isolated_vertx_test();
    assert!(lookup_vertx_loop(-1).is_none(), "negative id must be None");
    assert!(lookup_vertx_loop(0).is_none(), "zero id must be None");
    assert!(
        lookup_vertx_loop(i64::MAX).is_none(),
        "huge id must be None"
    );
}

// -----------------------------------------------------------------------
// T19.6-v15: panic in task does not kill the loop
// -----------------------------------------------------------------------
#[test]
fn loop_survives_panicking_task() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry,
    // join-handle map and `NEXT_LOOP_ID`; serialize against siblings.
    let _guard = isolated_vertx_test();
    let el = spawn_vertx_event_loop("test-panic").expect("spawn");
    let raw_id = el.id as i64;
    for _ in 0..200 {
        if el.loop_thread_id.load(Ordering::Acquire) != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    el.schedule_task(Box::new(|| panic!("intentional test panic")))
        .unwrap_or_else(|_| panic!("schedule panic failed"));
    thread::sleep(Duration::from_millis(50));

    // Loop must still be alive.
    let ran = Arc::new(AtomicBool::new(false));
    let r2 = Arc::clone(&ran);
    el.schedule_task(Box::new(move || r2.store(true, Ordering::Release)))
        .unwrap_or_else(|_| panic!("post-panic schedule failed"));

    let deadline = Instant::now() + Duration::from_millis(300);
    while !ran.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(ran.load(Ordering::Acquire), "loop must survive task panic");
    assert!(el.stats.panic_count.load(Ordering::Acquire) >= 1);

    shutdown_vertx_loop(raw_id);
}

// -----------------------------------------------------------------------
// T19.6-v16: DispatchStats counters increment
// -----------------------------------------------------------------------
#[test]
fn dispatch_stats_increment() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let el = spawn_vertx_event_loop("test-stats").expect("spawn");
    let raw_id = el.id as i64;
    for _ in 0..200 {
        if el.loop_thread_id.load(Ordering::Acquire) != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    let barrier = Arc::new(Barrier::new(2));
    let b2 = Arc::clone(&barrier);
    for _ in 0..4 {
        el.schedule_task(Box::new(|| {})).ok();
    }
    el.schedule_task(Box::new(move || {
        b2.wait();
    }))
    .ok();
    barrier.wait();

    // FIX(test-isolation): `barrier.wait()` only proves the 5th task has
    // STARTED (it is parked in `b2.wait()`); the loop bumps `tasks_run`
    // when each task body RETURNS, so the 5th task's count lands slightly
    // after the rendezvous. Under parallel CPU load that lag can leave
    // `tasks_run` momentarily at 4. Poll for the eventual condition with a
    // generous timeout instead of asserting instantly — same assertion
    // intent (>=5 tasks run, >=1 select cycle), just not racing dispatch.
    let deadline = Instant::now() + Duration::from_secs(5);
    let snap = loop {
        let snap = el.stats.snapshot();
        if snap.tasks_run >= 5 && snap.select_calls >= 1 {
            break snap;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for dispatch stats: tasks_run={}, select_calls={}",
                snap.tasks_run, snap.select_calls
            );
        }
        thread::sleep(Duration::from_millis(2));
    };
    assert!(snap.tasks_run >= 5, "tasks_run={}", snap.tasks_run);
    assert!(snap.select_calls >= 1, "select_calls={}", snap.select_calls);

    shutdown_vertx_loop(raw_id);
}

// -----------------------------------------------------------------------
// T19.6-v17: 8 threads concurrently schedule without data race
// -----------------------------------------------------------------------
#[test]
fn concurrent_8_thread_schedule_no_race() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry,
    // join-handle map and `NEXT_LOOP_ID`; serialize against siblings.
    let _guard = isolated_vertx_test();
    let el = spawn_vertx_event_loop("test-concurrent").expect("spawn");
    let raw_id = el.id as i64;
    for _ in 0..200 {
        if el.loop_thread_id.load(Ordering::Acquire) != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    let counter = Arc::new(AtomicI32::new(0));
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let el2 = Arc::clone(&el);
        let c2 = Arc::clone(&counter);
        let b2 = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b2.wait();
            for _ in 0..25 {
                let c3 = Arc::clone(&c2);
                let _ = el2.schedule_task(Box::new(move || {
                    c3.fetch_add(1, Ordering::Relaxed);
                }));
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }

    let deadline = Instant::now() + Duration::from_millis(500);
    while counter.load(Ordering::Relaxed) < 200 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        counter.load(Ordering::Relaxed) > 0,
        "no concurrent tasks ran"
    );

    shutdown_vertx_loop(raw_id);
}

// =======================================================================
// T19_K2 — Vert.x event-loop VM ThreadRegistry integration tests.
// =======================================================================

/// **T19_K2-1** — `spawn_vertx_event_loop_with_ctx` calls
/// `register_native_thread` exactly once with the right name and
/// `daemon == false`, so `wait_for_non_daemon_threads` will wait
/// for the loop.
#[test]
fn t19_k2_spawn_with_ctx_registers_as_non_daemon() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let before = ctx.registered_native_threads().len();
    let el =
        spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-1", false).expect("spawn_with_ctx");
    let after = ctx.registered_native_threads().len();
    assert_eq!(
        after - before,
        1,
        "exactly one register_native_thread call expected"
    );
    let last = ctx.registered_native_threads().last().cloned().unwrap();
    assert_eq!(last.0, "k2-test-1", "registered name");
    assert!(!last.1, "must register as NON-daemon (false)");
    assert!(last.2, "must register as alive");
    // vm_thread_id should be the id assigned by the mock (>= 1).
    assert!(
        el.vm_thread_id.load(Ordering::Acquire) > 0,
        "vm_thread_id should be set"
    );
    // Cleanup: the loop thread retires its own entry on its way out.
    shutdown_vertx_loop(el.id as i64);
    assert!(wait_until_retired(&ctx, "k2-test-1"));
}

/// **T19_K2-2** — `register_native_thread` returns a non-zero id
/// in the production path; loop's `vm_thread_id` mirrors it.
#[test]
fn t19_k2_vm_thread_id_assigned_on_spawn_with_ctx() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-2", false).expect("spawn");
    let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
    assert_ne!(vm_tid, 0, "vm_thread_id must be assigned");
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K2-3** — gcj j3/rt3 residual 5: the exit trampoline retires the
/// loop's own VM thread through its blocker, once, naming its
/// `vm_thread_id` (it used to queue the id on a process-wide queue).
#[test]
fn t19_k2_loop_exit_retires_its_vm_thread() {
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-3", false).expect("spawn");
    let expected_vm_tid = el.vm_thread_id.load(Ordering::Acquire);
    assert_ne!(expected_vm_tid, 0);
    assert!(
        ctx.retired_native_threads().is_empty(),
        "alive while running"
    );
    shutdown_vertx_loop(el.id as i64);
    assert!(
        wait_until_retired(&ctx, "k2-test-3"),
        "the loop thread must retire vm_thread_id={expected_vm_tid}"
    );
    assert_eq!(ctx.retired_native_threads(), vec![expected_vm_tid], "once");
}

/// **T19_K2-5** — `native_vertx_init` wires through
/// `spawn_vertx_event_loop_with_ctx` so each pool slot registers
/// its own VM thread.
#[test]
fn t19_k2_vertx_init_registers_one_vm_thread_per_pool_slot() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let before = ctx.registered_native_threads().len();
    let mirror =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    let pool = 3;
    native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(pool)])
        .expect("init");
    let after = ctx.registered_native_threads().len();
    assert_eq!(
        after - before,
        pool as usize,
        "init(N) must call register_native_thread N times"
    );
    // Each new entry must be non-daemon.
    let new_entries = ctx.registered_native_threads()[before..].to_vec();
    for (name, daemon, alive) in &new_entries {
        assert!(name.starts_with("vert.x-eventloop-"));
        assert!(!*daemon, "must be non-daemon");
        assert!(*alive);
    }
    // Cleanup.
    native_vertx_close(&mut ctx, &[Value::Object(Some(mirror))]).ok();
}

/// **T19_K2-6** — after `native_vertx_close` every loop thread retires
/// its own entry (gcj j3/rt3 residual 5: nothing has to flush a queue).
#[test]
fn t19_k2_vertx_close_retires_every_loop_thread() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let mirror =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    native_vertx_init(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(2)]).expect("init");
    let before_close = ctx
        .registered_native_threads()
        .iter()
        .filter(|e| e.2)
        .count();
    assert_eq!(before_close, 2, "two alive entries before close");
    // close shuts the loops down; each thread retires itself.
    native_vertx_close(&mut ctx, &[Value::Object(Some(mirror))]).expect("close");
    let deadline = Instant::now() + Duration::from_millis(1_000);
    let mut alive_after = 99usize;
    while Instant::now() < deadline {
        alive_after = ctx
            .registered_native_threads()
            .iter()
            .filter(|e| e.2)
            .count();
        if alive_after == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        alive_after, 0,
        "all threads must be marked dead after close"
    );
}

/// **T19_K2-7** — `spawn_vertx_event_loop` (the legacy variant
/// with no ctx) does NOT register any VM thread. Important for
/// keeping the unit tests deterministic.
#[test]
fn t19_k2_legacy_spawn_does_not_register_vm_thread() {
    // FIX(test-isolation): even the legacy (no-ctx) spawn mutates the
    // shared loop registry / join-handle map / `NEXT_LOOP_ID`, and this
    // test asserts a registered-thread DELTA of zero — a sibling's
    // `_with_ctx` spawn registering on the same mock would not affect this
    // ctx, but spawn/shutdown map contention is still serialized here.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let before = ctx.registered_native_threads().len();
    let el = spawn_vertx_event_loop("k2-test-7-legacy").expect("spawn");
    let after = ctx.registered_native_threads().len();
    assert_eq!(
        after, before,
        "legacy spawn must not call register_native_thread"
    );
    assert_eq!(
        el.vm_thread_id.load(Ordering::Acquire),
        0,
        "legacy spawn leaves vm_thread_id at 0"
    );
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K2-8** — daemon flag wired through correctly when the
/// caller asks for it.
#[test]
fn t19_k2_spawn_with_ctx_daemon_true_registered_as_daemon() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-8", true).expect("spawn");
    let last = ctx.registered_native_threads().last().cloned().unwrap();
    assert_eq!(last.0, "k2-test-8");
    assert!(last.1, "daemon flag must be true");
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K2-9** — a legacy loop (no ctx, `vm_thread_id == 0`) has no
/// blocker, so its exit retires nothing.
#[test]
fn t19_k2_legacy_loop_exit_retires_nothing() {
    let _guard = isolated_vertx_test();
    let ctx = mock_ctx();
    let el = spawn_vertx_event_loop("k2-test-9-legacy").expect("spawn");
    assert_eq!(el.vm_thread_id.load(Ordering::Acquire), 0);
    shutdown_vertx_loop(el.id as i64);
    thread::sleep(Duration::from_millis(50));
    assert!(ctx.retired_native_threads().is_empty());
}

/// **T19_K2-10** — Multiple concurrent VertxImpl instances each
/// register their own pool of non-daemon VM threads (no leakage,
/// no races).
#[test]
fn t19_k2_multiple_vertx_instances_register_independently() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let before = ctx.registered_native_threads().len();
    let mirror_a =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    let mirror_b =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    native_vertx_init(&mut ctx, &[Value::Object(Some(mirror_a)), Value::Int(2)])
        .expect("init A");
    native_vertx_init(&mut ctx, &[Value::Object(Some(mirror_b)), Value::Int(3)])
        .expect("init B");
    let after = ctx.registered_native_threads().len();
    assert_eq!(after - before, 5, "2 + 3 = 5 new VM threads");
    // All 5 must be marked non-daemon + alive.
    for entry in &ctx.registered_native_threads()[before..] {
        assert!(!entry.1, "non-daemon");
        assert!(entry.2, "alive");
    }
    // Cleanup.
    native_vertx_close(&mut ctx, &[Value::Object(Some(mirror_a))]).ok();
    native_vertx_close(&mut ctx, &[Value::Object(Some(mirror_b))]).ok();
}

/// **T19_K2-11** — `current_vertx_loop()` resolves the running
/// loop from inside its body — proxy for "Thread.currentThread"
/// would resolve correctly since the OS thread is dedicated.
#[test]
fn t19_k2_current_vertx_loop_resolves_inside_loop_body() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k2-test-11", false).expect("spawn");
    for _ in 0..200 {
        if el.loop_thread_id.load(Ordering::Acquire) != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let observed_id = Arc::new(AtomicU64::new(0));
    let oid2 = Arc::clone(&observed_id);
    let barrier = Arc::new(Barrier::new(2));
    let b2 = Arc::clone(&barrier);
    el.schedule_task(Box::new(move || {
        if let Some(curr) = current_vertx_loop() {
            oid2.store(curr.id, Ordering::Release);            }
        b2.wait();
    }))
    .unwrap_or_else(|_| panic!("schedule_task failed"));
    barrier.wait();
    assert_eq!(
        observed_id.load(Ordering::Acquire),
        el.id,
        "current_vertx_loop in loop body must resolve to the right loop"
    );
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K2-12** — gcj j3/rt3 residual 5: a second `retire` of the same
/// thread does nothing (the VM's is idempotent; so is the mock's).
#[test]
fn t19_k2_retire_is_idempotent() {
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let id = ctx.register_native_thread("k2-test-12", false, 0);
    let blocker = ctx
        .native_thread_blocker(id)
        .expect("a registered id has a blocker");
    assert!(blocker.retire(), "first retire records the death");
    assert!(!blocker.retire(), "second does nothing");
    assert_eq!(ctx.retired_native_threads(), vec![id]);
    assert!(
        ctx.native_thread_blocker(0).is_none(),
        "id 0 is not VM-tracked"
    );
}

// =======================================================================
// T19_K4 — Java Thread mirror linkage tests.
//
// K2 left `java_thread_obj` at None on the registry entry; K4 closes
// the gap by allocating a synthetic `java.lang.Thread` mirror at
// spawn time and threading it through `set_native_thread_java_obj`.
// These tests assert the mirror is allocated, named, linked, and
// accessible through the new APIs.
// =======================================================================

/// **T19_K4-1** — `spawn_vertx_event_loop_with_ctx` calls
/// `set_native_thread_java_obj` exactly once with a non-null
/// mirror after `register_native_thread`.
#[test]
fn t19_k4_spawn_with_ctx_registers_java_thread_mirror() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el =
        spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-test-1", false).expect("spawn_with_ctx");
    let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
    assert_ne!(vm_tid, 0, "vm_thread_id must be set");
    // The mock should have recorded the mirror pointer.
    let mirror_ptr = ctx.native_thread_java_obj_ptr(vm_tid);
    assert_ne!(
        mirror_ptr, 0,
        "set_native_thread_java_obj must have been called with a non-null mirror"
    );
    // And the loop's own field must mirror the same value.
    assert_eq!(
        mirror_ptr,
        el.java_thread_mirror_ptr.load(Ordering::Acquire),
        "VertxEventLoop.java_thread_mirror_ptr must equal the registered mirror"
    );
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K4-2** — The Thread mirror's `name` field is set to the
/// loop's name string (round-tripped through ctx.create_string).
#[test]
fn t19_k4_thread_mirror_name_field_set_to_loop_name() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-name-test", false).expect("spawn");
    let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
    let mirror_ptr_usize = ctx.native_thread_java_obj_ptr(vm_tid);
    assert_ne!(mirror_ptr_usize, 0);
    let mirror = unsafe { ObjectRef::from_raw(mirror_ptr_usize as *mut u8) };
    let name_obj = match ctx.get_field(mirror, crate::SYNTHETIC_THREAD_MIRROR_NAME_SLOT) {
        Value::Object(Some(o)) => o,
        other => panic!("name slot must hold a String ref, got {other:?}"),
    };
    let read = ctx.read_string(name_obj).expect("string");
    assert_eq!(
        read, "k4-name-test",
        "Thread mirror name must equal the loop name"
    );
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K4-3** — The Thread mirror's `tid` field is set to the
/// VM ThreadId so reflection / Thread.threadId() reads see a
/// stable id without going through the registry.
#[test]
fn t19_k4_thread_mirror_tid_field_matches_vm_thread_id() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-tid-test", false).expect("spawn");
    let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
    assert_ne!(vm_tid, 0);
    let mirror_ptr = ctx.native_thread_java_obj_ptr(vm_tid);
    let mirror = unsafe { ObjectRef::from_raw(mirror_ptr as *mut u8) };
    match ctx.get_field(mirror, crate::SYNTHETIC_THREAD_MIRROR_TID_SLOT) {
        Value::Long(t) => assert_eq!(t as u64, vm_tid, "tid slot must equal vm_thread_id"),
        other => panic!("tid slot must hold a Long, got {other:?}"),
    }
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K4-4** — Each pool slot in `VertxImpl.init(N)` gets its
/// own Thread mirror with a unique name. This is the "no name
/// aliasing" check: 5 loops must have 5 distinct mirror
/// pointers.
#[test]
fn t19_k4_vertx_init_creates_distinct_mirrors_per_slot() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let mirror_obj =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    let pool: i32 = 5;
    let before_thread_count = ctx.registered_native_threads().len();
    native_vertx_init(
        &mut ctx,
        &[Value::Object(Some(mirror_obj)), Value::Int(pool)],
    )
    .expect("init");
    let after_thread_count = ctx.registered_native_threads().len();
    assert_eq!(after_thread_count - before_thread_count, pool as usize);
    // Collect the registered tids and assert each has a distinct mirror.
    let mut mirror_ptrs = std::collections::HashSet::new();
    // mock allocates contiguous tids starting at 1 (or wherever
    // it was after previous tests). Walk the new range.
    for i in 0..pool {
        // FIX(test-isolation): tids are no longer 1-based — MockNativeContext
        // now assigns globally-unique tids from `native_tid_base`, so the
        // k-th registration has tid `native_tid_base + k` (read it back
        // rather than assuming a base of 1).
        let entries = ctx.registered_native_threads();
        let slot = before_thread_count + i as usize;
        let entry = &entries[slot];
        let actual_tid = ctx.native_tid_base + slot as u64;
        assert_eq!(entry.0, format!("vert.x-eventloop-{i}"));
        let ptr = ctx.native_thread_java_obj_ptr(actual_tid);
        assert_ne!(ptr, 0, "slot {i} must have a mirror");
        assert!(mirror_ptrs.insert(ptr), "slot {i} mirror must be unique");
    }
    native_vertx_close(&mut ctx, &[Value::Object(Some(mirror_obj))]).ok();
}

/// **T19_K4-5** — Legacy `spawn_vertx_event_loop` (no ctx) does
/// NOT allocate a mirror — `java_thread_mirror_ptr` stays 0.
/// Important so unit tests that use the legacy path don't
/// accidentally pollute the K4 mock state.
#[test]
fn t19_k4_legacy_spawn_does_not_allocate_mirror() {
    // FIX(test-isolation): legacy spawn/shutdown still mutate the shared
    // loop registry / join-handle map / `NEXT_LOOP_ID`; serialize.
    let _guard = isolated_vertx_test();
    let el = spawn_vertx_event_loop("k4-legacy-spawn").expect("spawn");
    assert_eq!(
        el.java_thread_mirror_ptr.load(Ordering::Acquire),
        0,
        "legacy spawn must leave java_thread_mirror_ptr at 0"
    );
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K4-6** — `spawn_vertx_event_loop_with_ctx` registers as
/// non-daemon by default (Vert.x event loops are non-daemon per
/// JDK contract). Combined with a Thread mirror, this is what
/// keeps the process alive past `main()`.
#[test]
fn t19_k4_spawn_default_is_non_daemon_with_mirror() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-non-daemon", false).expect("spawn");
    let entries = ctx.registered_native_threads();
    let last = entries.last().unwrap();
    assert_eq!(last.0, "k4-non-daemon");
    assert!(!last.1, "must be non-daemon");
    assert!(last.2, "must be alive");
    let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
    let ptr = ctx.native_thread_java_obj_ptr(vm_tid);
    assert_ne!(ptr, 0, "non-daemon spawn must still attach a mirror");
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K4-7** — Daemon flag set to `true` still allocates a
/// mirror. (Mirror allocation is independent of daemon-ness; the
/// daemon flag only affects whether `wait_for_non_daemon_threads`
/// blocks on us.)
#[test]
fn t19_k4_daemon_spawn_still_attaches_mirror() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-daemon-true", true).expect("spawn");
    let vm_tid = el.vm_thread_id.load(Ordering::Acquire);
    let ptr = ctx.native_thread_java_obj_ptr(vm_tid);
    assert_ne!(ptr, 0, "daemon=true must still attach a mirror");
    let entries = ctx.registered_native_threads();
    let last = entries.last().unwrap();
    assert!(last.1, "must be daemon");
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K4-8** — `set_native_thread_java_obj` rejects an unknown
/// thread id. Defends against a hostile native that obtains a
/// mirror reference and tries to forge a registration.
#[test]
fn t19_k4_set_native_thread_java_obj_rejects_unknown_id() {
    let mut ctx = mock_ctx();
    let mirror = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
    // Bogus thread id past the high-water mark.
    let res = ctx.set_native_thread_java_obj(u64::MAX / 2, mirror);
    assert!(!res, "unknown thread id must return false");
    // Zero is also rejected.
    let res = ctx.set_native_thread_java_obj(0, mirror);
    assert!(!res, "zero thread id must return false");
}

/// **T19_K4-9** — Mirror pointer is stable across loop lifetime:
/// re-reading `java_thread_mirror_ptr` returns the same value
/// before and after the loop has scheduled some work. (No
/// reallocation, no GC churn.)
#[test]
fn t19_k4_mirror_ptr_stable_over_loop_lifetime() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el = spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-stable-ptr", false).expect("spawn");
    let initial = el.java_thread_mirror_ptr.load(Ordering::Acquire);
    assert_ne!(initial, 0);
    // Wait for loop to claim its thread id so the next checks
    // see a fully-running loop.
    for _ in 0..200 {
        if el.loop_thread_id.load(Ordering::Acquire) != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    // Schedule a few tasks.
    for _ in 0..3 {
        let _ = el.schedule_task(Box::new(|| {}));
    }
    thread::sleep(Duration::from_millis(20));
    let after = el.java_thread_mirror_ptr.load(Ordering::Acquire);
    assert_eq!(initial, after, "mirror_ptr must be stable across activity");
    shutdown_vertx_loop(el.id as i64);
}

/// **T19_K4-10** — Multiple VertxImpl instances each get distinct
/// mirrors per slot — no leakage between groups.
#[test]
fn t19_k4_multiple_vertx_groups_have_disjoint_mirrors() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let m_a =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    let m_b =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_VERTX_IMPL, VERTX_NUM_SLOTS).unwrap();
    let before = ctx.registered_native_threads().len();
    native_vertx_init(&mut ctx, &[Value::Object(Some(m_a)), Value::Int(2)]).expect("init A");
    native_vertx_init(&mut ctx, &[Value::Object(Some(m_b)), Value::Int(2)]).expect("init B");
    let after = ctx.registered_native_threads().len();
    assert_eq!(after - before, 4);
    // Collect mirror pointers for the 4 new entries.
    let mut ptrs = Vec::new();
    for i in 0..4 {
        // FIX(test-isolation): tids start at native_tid_base, not 1.
        let tid = ctx.native_tid_base + (before + i) as u64;
        let p = ctx.native_thread_java_obj_ptr(tid);
        assert_ne!(p, 0, "entry {i} must have a mirror");
        ptrs.push(p);
    }
    // All 4 must be distinct.
    let unique: std::collections::HashSet<_> = ptrs.iter().copied().collect();
    assert_eq!(
        unique.len(),
        4,
        "all 4 mirrors must be distinct: {:?}",
        ptrs
    );
    native_vertx_close(&mut ctx, &[Value::Object(Some(m_a))]).ok();
    native_vertx_close(&mut ctx, &[Value::Object(Some(m_b))]).ok();
}

/// **T19_K4-11** — the loop's retirement after shutdown
/// does NOT clear the mirror pointer recorded on the loop. The
/// loop's mirror_ptr is stable for the loop's lifetime; the
/// registry's `is_alive` flips, but the mirror reference stays
/// (so any caller still holding the loop ref can read the
/// mirror).
#[test]
fn t19_k4_shutdown_does_not_clear_mirror_ptr() {
    // FIX(test-isolation): spawn/shutdown mutate the shared loop registry; serialize.
    let _guard = isolated_vertx_test();
    let mut ctx = mock_ctx();
    let el =
        spawn_vertx_event_loop_with_ctx(&mut ctx, "k4-shutdown-keep", false).expect("spawn");
    let mirror_before = el.java_thread_mirror_ptr.load(Ordering::Acquire);
    assert_ne!(mirror_before, 0);
    shutdown_vertx_loop(el.id as i64);
    // After shutdown the loop's vm_thread_id is still set; mirror_ptr
    // too. Only the registry's alive flag flips.
    assert!(wait_until_retired(&ctx, "k4-shutdown-keep"));
    let mirror_after = el.java_thread_mirror_ptr.load(Ordering::Acquire);
    assert_eq!(
        mirror_before, mirror_after,
        "mirror_ptr must not be reset on shutdown"
    );
}

/// Interpreter round i1 wave 19, lane L3 (i9-L1 item 4): a carrier run
/// with the VM's native-thread blocker publishes its OS thread id once and
/// parks inside the blocker -- every `enter_blocked` matched by a
/// `leave_blocked`, which is where a JDWP suspension holds the thread --
/// as the XNIO loop selects inside it.
#[test]
fn a_registered_carrier_parks_inside_its_native_thread_blocker() {
    use std::sync::atomic::AtomicUsize;
    #[derive(Default)]
    struct Counting {
        published: AtomicUsize,
        entered: AtomicUsize,
        left: AtomicUsize,
    }
    impl NativeThreadBlocker for Counting {
        fn publish_os_tid(&self) {
            self.published.fetch_add(1, Ordering::SeqCst);
        }
        fn enter_blocked(&self) {
            self.entered.fetch_add(1, Ordering::SeqCst);
        }
        fn leave_blocked(&self) {
            self.left.fetch_add(1, Ordering::SeqCst);
        }
    }
    let counting = Arc::new(Counting::default());
    let blocker: Arc<dyn NativeThreadBlocker> = counting.clone();
    let el = VertxEventLoop::new(u64::MAX - 19, "l3-blocker-carrier");
    let el_for_thread = Arc::clone(&el);
    let carrier =
        thread::spawn(move || run_vertx_event_loop_with_blocker(el_for_thread, Some(blocker)));
    let deadline = Instant::now() + Duration::from_secs(10);
    while counting.entered.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    el.shutdown();
    assert!(carrier.join().is_ok(), "the carrier exits on shutdown");
    assert_eq!(counting.published.load(Ordering::SeqCst), 1);
    let entered = counting.entered.load(Ordering::SeqCst);
    assert!(entered >= 1, "the idle carrier parked inside the blocker");
    assert_eq!(
        counting.left.load(Ordering::SeqCst),
        entered,
        "every park left"
    );
}
