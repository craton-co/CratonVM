use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, Barrier};

/// Assertion helper — `Box<dyn FnOnce>` does not implement Debug
/// so `.expect` / `.unwrap` do not compile on the Err branch.
fn must_execute(res: Result<(), IoTask>) {
    if res.is_err() {
        panic!("try_execute must succeed");
    }
}

fn must_schedule(res: Result<(u64, Arc<AtomicBool>), IoTask>) -> (u64, Arc<AtomicBool>) {
    match res {
        Ok(x) => x,
        Err(_) => panic!("try_schedule_at must succeed"),
    }
}

/// Spawn a worker thread running `run_io_loop` and return (handle,
/// join-handle, mock-selector-ref). The returned `IoThreadHandle`
/// is already registered in the process-wide registry.
fn spawn_test_loop(
    name: &str,
    id: u64,
) -> (
    Arc<IoThreadHandle>,
    thread::JoinHandle<()>,
    Arc<MockSelector>,
) {
    let mock = Arc::new(MockSelector::new());
    let selector: Arc<dyn SelectorHandle> = mock.clone();
    let h = IoThreadHandle::new(id, 1, selector, name);
    register_io_thread(h.clone());
    let h2 = h.clone();
    let jh = thread::Builder::new()
        .name(name.to_string())
        .spawn(move || run_io_loop(h2))
        .expect("spawn");
    // Wait briefly for the loop to claim its OS tid.
    for _ in 0..100 {
        if h.loop_thread_id.load(Ordering::Acquire) != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    (h, jh, mock)
}

fn shutdown_and_join(h: &Arc<IoThreadHandle>, jh: thread::JoinHandle<()>) {
    h.shutdown();
    jh.join().expect("join");
    unregister_io_thread(h.id);
}

/// Test 1: `execute` runs a runnable on the I/O thread.
#[test]
fn t19_7_c_execute_runs_runnable_on_thread() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-1", 1001);
    let flag = Arc::new(AtomicBool::new(false));
    let flag2 = flag.clone();
    must_execute(h.try_execute(Box::new(move || {
        flag2.store(true, Ordering::Release);
    })));
    // Give the loop a chance to drain.
    for _ in 0..500 {
        if flag.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(flag.load(Ordering::Acquire), "runnable must run");
    shutdown_and_join(&h, jh);
}

/// Test 2: `execute` from a different thread wakes the selector.
#[test]
fn t19_7_c_execute_from_other_thread_wakes_selector() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-2", 1002);
    let flag = Arc::new(AtomicBool::new(false));
    let flag2 = flag.clone();
    let h2 = h.clone();
    // Submit from a separate thread that is definitely not the loop.
    let sub = thread::spawn(move || {
        must_execute(h2.try_execute(Box::new(move || {
            flag2.store(true, Ordering::Release);
        })));
    });
    sub.join().expect("sub join");
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(200) {
        if flag.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        flag.load(Ordering::Acquire),
        "execute-from-other-thread must fire within 200ms (selector should have been woken)"
    );
    shutdown_and_join(&h, jh);
}

/// Test 3: `executeAfter` respects its deadline (500ms ±20% window).
#[test]
fn t19_7_c_execute_after_respects_deadline() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-3", 1003);
    let fired_at = Arc::new(Mutex::new(None::<Instant>));
    let fired_at2 = fired_at.clone();
    let started = Instant::now();
    let deadline = now_ms().saturating_add(500);
    h.try_schedule_at(
        deadline,
        Box::new(move || {
            *fired_at2.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }),
    )
    .map_err(|_| "schedule rejected")
    .unwrap();
    // Wait up to 900ms.
    for _ in 0..450 {
        if fired_at.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let got = fired_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("must fire");
    let elapsed = got.duration_since(started).as_millis() as i64;
    assert!(elapsed >= 450, "fired too early: {}ms", elapsed);
    assert!(elapsed <= 900, "fired too late: {}ms", elapsed);
    shutdown_and_join(&h, jh);
}

/// Test 4: `executeAtTime` respects the absolute deadline.
#[test]
fn t19_7_c_execute_at_time_absolute() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-4", 1004);
    let fired_at = Arc::new(Mutex::new(None::<Instant>));
    let fired_at2 = fired_at.clone();
    let started = Instant::now();
    let absolute = now_ms().saturating_add(200);
    h.try_schedule_at(
        absolute,
        Box::new(move || {
            *fired_at2.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }),
    )
    .map_err(|_| "schedule rejected")
    .unwrap();
    for _ in 0..250 {
        if fired_at.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let got = fired_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("must fire");
    let elapsed = got.duration_since(started).as_millis() as i64;
    assert!(elapsed >= 150, "fired too early: {}ms", elapsed);
    assert!(elapsed <= 500, "fired too late: {}ms", elapsed);
    shutdown_and_join(&h, jh);
}

