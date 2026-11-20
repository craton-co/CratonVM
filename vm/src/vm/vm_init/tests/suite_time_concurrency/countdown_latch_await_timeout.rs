#[allow(unused_imports)]
use super::*;

#[ignore = "STALE: the test passes a NULL TimeUnit and expects `false`. HotSpot throws NullPointerException: Cannot invoke \"java.util.concurrent.TimeUnit.toNanos(long)\" because \"unit\" is null -- verified on 25.0.3 -- and CratonVM now throws the identical message, so the CURRENT behaviour is correct and the assertion is not. It passed only while a synthetic CountDownLatch.await native ignored its TimeUnit argument. Triaged 2026-09-02 as part of the supposed enum-constant cluster; it is not an enum defect. synthetic-jdk-enum-surface-20260902.md"]
#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn countdown_latch_await_timeout_returns_result() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let latch = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "<init>",
        "(I)V",
        &[Value::Object(Some(latch)), Value::Int(1)],
    )
    .unwrap();

    // Await with timeout (count > 0 → returns false)
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "await",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        &[
            Value::Object(Some(latch)),
            Value::Long(100),
            Value::Object(None),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(0)); // not yet reached 0

    // Count down
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "countDown",
        "()V",
        &[Value::Object(Some(latch))],
    )
    .unwrap();

    // Now await should return true
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "await",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        &[
            Value::Object(Some(latch)),
            Value::Long(100),
            Value::Object(None),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn semaphore_basic_acquire_release() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sem = alloc_receiver(&shared, &mut thread, "java/util/concurrent/Semaphore", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "<init>",
        "(I)V",
        &[Value::Object(Some(sem)), Value::Int(3)],
    )
    .unwrap();

    let permits = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "availablePermits",
        "()I",
        &[Value::Object(Some(sem))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(permits, Value::Int(3));

    // Acquire 1
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "acquire",
        "()V",
        &[Value::Object(Some(sem))],
    )
    .unwrap();

    let permits = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "availablePermits",
        "()I",
        &[Value::Object(Some(sem))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(permits, Value::Int(2));

    // Release 1
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "release",
        "()V",
        &[Value::Object(Some(sem))],
    )
    .unwrap();

    let permits = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "availablePermits",
        "()I",
        &[Value::Object(Some(sem))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(permits, Value::Int(3));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn semaphore_try_acquire() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sem = alloc_receiver(&shared, &mut thread, "java/util/concurrent/Semaphore", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "<init>",
        "(I)V",
        &[Value::Object(Some(sem)), Value::Int(1)],
    )
    .unwrap();

    // First tryAcquire succeeds
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "tryAcquire",
        "()Z",
        &[Value::Object(Some(sem))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));

    // Second tryAcquire fails (no permits left)
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "tryAcquire",
        "()Z",
        &[Value::Object(Some(sem))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn semaphore_try_acquire_n() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sem = alloc_receiver(&shared, &mut thread, "java/util/concurrent/Semaphore", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "<init>",
        "(I)V",
        &[Value::Object(Some(sem)), Value::Int(5)],
    )
    .unwrap();

    // Try to acquire 3 of 5
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "tryAcquire",
        "(I)Z",
        &[Value::Object(Some(sem)), Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));

    let permits = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "availablePermits",
        "()I",
        &[Value::Object(Some(sem))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(permits, Value::Int(2));

    // Try to acquire 3 more — fails (only 2 available)
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "tryAcquire",
        "(I)Z",
        &[Value::Object(Some(sem)), Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn semaphore_drain_permits() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sem = alloc_receiver(&shared, &mut thread, "java/util/concurrent/Semaphore", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "<init>",
        "(I)V",
        &[Value::Object(Some(sem)), Value::Int(5)],
    )
    .unwrap();

    let drained = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "drainPermits",
        "()I",
        &[Value::Object(Some(sem))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(drained, Value::Int(5));

    let permits = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "availablePermits",
        "()I",
        &[Value::Object(Some(sem))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(permits, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn semaphore_fair_mode() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sem = alloc_receiver(&shared, &mut thread, "java/util/concurrent/Semaphore", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "<init>",
        "(IZ)V",
        &[Value::Object(Some(sem)), Value::Int(1), Value::Int(1)],
    )
    .unwrap();

    let fair = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Semaphore",
        "isFair",
        "()Z",
        &[Value::Object(Some(sem))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(fair, Value::Int(1));
}

#[ignore = "STALE: CyclicBarrier natives are registered only in synthetic-AQS mode or at runtime under --synthetic-jdk; the real-AQS default (which this harness builds) runs the real JDK class. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn cyclic_barrier_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let barrier = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "<init>",
        "(I)V",
        &[Value::Object(Some(barrier)), Value::Int(3)],
    )
    .unwrap();

    let parties = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "getParties",
        "()I",
        &[Value::Object(Some(barrier))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(parties, Value::Int(3));

    let waiting = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "getNumberWaiting",
        "()I",
        &[Value::Object(Some(barrier))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(waiting, Value::Int(0));

    let broken = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "isBroken",
        "()Z",
        &[Value::Object(Some(barrier))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(broken, Value::Int(0));
}


/// `await()` is a rendezvous: it blocks until every party has arrived.
///
/// This test used to call `await()` three times on ONE thread and assert
/// it returned 2, then 1, then 0. That only passed against the original
/// stub, which returned the arrival index WITHOUT waiting — see the
/// `cb_await_inner` doc comment in `native-builtins`, which records the
/// non-waiting behaviour as "wrong for any real barrier user once the
/// storage works". When the native was corrected to actually block, this
/// test was left behind and deadlocked on its first `await()`, holding a
/// libtest worker thread for the rest of the run.
///
/// The barrier contract is now exercised the only way it can be: three
/// parties on three threads. Each gets a distinct arrival index and the
/// set of them is exactly {2, 1, 0}, which subsumes the old assertion
/// without depending on which thread arrives first.
#[ignore = "STALE: CyclicBarrier natives are registered only in synthetic-AQS mode or at runtime under --synthetic-jdk; the real-AQS default (which this harness builds) runs the real JDK class. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn cyclic_barrier_await_sequence() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let barrier = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "<init>",
        "(I)V",
        &[Value::Object(Some(barrier)), Value::Int(3)],
    )
    .unwrap();

    // Two parties block; the third (this thread) trips the barrier.
    let parties: Vec<_> = (1..3u64)
        .map(|i| {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let mut t = JvmThread::new(ThreadId(i), "party");
                call_native(
                    &shared,
                    &mut t,
                    "java/util/concurrent/CyclicBarrier",
                    "await",
                    "()I",
                    &[Value::Object(Some(barrier))],
                )
                .unwrap()
                .unwrap()
            })
        })
        .collect();

    // Do not trip the barrier until both other parties are parked —
    // otherwise this thread could arrive first and block forever.
    cb_await_waiting_count(&shared, barrier, 2);

    let mut indices = vec![call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "await",
        "()I",
        &[Value::Object(Some(barrier))],
    )
    .unwrap()
    .unwrap()];
    for p in parties {
        indices.push(p.join().expect("party thread panicked"));
    }

    let mut indices: Vec<i32> = indices
        .into_iter()
        .map(|v| match v {
            Value::Int(i) => i,
            other => panic!("await returned {other:?}, expected Int"),
        })
        .collect();
    indices.sort_unstable();
    assert_eq!(
        indices,
        vec![0, 1, 2],
        "each party must get a distinct arrival index",
    );

    // After the trip the generation is reset.
    let waiting = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "getNumberWaiting",
        "()I",
        &[Value::Object(Some(barrier))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(waiting, Value::Int(0));
}

/// `reset()` releases a party that is already parked in `await()`.
///
/// Same history as `cyclic_barrier_await_sequence`: the old version
/// called `await()` on the main thread with `parties = 2` and relied on
/// it returning immediately, so it deadlocked once the native began
/// honouring the rendezvous. Resetting a barrier nobody is waiting on
/// also tested nothing — the interesting case is precisely the one with
/// a parked waiter, which is what this now sets up.
#[ignore = "STALE: CyclicBarrier natives are registered only in synthetic-AQS mode or at runtime under --synthetic-jdk; the real-AQS default (which this harness builds) runs the real JDK class. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn cyclic_barrier_reset() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let barrier = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "<init>",
        "(I)V",
        &[Value::Object(Some(barrier)), Value::Int(2)],
    )
    .unwrap();

    // One of two parties arrives and parks.
    let waiter = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let mut t = JvmThread::new(ThreadId(1), "party");
            call_native(
                &shared,
                &mut t,
                "java/util/concurrent/CyclicBarrier",
                "await",
                "()I",
                &[Value::Object(Some(barrier))],
            )
        })
    };
    cb_await_waiting_count(&shared, barrier, 1);

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "reset",
        "()V",
        &[Value::Object(Some(barrier))],
    )
    .unwrap();

    // The reset must release the parked party rather than strand it.
    waiter
        .join()
        .expect("waiting party panicked")
        .expect("reset should release the parked party");

    let waiting = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "getNumberWaiting",
        "()I",
        &[Value::Object(Some(barrier))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(waiting, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn copy_on_write_arraylist_basic() {
    // `CopyOnWriteArrayList.<init>()V` is deliberately NOT overridden so the
    // real constructor runs and initialises `this.lock` — the registration
    // comment in `native-builtins/src/util_concurrent_ext.rs` records that
    // the previous override wrote the backing array into the `lock` slot
    // and broke Spring's BeanPostProcessor lists. The mutators ARE bridged;
    // only construction defers.
    let registry = &*TEST_NATIVE_REGISTRY;
    let cowal = "java/util/concurrent/CopyOnWriteArrayList";
    assert!(
        registry.find(cowal, "<init>", "()V").is_none(),
        "COWAL construction must run real bytecode so `lock` is built",
    );
    assert!(
        registry
            .find(cowal, "add", "(Ljava/lang/Object;)Z")
            .is_some(),
        "COWAL mutators are permanent bridges and must stay registered",
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn countdown_latch_negative_count_throws() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let latch = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "<init>",
        "(I)V",
        &[Value::Object(Some(latch)), Value::Int(-1)],
    );
    assert!(result.is_err());
}

#[ignore = "STALE: CyclicBarrier natives are registered only in synthetic-AQS mode or at runtime under --synthetic-jdk; the real-AQS default (which this harness builds) runs the real JDK class. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn cyclic_barrier_zero_parties_throws() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let barrier = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CyclicBarrier",
        "<init>",
        "(I)V",
        &[Value::Object(Some(barrier)), Value::Int(0)],
    );
    assert!(result.is_err());
}

