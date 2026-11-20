use super::*;

/// The window, step by step as `start0` takes it: the entry is
/// registered with its mirror, the `java_tid` index is recorded, another
/// thread unparks and interrupts the new thread through both lookups an
/// `unpark` uses, and only then does the start adopt the states the
/// thread will park on. The permit and the interrupt are on them, and the
/// reverse index still answers the same state. Run with the atomic
/// permit both off and on (set explicitly).
#[test]
fn an_unpark_in_the_start_window_reaches_the_state_the_thread_parks_on() {
    for atomic in [false, true] {
        let mut registry = ThreadRegistry::new();
        registry.set_park_atomic_permit_for_test(atomic);
        let tid = ThreadId(9741);
        let mut backing = [0u64; 2];
        // SAFETY: the registry only stores, hashes and compares this address.
        let mirror = unsafe { ObjectRef::from_raw(backing.as_mut_ptr() as *mut u8) };
        registry.register_starting_with_daemon(tid, "i74-start-window", Some(mirror), false);
        registry.set_java_thread_obj_with_tid(tid, mirror, 77_741);

        // Another thread's `unpark(t)` lands in the window.
        let (id, early) = registry.unpark_target_by_java_tid(77_741).expect("indexed");
        assert_eq!(id, tid);
        early.unpark_lock_free();
        registry.set_interrupted(tid, true);

        // The start's park-state step.
        let (park, interrupted) = registry.starting_thread_states(tid);
        registry.set_park_state(tid, park.clone());
        registry.set_interrupted_flag(tid, interrupted.clone());

        assert!(
            Arc::ptr_eq(&park, &early),
            "atomic={atomic}: one state, not two"
        );
        let by_mirror = registry
            .find_park_state_by_thread_obj(mirror)
            .expect("indexed");
        assert!(
            Arc::ptr_eq(&park, &by_mirror),
            "atomic={atomic}: reverse index"
        );
        assert!(
            interrupted.load(Ordering::SeqCst),
            "atomic={atomic}: interrupt kept"
        );
        // The thread's first park finds the permit and returns at once.
        let not_interrupted = AtomicBool::new(false);
        let t0 = std::time::Instant::now();
        park.park_interruptible(Some(std::time::Duration::from_secs(20)), &not_interrupted);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "atomic={atomic}: the window's permit ended the first park"
        );
        assert!(!park.take_permit(), "atomic={atomic}: the park consumed it");
    }
}