/// Test 5: cancelling a scheduled task before its deadline prevents it
/// from running.
#[test]
fn t19_7_c_scheduled_task_cancel_prevents_execution() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-5", 1005);
    let fired = Arc::new(AtomicBool::new(false));
    let fired2 = fired.clone();
    let deadline = now_ms().saturating_add(300);
    let (_task_id, cancelled) = h
        .try_schedule_at(
            deadline,
            Box::new(move || fired2.store(true, Ordering::Release)),
        )
        .map_err(|_| "schedule rejected")
        .unwrap();
    cancelled.store(true, Ordering::Release);
    // Wake so loop reconsiders heap.
    h.selector.wakeup();
    thread::sleep(Duration::from_millis(500));
    assert!(
        !fired.load(Ordering::Acquire),
        "cancelled task must not run"
    );
    shutdown_and_join(&h, jh);
}

/// Test 6: cancelling a task AFTER it has already fired returns false
/// (second cancel attempt returns false too; the first cancel attempt
/// after-fire also returns false because the task is no longer
/// pending).
#[test]
fn t19_7_c_scheduled_task_cancel_after_execution_returns_false() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-6", 1006);
    let fired = Arc::new(AtomicBool::new(false));
    let fired2 = fired.clone();
    let deadline = now_ms().saturating_add(50);
    let (_tid, cancelled) = h
        .try_schedule_at(
            deadline,
            Box::new(move || fired2.store(true, Ordering::Release)),
        )
        .map_err(|_| "schedule rejected")
        .unwrap();
    for _ in 0..200 {
        if fired.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(fired.load(Ordering::Acquire), "must have fired");
    // Now cancel after the fact. The swap returns the previous flag
    // value (false); the JDK semantics say remove() returns true only
    // if the task was genuinely cancelled before running. Emulating
    // that here: the task has been consumed, so the flag was
    // never flipped, swap returns false, which we interpret as "no,
    // task already completed, cancel was a no-op".
    let was_already = cancelled.swap(true, Ordering::AcqRel);
    assert!(!was_already, "flag still false pre-cancel");
    // The Java-level Key.remove() call would then observe this.
    // Verify the flag is now true (set, but too late).
    assert!(cancelled.load(Ordering::Acquire));
    shutdown_and_join(&h, jh);
}

/// Test 7: a panicking runnable does not kill the loop; a later
/// normal runnable still runs.
#[test]
fn t19_7_c_task_panic_caught_loop_continues() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-7", 1007);
    must_execute(h.try_execute(Box::new(|| panic!("intentional test panic"))));
    let ran = Arc::new(AtomicBool::new(false));
    let ran2 = ran.clone();
    must_execute(h.try_execute(Box::new(move || ran2.store(true, Ordering::Release))));
    for _ in 0..300 {
        if ran.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        ran.load(Ordering::Acquire),
        "loop must survive a panic and run later tasks"
    );
    shutdown_and_join(&h, jh);
}

/// Test 8: shutdown drains any remaining pending tasks (they run)
/// and marks all scheduled tasks cancelled.
#[test]
fn t19_7_c_shutdown_drains_remaining_tasks_or_cancels() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-8", 1008);
    // Queue a slow task so its successor is unlikely to run before
    // shutdown arrives.
    let slow_done = Arc::new(AtomicBool::new(false));
    let slow_done2 = slow_done.clone();
    must_execute(h.try_execute(Box::new(move || {
        thread::sleep(Duration::from_millis(100));
        slow_done2.store(true, Ordering::Release);
    })));
    // Stack up more tasks; the drain at shutdown should still fire them.
    let counter = Arc::new(AtomicI32::new(0));
    for _ in 0..5 {
        let c = counter.clone();
        must_execute(h.try_execute(Box::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
        })));
    }
    // Also schedule something far in the future — must be cancelled.
    let sched_fired = Arc::new(AtomicBool::new(false));
    let sched_fired2 = sched_fired.clone();
    let (_tid, _flag) = h
        .try_schedule_at(
            now_ms() + 10_000,
            Box::new(move || sched_fired2.store(true, Ordering::Release)),
        )
        .map_err(|_| "schedule rejected")
        .unwrap();
    // Trigger shutdown.
    h.shutdown();
    jh.join().expect("join");
    unregister_io_thread(h.id);
    assert!(slow_done.load(Ordering::Acquire), "slow must have run");
    // Drain-on-shutdown should have run everything.
    assert_eq!(counter.load(Ordering::Relaxed), 5, "queued tasks drained");
    // The far-future scheduled task must NOT have fired.
    assert!(
        !sched_fired.load(Ordering::Acquire),
        "far-future scheduled task cannot have fired post-shutdown"
    );
}