// ========================================================================
// Phase 25: Properties tests
// ========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn properties_set_and_get_property() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Allocate with 4 fields (3 for HashMap + 1 for defaults)
    let props = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "()V",
        &[Value::Object(Some(props))],
    )
    .unwrap();

    let key = create_java_string(&shared, "host");
    let val = create_java_string(&shared, "localhost");
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        &[
            Value::Object(Some(props)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(props)), Value::Object(Some(key))],
    )
    .unwrap();
    let result_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, result_obj),
        Some("localhost".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn properties_get_missing_returns_null() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let props = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "()V",
        &[Value::Object(Some(props))],
    )
    .unwrap();

    let key = create_java_string(&shared, "missing");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(props)), Value::Object(Some(key))],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Object(None)));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn properties_get_with_default_value() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let props = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "()V",
        &[Value::Object(Some(props))],
    )
    .unwrap();

    let key = create_java_string(&shared, "missing");
    let def = create_java_string(&shared, "default_val");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[
            Value::Object(Some(props)),
            Value::Object(Some(key)),
            Value::Object(Some(def)),
        ],
    )
    .unwrap();
    let result_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, result_obj),
        Some("default_val".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn properties_defaults_chain() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create defaults Properties with a key
    let defaults = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "()V",
        &[Value::Object(Some(defaults))],
    )
    .unwrap();
    let dk = create_java_string(&shared, "color");
    let dv = create_java_string(&shared, "blue");
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        &[
            Value::Object(Some(defaults)),
            Value::Object(Some(dk)),
            Value::Object(Some(dv)),
        ],
    )
    .unwrap();

    // Create child Properties with defaults
    let child = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "(Ljava/util/Properties;)V",
        &[Value::Object(Some(child)), Value::Object(Some(defaults))],
    )
    .unwrap();

    // getProperty on child should find key from defaults
    let key = create_java_string(&shared, "color");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(child)), Value::Object(Some(key))],
    )
    .unwrap();
    let result_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, result_obj),
        Some("blue".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn properties_size_and_is_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let props = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "()V",
        &[Value::Object(Some(props))],
    )
    .unwrap();

    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(props))],
    )
    .unwrap();
    assert_eq!(empty, Some(Value::Int(1)));

    let k = create_java_string(&shared, "a");
    let v = create_java_string(&shared, "1");
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        &[
            Value::Object(Some(props)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "size",
        "()I",
        &[Value::Object(Some(props))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(1)));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn properties_load_from_bais() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a BAIS with properties content
    let content = b"# comment\nhost=localhost\nport=8080\n";
    let data = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        content.len(),
    );
    for (i, &b) in content.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(data, i, Value::Int(b as i32));
    }
    // BAIS layout is {buf, pos, mark, count} — 4 slots. Under-allocating
    // triggers the gen_heap layout-mismatch guard which silently drops
    // the count write and later reads back Object(None), making
    // Properties.load() see an empty stream.
    let bais = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(data))],
    )
    .unwrap();

    // Create Properties and load
    let props = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "()V",
        &[Value::Object(Some(props))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "load",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(props)), Value::Object(Some(bais))],
    )
    .unwrap();

    // Verify loaded properties
    let host_key = create_java_string(&shared, "host");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(props)), Value::Object(Some(host_key))],
    )
    .unwrap();
    let result_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, result_obj),
        Some("localhost".to_string())
    );

    let port_key = create_java_string(&shared, "port");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(props)), Value::Object(Some(port_key))],
    )
    .unwrap();
    let result_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, result_obj),
        Some("8080".to_string())
    );

    // Size should be 2
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "size",
        "()I",
        &[Value::Object(Some(props))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(2)));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn properties_load_colon_separator() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let content = b"key1:value1\nkey2:value2\n";
    let data = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        content.len(),
    );
    for (i, &b) in content.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(data, i, Value::Int(b as i32));
    }
    // BAIS layout is {buf, pos, mark, count} — 4 slots (see
    // properties_load_from_bais for the rationale).
    let bais = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(data))],
    )
    .unwrap();

    let props = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "()V",
        &[Value::Object(Some(props))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "load",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(props)), Value::Object(Some(bais))],
    )
    .unwrap();

    let key = create_java_string(&shared, "key1");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(props)), Value::Object(Some(key))],
    )
    .unwrap();
    let result_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, result_obj),
        Some("value1".to_string())
    );
}