/// `CRATONVM_PARK_UNTIMED_NO_POLL` (wave 74, set explicitly): with the
/// switch on, `set_interrupted` alone -- no unpark -- ends a thread's
/// unpolled untimed park, so no writer of the flag depends on the poll.
#[test]
fn set_interrupted_wakes_an_unpolled_park_with_the_switch() {
    let mut registry = ThreadRegistry::new();
    registry.set_park_no_poll_for_test(true);
    assert!(registry.park_no_poll());
    let tid = ThreadId(9743);
    registry.register(tid, "i74-unpolled-parker", None);
    let park = registry.get_park_state(tid).expect("registered");
    park.enable_lock_free();
    let flag = registry.get_interrupted_flag(tid).expect("registered");
    let (tx, rx) = std::sync::mpsc::channel();
    let (park2, flag2) = (park.clone(), flag.clone());
    std::thread::spawn(move || {
        park2.park_interruptible_unpolled(None, &flag2);
        let _ = tx.send(());
    });
    while park.sleeper_count() == 0 {
        std::thread::yield_now();
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
    registry.set_interrupted(tid, true);
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("the interrupt woke the park");
    assert!(flag.load(Ordering::SeqCst), "the flag stays raised");
    assert!(!park.take_permit(), "an interrupt wake gives no permit");
    registry.set_park_no_poll_for_test(false);
    assert!(!registry.park_no_poll());
    registry.set_interrupted(tid, false);
}

/// Interpreter round i3 wave 75, lane L6 (`CRATONVM_SLEEP_NO_POLL`): with
/// that switch on (and `CRATONVM_PARK_UNTIMED_NO_POLL` explicitly off),
/// `set_interrupted` alone ends a thread's unpolled sleep at once.
#[test]
fn set_interrupted_wakes_an_unpolled_sleep_with_the_sleep_switch() {
    let mut registry = ThreadRegistry::new();
    registry.set_park_no_poll_for_test(false);
    registry.set_sleep_no_poll_for_test(true);
    assert!(registry.sleep_no_poll());
    let tid = ThreadId(9744);
    registry.register(tid, "i75-unpolled-sleeper", None);
    let park = registry.get_park_state(tid).expect("registered");
    let flag = registry.get_interrupted_flag(tid).expect("registered");
    let (tx, rx) = std::sync::mpsc::channel();
    let (park2, flag2) = (park.clone(), flag.clone());
    std::thread::spawn(move || {
        let interrupted = park2.sleep_unpolled(std::time::Duration::from_secs(60), &flag2);
        let _ = tx.send(interrupted);
    });
    std::thread::sleep(std::time::Duration::from_millis(20));
    registry.set_interrupted(tid, true);
    let interrupted = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the interrupt woke the sleep");
    assert!(interrupted);
    registry.set_sleep_no_poll_for_test(false);
    assert!(!registry.sleep_no_poll());
    registry.set_interrupted(tid, false);
}

/// Interpreter round i3 wave 76, lane L6
/// (`CRATONVM_PARK_WAKE_AFTER_UNLOCK`, set explicitly): `raise_interrupt`
/// raises the flag WITHOUT waking (an unpolled park waits on) and hands
/// back the entry's own park state, whose `interrupt_wake_after_unlock`
/// then ends the park; an unknown id raises nothing.
#[test]
fn raise_interrupt_raises_the_flag_and_hands_back_the_state_to_wake() {
    let mut registry = ThreadRegistry::new();
    registry.set_park_wake_after_unlock_for_test(true);
    assert!(registry.park_wake_after_unlock());
    assert!(registry
        .raise_interrupt(ThreadId(9_746_999), true)
        .is_none());
    let tid = ThreadId(9746);
    registry.register(tid, "i76-raise-interrupt", None);
    let park = registry.get_park_state(tid).expect("registered");
    let flag = registry.get_interrupted_flag(tid).expect("registered");
    let (tx, rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (park2, flag2) = (park.clone(), flag.clone());
    std::thread::spawn(move || {
        let _ = ready_tx.send(());
        park2.park_interruptible_unpolled(None, &flag2);
        let _ = tx.send(());
    });
    // Wait for the parker to RUN before the grace period (gci fx): under a
    // loaded full-suite run a bare 20 ms sleep could end before the spawned
    // thread started, so it found the flag already raised at its park's
    // entry and returned -- read here as "raise_interrupt woke it".
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the parker started");
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (raised, monitor) = registry.raise_interrupt(tid, true).expect("registered");
    assert!(Arc::ptr_eq(&raised, &park), "the entry's own state");
    assert!(monitor.is_none(), "not in Object.wait()");
    assert!(flag.load(Ordering::SeqCst), "the flag is raised");
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "raise_interrupt itself wakes nothing"
    );
    raised.interrupt_wake_after_unlock();
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("the one wake ended the park");
    registry.set_park_wake_after_unlock_for_test(false);
    assert!(!registry.park_wake_after_unlock());
    registry.set_interrupted(tid, false);
}

/// An id the registry does not know gets fresh, unset states.
#[test]
fn an_unregistered_start_gets_fresh_states() {
    let registry = ThreadRegistry::new();
    let (park, interrupted) = registry.starting_thread_states(ThreadId(9742));
    assert!(!park.take_permit());
    assert!(!interrupted.load(Ordering::SeqCst));
}

/// `CRATONVM_THREAD_START_ONE_PUBLISH` (interpreter round i3 wave 82,
/// lane L6): the one read hands out the entry's own `Arc`s, a thread
/// built from them shares every one with the registry without a `set_*`
/// call, and an unpark in the start window still reaches its park state.
/// The switch is set explicitly; the default is not pinned.
#[test]
fn one_publish_start_adopts_the_entry_state_without_set_calls() {
    let mut registry = ThreadRegistry::new();
    registry.set_thread_start_one_publish_for_test(true);
    assert!(registry.thread_start_one_publish());
    let tid = ThreadId(9743);
    let mut backing = [0u64; 2];
    // SAFETY: the registry only stores, hashes and compares this address.
    let mirror = unsafe { ObjectRef::from_raw(backing.as_mut_ptr() as *mut u8) };
    registry.register_starting_with_daemon(tid, "i82-one-publish", Some(mirror), false);
    registry.set_java_thread_obj_with_tid(tid, mirror, 77_743);
    let (_, early) = registry.unpark_target_by_java_tid(77_743).expect("indexed");
    early.unpark_lock_free();

    let shared = registry.starting_thread_shared(tid).expect("registered");
    let again = registry.starting_thread_shared(tid).expect("registered");
    assert!(
        Arc::ptr_eq(&shared.park_state, &early),
        "the window's state"
    );
    assert!(Arc::ptr_eq(&shared.interrupted, &again.interrupted));
    assert!(Arc::ptr_eq(&shared.root_snapshot, &again.root_snapshot));
    assert!(Arc::ptr_eq(&shared.frame_trace, &again.frame_trace));
    assert!(Arc::ptr_eq(&shared.vm_state, &again.vm_state));
    assert!(Arc::ptr_eq(&shared.gc_block_state, &again.gc_block_state));

    let thread = crate::threading::jvm_thread::JvmThread::with_shared(tid, "i82", shared);
    assert!(Arc::ptr_eq(&thread.park_state, &again.park_state));
    assert!(Arc::ptr_eq(&thread.interrupted, &again.interrupted));
    assert!(Arc::ptr_eq(&thread.gc_block_state, &again.gc_block_state));
    // A deposit through the thread's own handle is the registry's.
    thread.root_snapshot.lock().push(mirror);
    assert_eq!(registry.root_snapshot_of(tid), vec![mirror]);
    let by_mirror = registry
        .find_park_state_by_thread_obj(mirror)
        .expect("indexed");
    assert!(Arc::ptr_eq(&thread.park_state, &by_mirror), "reverse index");
    assert!(
        thread.park_state.take_permit(),
        "the window's permit is kept"
    );

    assert!(registry.starting_thread_shared(ThreadId(9744)).is_none());
    registry.set_thread_start_one_publish_for_test(false);
    assert!(!registry.thread_start_one_publish());
}