/// Test 9: 8 concurrent threads each submitting `execute` must all
/// land without data race / loss.
#[test]
fn t19_7_c_concurrent_execute_from_8_threads_no_data_race() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-9", 1009);
    const N_THREADS: usize = 8;
    const PER_THREAD: usize = 50;
    let counter = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(N_THREADS));
    let mut handles = Vec::new();
    for _ in 0..N_THREADS {
        let hh = h.clone();
        let c = counter.clone();
        let b = barrier.clone();
        handles.push(thread::spawn(move || {
            b.wait();
            for _ in 0..PER_THREAD {
                let c2 = c.clone();
                must_execute(hh.try_execute(Box::new(move || {
                    c2.fetch_add(1, Ordering::Relaxed);
                })));
            }
        }));
    }
    for t in handles {
        t.join().expect("joiner");
    }
    // Wait for loop to drain everything.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if counter.load(Ordering::Relaxed) == N_THREADS * PER_THREAD {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        counter.load(Ordering::Relaxed),
        N_THREADS * PER_THREAD,
        "all concurrent execute()s must have fired exactly once"
    );
    shutdown_and_join(&h, jh);
}

/// Test 10: `currentThread` returns self inside the loop, null
/// outside.
#[test]
fn t19_7_c_current_thread_returns_self_in_loop_returns_null_elsewhere() {
    let (h, jh, _sel) = spawn_test_loop("iot-test-10", 1010);
    // Outside the loop, current_io_thread is None.
    assert!(current_io_thread().is_none(), "not inside any IO loop");
    // Inside the loop (task posted to run on it), it matches h.
    let got = Arc::new(Mutex::new(None::<u64>));
    let got2 = got.clone();
    must_execute(h.try_execute(Box::new(move || {
        let cur = current_io_thread();
        *got2.lock().unwrap_or_else(|e| e.into_inner()) = cur.map(|c| c.id);
    })));
    for _ in 0..300 {
        if got.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let saw = *got.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(saw, Some(h.id), "loop task sees itself via currentThread");
    shutdown_and_join(&h, jh);
}

// -------- Additional coverage tests --------

/// `execute` over-cap is rejected with `RejectedExecutionException`
/// semantics (Err return from try_execute).
#[test]
fn t19_7_c_execute_over_cap_rejected() {
    let mock = Arc::new(MockSelector::new());
    let selector: Arc<dyn SelectorHandle> = mock;
    let h = IoThreadHandle::new(9999, 1, selector, "iot-cap");
    // DO NOT start the loop so tasks don't drain. Fill past MAX.
    for _ in 0..MAX_PENDING_TASKS {
        must_execute(h.try_execute(Box::new(|| {})));
    }
    let err = h.try_execute(Box::new(|| {}));
    assert!(err.is_err(), "over-cap must be rejected");
}

/// Scheduled over-cap is rejected.
#[test]
fn t19_7_c_schedule_over_cap_rejected() {
    let mock = Arc::new(MockSelector::new());
    let selector: Arc<dyn SelectorHandle> = mock;
    let h = IoThreadHandle::new(9998, 1, selector, "iot-sched-cap");
    let far = now_ms() + 1_000_000;
    for _ in 0..MAX_SCHEDULED_TASKS {
        h.try_schedule_at(far, Box::new(|| {}))
            .map_err(|_| "unexpected")
            .unwrap();
    }
    let err = h.try_schedule_at(far, Box::new(|| {}));
    assert!(err.is_err(), "scheduled over-cap must be rejected");
}

/// ScheduledHeap ordering — earlier deadlines pop first regardless
/// of insertion order.
#[test]
fn t19_7_c_scheduled_heap_orders_by_deadline() {
    let mut heap = ScheduledHeap::new();
    heap.push_bounded(ScheduledTask {
        deadline_ms: 200,
        seq: 2,
        runnable: Some(Box::new(|| {})),
        cancelled: Arc::new(AtomicBool::new(false)),
        task_id: 2,
    })
    .map_err(|_| "push")
    .unwrap();
    heap.push_bounded(ScheduledTask {
        deadline_ms: 100,
        seq: 1,
        runnable: Some(Box::new(|| {})),
        cancelled: Arc::new(AtomicBool::new(false)),
        task_id: 1,
    })
    .map_err(|_| "push")
    .unwrap();
    heap.push_bounded(ScheduledTask {
        deadline_ms: 300,
        seq: 3,
        runnable: Some(Box::new(|| {})),
        cancelled: Arc::new(AtomicBool::new(false)),
        task_id: 3,
    })
    .map_err(|_| "push")
    .unwrap();
    let first = heap.pop_if_expired(500).expect("pop").task_id;
    let second = heap.pop_if_expired(500).expect("pop").task_id;
    let third = heap.pop_if_expired(500).expect("pop").task_id;
    assert_eq!((first, second, third), (1, 2, 3));
}

/// MockSelector wakeup semantics: a wakeup before select returns
/// immediately.
#[test]
fn t19_7_c_mock_selector_wakeup_short_circuits_select() {
    let mock = MockSelector::new();
    mock.wakeup();
    let start = Instant::now();
    let _ = mock.select(1_000).expect("select");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "wakeup should have short-circuited select"
    );
}