// T16.3: Properties.load must accept both '=' and ':' separators in the
// same stream and return values as Java String mirrors (not raw bytes or
// Rust strings). Mix both separators, comments, leading whitespace, and
// verify each key lookup returns a valid Java string.
#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn properties_load_mixed_separators() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Mix '=' and ':' separators plus comment lines and leading whitespace.
    let content = b"# mixed separators\nequals_key=eqValue\n  colon_key : colonValue\n! bang comment\nkey3=mix:ed\n";
    let data = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        content.len(),
    );
    for (i, &b) in content.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(data, i, Value::Int(b as i32));
    }
    // BAIS: {buf, pos, mark, count} — 4 slots.
    let bais = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(data))],
    )
    .unwrap();

    let props = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "()V",
        &[Value::Object(Some(props))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "load",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(props)), Value::Object(Some(bais))],
    )
    .unwrap();

    // Helper: look up a key and assert that the returned Value is a Java
    // String mirror carrying the expected Rust string contents.
    let lookup = |shared: &Arc<SharedVm>, thread: &mut JvmThread, key: &str| -> String {
        let key_obj = create_java_string(shared, key);
        let result = call_native(
            shared,
            thread,
            "java/util/Properties",
            "getProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(props)), Value::Object(Some(key_obj))],
        )
        .unwrap();
        let result_obj = match result {
            Some(Value::Object(Some(o))) => o,
            other => panic!(
                "expected Java String mirror for key {:?}, got {:?}",
                key, other
            ),
        };
        read_java_string(&shared.mem.heap, result_obj)
            .unwrap_or_else(|| panic!("getProperty({:?}) did not return a valid Java String", key))
    };

    // '=' separator
    assert_eq!(lookup(&shared, &mut thread, "equals_key"), "eqValue");
    // ':' separator, even when the key line has leading whitespace
    assert_eq!(lookup(&shared, &mut thread, "colon_key"), "colonValue");
    // '=' separator must win when both are present in the value
    assert_eq!(lookup(&shared, &mut thread, "key3"), "mix:ed");

    // Size should be exactly 3 — comments are skipped.
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "size",
        "()I",
        &[Value::Object(Some(props))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(3)));
}

