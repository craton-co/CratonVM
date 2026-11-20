use super::*;
use std::sync::Barrier;

/// Helper: spawn a loop, run `body` with its handle, then shut down + join.
fn with_loop<F: FnOnce(Arc<EventLoop>)>(name: &str, body: F) {
    let mgr = EventLoopManager::new();
    let el = mgr.spawn(name).expect("spawn");
    // Give the loop a chance to claim its thread id slot.
    for _ in 0..200 {
        if el.loop_thread_id.load(Ordering::Acquire) != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    body(el.clone());
    let _ = mgr.shutdown_and_join(el.id);
}

// Test 1: schedule_task runs the task on the loop's OS thread.
#[test]
fn t19_6_schedule_runs_task_on_loop_thread() {
    with_loop("t19-6-test-1", |el| {
        let observed = Arc::new(Mutex::new(0u64));
        let observed2 = observed.clone();
        let barrier = Arc::new(Barrier::new(2));
        let b2 = barrier.clone();
        el.schedule_task(Box::new(move || {
            let tid = os_thread_id();
            *observed2.lock().unwrap_or_else(|e| e.into_inner()) = tid;
            b2.wait();
        }))
        .expect("schedule");
        barrier.wait();
        let tid = *observed.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(tid, el.loop_thread_id.load(Ordering::Acquire));
    });
}

// Test 2: a timer fires within ~10 ms of its deadline (relaxed
// upper bound for slow test runners).
#[test]
fn t19_6_timer_fires_within_window() {
    with_loop("t19-6-test-2", |el| {
        let fired_at = Arc::new(Mutex::new(None::<Instant>));
        let fired2 = fired_at.clone();
        let scheduled_at = Instant::now();
        let deadline = scheduled_at + Duration::from_millis(50);
        let _ = el.schedule_timer(
            deadline,
            Box::new(move || {
                *fired2.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            }),
        );
        // Wait up to 500 ms.
        for _ in 0..250 {
            if fired_at.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let got = fired_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("timer should fire");
        assert!(got >= scheduled_at + Duration::from_millis(40));
        assert!(got <= scheduled_at + Duration::from_millis(500));
    });
}

// Test 3: cross-thread schedule_task wakes the parked loop.
#[test]
fn t19_6_cross_thread_wake_delivered() {
    with_loop("t19-6-test-3", |el| {
        let flag = Arc::new(AtomicBool::new(false));
        let f2 = flag.clone();
        let el2 = el.clone();
        let jh = thread::spawn(move || {
            el2.schedule_task(Box::new(move || {
                f2.store(true, Ordering::Release);
            }))
            .expect("schedule");
        });
        jh.join().expect("join submit");
        for _ in 0..250 {
            if flag.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            flag.load(Ordering::Acquire),
            "cross-thread wake must fire within 500ms"
        );
    });
}

// Test 4: panicking task doesn't kill the loop.
#[test]
fn t19_6_panic_in_task_does_not_kill_loop() {
    with_loop("t19-6-test-4", |el| {
        let _ = el.schedule_task(Box::new(|| panic!("boom")));
        // Give the loop a moment to consume the panic.
        thread::sleep(Duration::from_millis(50));
        // Now schedule a normal task; it must still run.
        let flag = Arc::new(AtomicBool::new(false));
        let f2 = flag.clone();
        el.schedule_task(Box::new(move || f2.store(true, Ordering::Release)))
            .expect("schedule after panic");
        for _ in 0..250 {
            if flag.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(flag.load(Ordering::Acquire), "post-panic task must run");
        assert!(el.stats.task_panics.load(Ordering::Acquire) >= 1);
    });
}

// Test 5: DispatchStats counters increment correctly.
#[test]
fn t19_6_dispatch_stats_counters_increment() {
    with_loop("t19-6-test-5", |el| {
        for _ in 0..5 {
            el.schedule_task(Box::new(|| {})).expect("schedule");
        }
        // drain
        thread::sleep(Duration::from_millis(50));
        let stats = el.stats();
        assert_eq!(stats.tasks_enqueued, 5);
        assert_eq!(stats.tasks_dispatched, 5);
        assert_eq!(stats.task_panics, 0);
    });
}

// Test 6: cancelling a timer before its deadline prevents firing.
#[test]
fn t19_6_cancel_before_fire_is_noop() {
    with_loop("t19-6-test-6", |el| {
        let fired = Arc::new(AtomicBool::new(false));
        let f2 = fired.clone();
        let (_tid, cancel_flag) = el
            .schedule_timer(
                Instant::now() + Duration::from_millis(200),
                Box::new(move || f2.store(true, Ordering::Release)),
            )
            .expect("schedule");
        cancel_flag.store(true, Ordering::Release);
        // Wake so the loop reconsiders the (now cancelled) head.
        el.parker.wake();
        thread::sleep(Duration::from_millis(400));
        assert!(
            !fired.load(Ordering::Acquire),
            "cancelled timer must not fire"
        );
        // Stats: cancelled should have been counted.
        assert!(el.stats.timers_cancelled.load(Ordering::Acquire) >= 1);
    });
}

// Test 7: 8 threads concurrently scheduling tasks — no data race,
// every task runs exactly once.
#[test]
fn t19_6_eight_threads_schedule_concurrently() {
    with_loop("t19-6-test-7", |el| {
        let counter = Arc::new(AtomicUsize::new(0));
        let per_thread = 25;
        let threads = 8;
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let el2 = el.clone();
            let c2 = counter.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..per_thread {
                    let c3 = c2.clone();
                    el2.schedule_task(Box::new(move || {
                        c3.fetch_add(1, Ordering::Relaxed);
                    }))
                    .expect("schedule");
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        // Wait for drain.
        let total = threads * per_thread;
        for _ in 0..500 {
            if counter.load(Ordering::Acquire) == total {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(counter.load(Ordering::Acquire), total);
    });
}

// Test 8: EventLoopId validation rejects negative and zero.
#[test]
fn t19_6_event_loop_id_validation() {
    assert!(EventLoopId::from_raw(-1).is_err());
    assert!(EventLoopId::from_raw(0).is_err());
    assert!(EventLoopId::from_raw(1).is_ok());
    assert_eq!(EventLoopId::from_raw(42).unwrap().to_raw(), 42);
}

// Test 9: manager.schedule_on_event_loop rejects unknown ids.
#[test]
fn t19_6_schedule_rejects_unknown_id() {
    let mgr = EventLoopManager::new();
    // A very high id past the allocation high-water mark is NotFound.
    let bogus = EventLoopId(u64::MAX / 2);
    let res = mgr.schedule_on_event_loop(bogus, Box::new(|| {}));
    assert_eq!(res, Err(EventLoopError::NotFound));
}

// Test 10: schedule_task on a shutdown loop returns ShuttingDown.
#[test]
fn t19_6_schedule_after_shutdown_errors() {
    let mgr = EventLoopManager::new();
    let el = mgr.spawn("t19-6-test-10").expect("spawn");
    let id = el.id;
    // Give the loop a chance to claim its thread id slot.
    for _ in 0..200 {
        if el.loop_thread_id.load(Ordering::Acquire) != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    el.shutdown();
    // Direct schedule on the handle — should be ShuttingDown.
    let res = el.schedule_task(Box::new(|| {}));
    assert_eq!(res, Err(EventLoopError::ShuttingDown));
    // Clean up join.
    let _ = mgr.shutdown_and_join(id);
}

// Test 11: timer heap lazy cancellation frees root-position entries.
#[test]
fn t19_6_timer_heap_lazy_cancel() {
    let mut heap = TimerHeap::new();
    let now = Instant::now();
    let c1 = Arc::new(AtomicBool::new(false));
    let c2 = Arc::new(AtomicBool::new(false));
    heap.push_bounded(ScheduledTimer {
        deadline: now + Duration::from_millis(10),
        seq: 0,
        task: Some(Box::new(|| {})),
        cancelled: c1.clone(),
        timer_id: 1,
    })
    .map_err(|_| "cap hit")
    .unwrap();
    heap.push_bounded(ScheduledTimer {
        deadline: now + Duration::from_millis(20),
        seq: 1,
        task: Some(Box::new(|| {})),
        cancelled: c2.clone(),
        timer_id: 2,
    })
    .map_err(|_| "cap hit")
    .unwrap();
    assert_eq!(heap.len(), 2);
    // Cancel the first. peek_deadline should now skip it and return c2's.
    c1.store(true, Ordering::Release);
    let d = heap.peek_deadline().expect("has peek");
    assert_eq!(d, now + Duration::from_millis(20));
    // The cancelled root should have been popped; only c2 remains.
    assert_eq!(heap.len(), 1);
}

// Test 12: MAX_PENDING_TASKS cap bounces with TaskQueueFull.
#[test]
fn t19_6_task_queue_cap_enforced() {
    // Construct an EventLoop directly (no spawn) so we can stuff it
    // at will without racing a draining loop.
    let el = EventLoop::new(EventLoopId(99_999), "cap-test");
    for i in 0..MAX_PENDING_TASKS {
        let res = el.schedule_task(Box::new(|| {}));
        assert!(res.is_ok(), "push {} must succeed", i);
    }
    let res = el.schedule_task(Box::new(|| {}));
    assert_eq!(res, Err(EventLoopError::TaskQueueFull));
}

// Test 13: MAX_SCHEDULED_TIMERS cap bounces with TimerHeapFull.
#[test]
fn t19_6_timer_heap_cap_enforced() {
    let el = EventLoop::new(EventLoopId(99_998), "timer-cap-test");
    let d = Instant::now() + Duration::from_secs(3600);
    for _ in 0..MAX_SCHEDULED_TIMERS {
        let res = el.schedule_timer(d, Box::new(|| {}));
        assert!(res.is_ok());
    }
    let res = el.schedule_timer(d, Box::new(|| {}));
    assert_eq!(res.err(), Some(EventLoopError::TimerHeapFull));
}

// Test 14: wake deduplication — a second wake before park consumes the
// first is a no-op at the wakes_issued counter.
#[test]
fn t19_6_wake_dedupes_concurrent_calls() {
    let wk = WakeableCondvar::new();
    wk.wake();
    wk.wake(); // second wake is a dedupe
    wk.wake(); // third too
               // Now park should observe the wake on the fast path.
    let woken = wk.park(Duration::from_millis(50));
    assert!(woken);
    // After consumption, the next park should timeout. gci i18 (orchestrator):
    // `park` may return `false` early on a spurious condvar wakeup, so park
    // until 40 ms have passed; the property is that no park reports a wake.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(40) {
        assert!(!wk.park(Duration::from_millis(50)), "a consumed wake was seen twice");
    }
}

// Test 15: shutdown_and_join drains pending tasks before the loop exits.
#[test]
fn t19_6_shutdown_drains_remaining_tasks() {
    let mgr = EventLoopManager::new();
    let el = mgr.spawn("t19-6-test-15").expect("spawn");
    let id = el.id;
    // Queue a bunch of tasks WITHOUT waiting for them to drain.
    let c = Arc::new(AtomicUsize::new(0));
    for _ in 0..20 {
        let c2 = c.clone();
        el.schedule_task(Box::new(move || {
            c2.fetch_add(1, Ordering::Relaxed);
        }))
        .expect("schedule");
    }
    // Immediately shut down; the loop must still drain them per the
    // module contract.
    mgr.shutdown_and_join(id).expect("shutdown");
    assert_eq!(c.load(Ordering::Acquire), 20);
}

// Test 16: EventLoopAffinity round-trips an id.
#[test]
fn t19_6_event_loop_affinity_round_trip() {
    let id = EventLoopId::from_raw(7).unwrap();
    let aff = EventLoopAffinity::new(id);
    assert_eq!(aff.loop_id, id);
    let copy = aff;
    assert_eq!(copy, aff);
}

// Test 17: current_event_loop() is None on a non-loop thread and
// Some on a loop thread.
#[test]
fn t19_6_current_event_loop_tls_tracking() {
    assert!(current_event_loop().is_none());
    with_loop("t19-6-test-17", |el| {
        let found = Arc::new(Mutex::new(false));
        let f2 = found.clone();
        let expected_id = el.id;
        el.schedule_task(Box::new(move || {
            if let Some(curr) = current_event_loop() {
                if curr.id == expected_id {
                    *f2.lock().unwrap_or_else(|e| e.into_inner()) = true;
                }
            }
        }))
        .expect("schedule");
        for _ in 0..250 {
            if *found.lock().unwrap_or_else(|e| e.into_inner()) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(*found.lock().unwrap_or_else(|e| e.into_inner()));        });    }