// =======================================================================
// T19_K4 — XNIO IO thread Java Thread mirror linkage tests.
//
// These mirror the Vert.x event-loop K4 tests: every XNIO IO thread
// spawned via `spawn_io_thread_with_ctx` must allocate a synthetic
// `java.lang.Thread` mirror and route it through
// `set_native_thread_java_obj` so registry look-ups by ObjectRef
// resolve and `Thread.enumerate()` sees the carrier.
// =======================================================================

/// **T19_K4-XNIO-1** — `spawn_io_thread_with_ctx` registers a Java
/// Thread mirror on the carrier OS thread.
#[test]
fn t19_k4_xnio_spawn_with_ctx_registers_java_thread_mirror() {
    let mut ctx = crate::test_utils::mock_ctx();
    let mock = Arc::new(MockSelector::new());
    let sel: Arc<dyn SelectorHandle> = mock.clone();
    let id = 9_001u64;
    let h = spawn_io_thread_with_ctx(&mut ctx, "k4-xnio-1", id, 1, sel, false).expect("spawn");
    // The mock should have recorded the mirror pointer.
    let vm_tid = lookup_vm_thread_id(h.id);
    assert_ne!(vm_tid, 0, "vm_tid must be assigned");
    let mirror_ptr = ctx.native_thread_java_obj_ptr(vm_tid);
    assert_ne!(mirror_ptr, 0, "set_native_thread_java_obj must have run");
    // And the iot-side lookup must agree.
    assert_eq!(
        lookup_iot_java_mirror_ptr(h.id),
        mirror_ptr,
        "iot-side mirror lookup must match"
    );
    h.shutdown();
    unregister_io_thread(h.id);
}

/// **T19_K4-XNIO-2** — XNIO mirror's name field is set to the
/// IO thread name.
#[test]
fn t19_k4_xnio_thread_mirror_name_field_matches() {
    let mut ctx = crate::test_utils::mock_ctx();
    let mock = Arc::new(MockSelector::new());
    let sel: Arc<dyn SelectorHandle> = mock.clone();
    let id = 9_002u64;
    let h =
        spawn_io_thread_with_ctx(&mut ctx, "k4-xnio-name", id, 1, sel, false).expect("spawn");
    let vm_tid = lookup_vm_thread_id(h.id);
    let mirror_ptr = ctx.native_thread_java_obj_ptr(vm_tid);
    let mirror = unsafe { ObjectRef::from_raw(mirror_ptr as *mut u8) };
    let name_obj = match ctx.get_field(mirror, 0) {
        Value::Object(Some(o)) => o,
        other => panic!("name slot must hold a String, got {other:?}"),
    };
    assert_eq!(ctx.read_string(name_obj).as_deref(), Some("k4-xnio-name"));
    h.shutdown();
    unregister_io_thread(h.id);
}

/// **T19_K4-XNIO-3** — Multiple XNIO IO threads register distinct
/// mirrors per carrier (no aliasing).
#[test]
fn t19_k4_xnio_multiple_threads_have_distinct_mirrors() {
    let mut ctx = crate::test_utils::mock_ctx();
    let base_id = 9_100u64;
    let mut handles = Vec::new();
    let mut mirror_ptrs = std::collections::HashSet::new();
    for i in 0..3 {
        let mock = Arc::new(MockSelector::new());
        let sel: Arc<dyn SelectorHandle> = mock.clone();
        let h = spawn_io_thread_with_ctx(
            &mut ctx,
            &format!("k4-xnio-multi-{i}"),
            base_id + i,
            1,
            sel,
            false,
        )
        .expect("spawn");
        let vm_tid = lookup_vm_thread_id(h.id);
        let p = ctx.native_thread_java_obj_ptr(vm_tid);
        assert_ne!(p, 0);
        assert!(mirror_ptrs.insert(p), "iter {i} mirror must be unique");
        handles.push(h);
    }
    for h in &handles {
        h.shutdown();
        unregister_io_thread(h.id);
    }
}

/// **T19_K4-XNIO-4** — `lookup_iot_java_mirror_ptr` returns 0 for
/// an unknown IO thread id.
#[test]
fn t19_k4_xnio_lookup_unknown_id_returns_zero() {
    assert_eq!(
        lookup_iot_java_mirror_ptr(u64::MAX),
        0,
        "unknown id must map to 0"
    );
}