// ========================================================================
// Phase 25: Base64 tests
// ========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn base64_encode_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Get encoder
    let encoder_result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64",
        "getEncoder",
        "()Ljava/util/Base64$Encoder;",
        &[],
    )
    .unwrap();
    let encoder = match encoder_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected encoder"),
    };

    // Create byte[] input: "Hello"
    let input = b"Hello";
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        input.len(),
    );
    for (i, &b) in input.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(b as i32));
    }

    // encodeToString
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64$Encoder",
        "encodeToString",
        "([B)Ljava/lang/String;",
        &[Value::Object(Some(encoder)), Value::Object(Some(arr))],
    )
    .unwrap();
    let result_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, result_obj),
        Some("SGVsbG8=".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn base64_encode_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let encoder_result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64",
        "getEncoder",
        "()Ljava/util/Base64$Encoder;",
        &[],
    )
    .unwrap();
    let encoder = match encoder_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected encoder"),
    };

    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        0,
    );
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64$Encoder",
        "encodeToString",
        "([B)Ljava/lang/String;",
        &[Value::Object(Some(encoder)), Value::Object(Some(arr))],
    )
    .unwrap();
    let result_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, result_obj),
        Some(String::new())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn base64_decode_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let decoder_result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64",
        "getDecoder",
        "()Ljava/util/Base64$Decoder;",
        &[],
    )
    .unwrap();
    let decoder = match decoder_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected decoder"),
    };

    // decode("SGVsbG8=") -> "Hello"
    let encoded = create_java_string(&shared, "SGVsbG8=");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64$Decoder",
        "decode",
        "(Ljava/lang/String;)[B",
        &[Value::Object(Some(decoder)), Value::Object(Some(encoded))],
    )
    .unwrap();
    let result_arr = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected byte array"),
    };

    let len = shared.mem.heap.array_length(result_arr);
    assert_eq!(len, 5);
    let mut decoded = Vec::new();
    for i in 0..len {
        if let Value::Int(b) = shared.mem.heap.get_array_element(result_arr, i).unwrap() {
            decoded.push(b as u8);
        }
    }
    assert_eq!(&decoded, b"Hello");
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn base64_roundtrip() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let encoder_result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64",
        "getEncoder",
        "()Ljava/util/Base64$Encoder;",
        &[],
    )
    .unwrap();
    let encoder = match encoder_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected encoder"),
    };

    let decoder_result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64",
        "getDecoder",
        "()Ljava/util/Base64$Decoder;",
        &[],
    )
    .unwrap();
    let decoder = match decoder_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected decoder"),
    };

    // Input: "Hello, World!" as byte array
    let input = b"Hello, World!";
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        input.len(),
    );
    for (i, &b) in input.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(b as i32));
    }

    // Encode
    let encoded = call_native(
        &shared,
        &mut thread,
        "java/util/Base64$Encoder",
        "encode",
        "([B)[B",
        &[Value::Object(Some(encoder)), Value::Object(Some(arr))],
    )
    .unwrap();
    let encoded_arr = match encoded {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected byte array"),
    };

    // Decode
    let decoded = call_native(
        &shared,
        &mut thread,
        "java/util/Base64$Decoder",
        "decode",
        "([B)[B",
        &[
            Value::Object(Some(decoder)),
            Value::Object(Some(encoded_arr)),
        ],
    )
    .unwrap();
    let decoded_arr = match decoded {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected byte array"),
    };

    let len = shared.mem.heap.array_length(decoded_arr);
    assert_eq!(len, input.len());
    let mut result = Vec::new();
    for i in 0..len {
        if let Value::Int(b) = shared.mem.heap.get_array_element(decoded_arr, i).unwrap() {
            result.push(b as u8);
        }
    }
    assert_eq!(&result, input);
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn base64_url_encoder() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let encoder_result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64",
        "getUrlEncoder",
        "()Ljava/util/Base64$Encoder;",
        &[],
    )
    .unwrap();
    let encoder = match encoder_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected encoder"),
    };

    // Input with bytes that would produce + and / in basic encoding
    // Use bytes [0xFB, 0xFF, 0xFE] which produces +//+ in basic
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        3,
    );
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Int(0xFBu8 as i32));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Int(0xFFu8 as i32));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 2, Value::Int(0xFEu8 as i32));

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64$Encoder",
        "encodeToString",
        "([B)Ljava/lang/String;",
        &[Value::Object(Some(encoder)), Value::Object(Some(arr))],
    )
    .unwrap();
    let result_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    let s = read_java_string(&shared.mem.heap, result_obj).unwrap();
    // URL-safe encoding should not contain + or /
    assert!(!s.contains('+'));
    assert!(!s.contains('/'));
    assert!(s.contains('-') || s.contains('_'));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn base64_url_roundtrip() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let encoder_result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64",
        "getUrlEncoder",
        "()Ljava/util/Base64$Encoder;",
        &[],
    )
    .unwrap();
    let encoder = match encoder_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected encoder"),
    };
    let decoder_result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64",
        "getUrlDecoder",
        "()Ljava/util/Base64$Decoder;",
        &[],
    )
    .unwrap();
    let decoder = match decoder_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected decoder"),
    };

    let input = b"test URL-safe encoding!";
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        input.len(),
    );
    for (i, &b) in input.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(b as i32));
    }

    let encoded = call_native(
        &shared,
        &mut thread,
        "java/util/Base64$Encoder",
        "encode",
        "([B)[B",
        &[Value::Object(Some(encoder)), Value::Object(Some(arr))],
    )
    .unwrap();
    let encoded_arr = match encoded {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected byte array"),
    };

    let decoded = call_native(
        &shared,
        &mut thread,
        "java/util/Base64$Decoder",
        "decode",
        "([B)[B",
        &[
            Value::Object(Some(decoder)),
            Value::Object(Some(encoded_arr)),
        ],
    )
    .unwrap();
    let decoded_arr = match decoded {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected byte array"),
    };

    let len = shared.mem.heap.array_length(decoded_arr);
    let mut result = Vec::new();
    for i in 0..len {
        if let Value::Int(b) = shared.mem.heap.get_array_element(decoded_arr, i).unwrap() {
            result.push(b as u8);
        }
    }
    assert_eq!(&result, input);
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn base64_decode_invalid_throws() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let decoder_result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64",
        "getDecoder",
        "()Ljava/util/Base64$Decoder;",
        &[],
    )
    .unwrap();
    let decoder = match decoder_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected decoder"),
    };

    let bad_input = create_java_string(&shared, "!!!not-base64!!!");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Base64$Decoder",
        "decode",
        "(Ljava/lang/String;)[B",
        &[Value::Object(Some(decoder)), Value::Object(Some(bad_input))],
    );
    assert!(result.is_err());
}

