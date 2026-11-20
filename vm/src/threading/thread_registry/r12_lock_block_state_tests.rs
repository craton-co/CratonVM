use super::*;

#[test]
fn a_published_contended_entry_reads_blocked_until_it_completes() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(9101);
    registry.register(tid, "r12-lazy-parker", None);
    // SAFETY: only stored and compared by address; never dereferenced.
    let lock = unsafe { ObjectRef::from_raw(0x51_0000 as *mut u8) };
    assert_eq!(registry.java_block_state(tid), 0, "running: RUNNABLE");
    registry.set_jmx_contended_monitor(tid, lock);
    assert_eq!(registry.java_block_state(tid), 2, "contending: BLOCKED");
    registry.complete_jmx_monitor_enter(tid, lock);
    assert_eq!(registry.java_block_state(tid), 0, "acquired: RUNNABLE");
    registry.remove_jmx_locked_monitor(tid, lock);
    assert_eq!(
        registry.java_block_state(ThreadId(9102)),
        0,
        "unknown thread"
    );
}

/// Interpreter round i2 wave 57 (lane L1): a thread inside `Object.wait`
/// that is not GC-blocked (the release-first wait's lazy running phase)
/// reads its wait's state while its waiting monitor is published, and
/// RUNNABLE once the wait takes it back, whatever `java_state` still says.
#[test]
fn a_published_wait_reads_its_state_before_it_blocks() {
    let mut registry = ThreadRegistry::new();
    // The wave-57 reading this test pins; `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`
    // (default on since the wave-67 triage) reads the published state
    // without a monitor.
    registry.set_published_all_wait_kinds_for_test(false);
    let tid = ThreadId(9103);
    registry.register(tid, "w57-lazy-waiter", None);
    let block = registry.gc_block_state_of(tid).expect("registered");
    // SAFETY: only stored and compared by address; never dereferenced.
    let lock = unsafe { ObjectRef::from_raw(0x52_0000 as *mut u8) };
    block.java_state.store(1, Ordering::Release);
    assert_eq!(
        registry.java_block_state(tid),
        0,
        "no wait published: RUNNABLE"
    );
    registry.set_jmx_waiting_monitor(tid, lock);
    assert_eq!(registry.java_block_state(tid), 1, "waiting: WAITING");
    block.java_state.store(3, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 3, "timed: TIMED_WAITING");
    block.java_state.store(2, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 2, "re-entering: BLOCKED");
    registry.take_jmx_waiting_monitor(tid);
    assert_eq!(registry.java_block_state(tid), 0, "wait over: RUNNABLE");
}

/// Interpreter round i6 wave 132 (lane L1, i130-L1b,
/// `CRATONVM_JVMTI_THREAD_STATE_ONE_READ`): the one-read facts answer
/// what the three separate reads answer, in every state a published wait
/// passes through, with the all-kinds switch on and off, interrupted or
/// not, and for an unknown thread.
#[test]
fn the_one_read_state_facts_equal_the_three_reads() {
    for all_kinds in [false, true] {
        let mut registry = ThreadRegistry::new();
        registry.set_published_all_wait_kinds_for_test(all_kinds);
        let tid = ThreadId(9132);
        registry.register(tid, "w132-one-read", None);
        let block = registry.gc_block_state_of(tid).expect("registered");
        // SAFETY: only stored and compared by address; never dereferenced.
        let lock = unsafe { ObjectRef::from_raw(0x53_0000 as *mut u8) };
        let check = |registry: &ThreadRegistry, what: &str| {
            for id in [tid, ThreadId(9133)] {
                let (state, kind, interrupted) = registry.jvmti_state_facts(id);
                assert_eq!(state, registry.java_block_state(id), "{what}: state");
                let separate = matches!(state, 1 | 3)
                    .then(|| registry.published_wait_kind_of(id))
                    .flatten();
                assert_eq!(kind, separate, "{what}: kind (all={all_kinds})");
                let flag = registry
                    .get_interrupted_flag(id)
                    .is_some_and(|f| f.load(Ordering::Acquire));
                assert_eq!(interrupted, flag, "{what}: interrupted");
            }
        };
        check(&registry, "running");
        block.java_state.store(1, Ordering::Release);
        check(&registry, "state stored, nothing published");
        registry.set_jmx_waiting_monitor(tid, lock);
        check(&registry, "waiting");
        block.java_state.store(3, Ordering::Release);
        check(&registry, "timed");
        if let Some(flag) = registry.get_interrupted_flag(tid) {
            flag.store(true, Ordering::Release);
        }
        check(&registry, "timed, interrupted");
        block.java_state.store(2, Ordering::Release);
        check(&registry, "re-entering");
        registry.take_jmx_waiting_monitor(tid);
        check(&registry, "wait over");
    }
}

/// Interpreter round i2 wave 59 (lane L6): a thread blocked in a
/// registered native standing in for a JDK native (`BlockKind::InNative`,
/// `jvm_thread::JAVA_STATE_IN_NATIVE`) reads RUNNABLE and in native while
/// the flag is up, and neither once it is down; a WAITING region is not
/// in native.
#[test]
fn a_thread_blocked_in_a_native_reads_runnable_and_in_native() {
    use crate::threading::jvm_thread::JAVA_STATE_IN_NATIVE;
    let registry = ThreadRegistry::new();
    let tid = ThreadId(9104);
    registry.register(tid, "w59-blocked-in-native", None);
    let block = registry.gc_block_state_of(tid).expect("registered");
    block
        .java_state
        .store(JAVA_STATE_IN_NATIVE, Ordering::Release);
    assert!(!registry.blocked_in_native(tid), "flag down: not blocked");
    block.in_blocked_region.store(true, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 0, "in native: RUNNABLE");
    assert!(registry.blocked_in_native(tid));
    assert!(registry.thread_in_native(tid));
    block.java_state.store(1, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 1, "a wait: WAITING");
    assert!(!registry.blocked_in_native(tid));
    assert!(!registry.thread_in_native(tid));
    block.in_blocked_region.store(false, Ordering::Release);
    assert!(
        !registry.blocked_in_native(ThreadId(9105)),
        "unknown thread"
    );
}

/// Interpreter round i2 wave 63 (lane L1, `CRATONVM_PUBLISHED_WAIT_KIND`):
/// a sleep's published wait kind reads as sleeping only while the flag is
/// up over a TIMED_WAITING state; switched off, the reader is told to
/// ask the frames (`None`).
#[test]
fn a_published_sleep_reads_sleeping_only_inside_its_region() {
    use crate::threading::jvm_thread::{JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_SLEEPING};
    let mut registry = ThreadRegistry::new();
    registry.set_published_wait_kind_for_test(false);
    // `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` implies the word too, and is
    // default on since the wave-67 triage.
    registry.set_published_all_wait_kinds_for_test(false);
    let tid = ThreadId(9106);
    registry.register(tid, "w63-sleeper", None);
    let block = registry.gc_block_state_of(tid).expect("registered");
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_SLEEPING, Ordering::Release);
    block.java_state.store(3, Ordering::Release);
    block.in_blocked_region.store(true, Ordering::Release);
    assert_eq!(
        registry.published_sleeping(tid),
        None,
        "switch off: ask the frames"
    );

    registry.set_published_wait_kind_for_test(true);
    assert_eq!(registry.published_sleeping(tid), Some(true), "sleeping");
    assert_eq!(registry.java_block_state(tid), 3, "still TIMED_WAITING");
    // A timed park after the sleep: the kind was lowered at the sleep's end.
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
    assert_eq!(registry.published_sleeping(tid), Some(false), "timed park");
    // A stale kind under another state does not read as a sleep.
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_SLEEPING, Ordering::Release);
    block.java_state.store(1, Ordering::Release);
    assert_eq!(
        registry.published_sleeping(tid),
        Some(false),
        "WAITING is no sleep"
    );
    block.java_state.store(3, Ordering::Release);
    block.in_blocked_region.store(false, Ordering::Release);
    assert_eq!(
        registry.published_sleeping(tid),
        Some(false),
        "flag down: running"
    );
    assert_eq!(
        registry.published_sleeping(ThreadId(9107)),
        Some(false),
        "unknown thread"
    );
}

/// Interpreter round i2 wave 64 (lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
/// every published kind reads back through the one function; switched
/// off, the reader is told to assemble the kind itself (`None`); the
/// switch implies the wave-63 sleep switch.
#[test]
fn every_published_wait_kind_reads_back_through_one_function() {
    use crate::threading::jvm_thread::{
        JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_OBJECT_WAIT, JAVA_WAIT_KIND_PARKED,
        JAVA_WAIT_KIND_REGION_OBJECT_WAIT, JAVA_WAIT_KIND_SLEEPING,
    };
    let mut registry = ThreadRegistry::new();
    registry.set_published_wait_kind_for_test(false);
    registry.set_published_all_wait_kinds_for_test(false);
    let tid = ThreadId(9108);
    registry.register(tid, "w64-waiter", None);
    let block = registry.gc_block_state_of(tid).expect("registered");
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_PARKED, Ordering::Release);
    block.java_state.store(1, Ordering::Release);
    block.in_blocked_region.store(true, Ordering::Release);
    assert_eq!(registry.published_wait_kind_of(tid), None, "switch off");
    assert!(!registry.published_wait_kind(), "sleep switch off too");

    registry.set_published_all_wait_kinds_for_test(true);
    assert!(registry.published_wait_kind(), "implies the sleep switch");
    assert_eq!(
        registry.published_wait_kind_of(tid),
        Some(PublishedWaitKind::Parked)
    );
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_OBJECT_WAIT, Ordering::Release);
    assert_eq!(
        registry.published_wait_kind_of(tid),
        Some(PublishedWaitKind::ObjectWait)
    );
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_REGION_OBJECT_WAIT, Ordering::Release);
    assert_eq!(
        registry.published_wait_kind_of(tid),
        Some(PublishedWaitKind::ObjectWait),
        "the served join"
    );
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
    assert_eq!(
        registry.published_wait_kind_of(tid),
        Some(PublishedWaitKind::Unrecorded)
    );
    // A sleep counts only over TIMED_WAITING with the flag up.
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_SLEEPING, Ordering::Release);
    assert_eq!(
        registry.published_wait_kind_of(tid),
        Some(PublishedWaitKind::Unrecorded),
        "WAITING is no sleep"
    );
    block.java_state.store(3, Ordering::Release);
    assert_eq!(
        registry.published_wait_kind_of(tid),
        Some(PublishedWaitKind::Sleeping)
    );
    assert_eq!(registry.published_sleeping(tid), Some(true));
    block.in_blocked_region.store(false, Ordering::Release);
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
    assert_eq!(
        registry.published_wait_kind_of(ThreadId(9109)),
        Some(PublishedWaitKind::Unrecorded),
        "unknown thread"
    );
}

/// Interpreter round i2 wave 64 (lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
/// the lazy phase of `Object.wait` (flag down) reads its wait's state from
/// the published `Object.wait` kind, not the JMX waiting monitor; the
/// served join's kind, which outlives its flag for a moment, does not
/// count there.
#[test]
fn a_lazy_wait_reads_its_state_from_the_published_kind() {
    use crate::threading::jvm_thread::{
        JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_OBJECT_WAIT, JAVA_WAIT_KIND_REGION_OBJECT_WAIT,
    };
    let mut registry = ThreadRegistry::new();
    registry.set_published_all_wait_kinds_for_test(true);
    let tid = ThreadId(9110);
    registry.register(tid, "w64-lazy-waiter", None);
    let block = registry.gc_block_state_of(tid).expect("registered");
    block.java_state.store(3, Ordering::Release);
    assert_eq!(
        registry.java_block_state(tid),
        0,
        "no wait published: RUNNABLE"
    );
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_OBJECT_WAIT, Ordering::Release);
    assert_eq!(
        registry.java_block_state(tid),
        3,
        "lazy timed wait: TIMED_WAITING"
    );
    block.java_state.store(2, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 2, "re-entering: BLOCKED");
    block.java_state.store(1, Ordering::Release);
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_REGION_OBJECT_WAIT, Ordering::Release);
    assert_eq!(
        registry.java_block_state(tid),
        0,
        "a join's kind after its flag: RUNNABLE"
    );
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 0, "wait over: RUNNABLE");
}

/// Interpreter round i3 wave 79, lane L6 (`CRATONVM_PARK_LAZY_SPIN_NS`):
/// the value parse -- off unset, at `0`, unparsable or on one CPU; a
/// positive value in nanoseconds, capped at `PARK_LAZY_SPIN_CAP`.
#[test]
fn park_lazy_spin_parses_nanos_caps_them_and_skips_one_cpu() {
    use std::time::Duration;
    assert_eq!(
        park_lazy_spin_from(None, 8),
        Some(Duration::from_nanos(PARK_LAZY_SPIN_DEFAULT_NS)),
        "unset: the default spin"
    );
    assert_eq!(
        park_lazy_spin_from(None, 1),
        None,
        "one CPU: no spin, even by default"
    );
    assert_eq!(park_lazy_spin_from(Some("0"), 8), None);
    assert_eq!(park_lazy_spin_from(Some("x"), 8), None);
    assert_eq!(park_lazy_spin_from(Some("-5"), 8), None);
    assert_eq!(
        park_lazy_spin_from(Some("2000"), 1),
        None,
        "one CPU: no spin"
    );
    assert_eq!(
        park_lazy_spin_from(Some(" 2000 "), 2),
        Some(Duration::from_nanos(2_000))
    );
    assert_eq!(
        park_lazy_spin_from(Some("10000"), 8),
        Some(Duration::from_nanos(10_000))
    );
    assert_eq!(
        park_lazy_spin_from(Some("1000000"), 8),
        Some(PARK_LAZY_SPIN_CAP)
    );
    let mut registry = ThreadRegistry::new();
    registry.set_park_lazy_spin_for_test(Some(Duration::from_nanos(2_000)));
    assert_eq!(registry.park_lazy_spin(), Some(Duration::from_nanos(2_000)));
    registry.set_park_lazy_spin_for_test(None);
    assert_eq!(registry.park_lazy_spin(), None);
}

/// Interpreter round i3 wave 71, lane L1 (`CRATONVM_PARK_LAZY`): a park's
/// running phase (flag down, kind `PARKED`) reads its state only with the
/// switch on; off, that window stays RUNNABLE as before. The switch is set
/// explicitly both ways.
#[test]
fn a_lazy_park_reads_its_state_from_the_published_kind_only_with_the_switch() {
    use crate::threading::jvm_thread::{JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_PARKED};
    let mut registry = ThreadRegistry::new();
    registry.set_published_all_wait_kinds_for_test(true);
    registry.set_park_lazy_budget_for_test(None);
    let tid = ThreadId(9112);
    registry.register(tid, "w71-lazy-parker", None);
    let block = registry.gc_block_state_of(tid).expect("registered");
    block.java_state.store(1, Ordering::Release);
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_PARKED, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 0, "switch off: RUNNABLE");
    registry.set_park_lazy_budget_for_test(Some(std::time::Duration::from_micros(2_000)));
    assert_eq!(registry.java_block_state(tid), 1, "lazy park: WAITING");
    block.java_state.store(3, Ordering::Release);
    assert_eq!(
        registry.java_block_state(tid),
        3,
        "lazy timed park: TIMED_WAITING"
    );
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 0, "park over: RUNNABLE");
}

/// Interpreter round i3 wave 72, lane L6 (`CRATONVM_PARK_LAZY_STW_WAKE`):
/// with the switch on, a park's running phase is listed for its duration
/// and a pause initiator's `wake_lazy_park_phases` ends a phase that would
/// otherwise wait out a long budget in one wait; off, nothing is listed.
/// The switch is set explicitly both ways.
#[test]
fn a_pause_initiator_wakes_a_listed_park_phase_only_with_the_switch() {
    let mut registry = ThreadRegistry::new();
    registry.set_park_lazy_stw_wake_for_test(false);
    let ps = Arc::new(ParkState::new());
    registry.lazily_parked(&ps, || {
        assert_eq!(registry.lazy_park_state_count(), 0, "off: not listed");
    });
    registry.wake_lazy_park_phases();

    registry.set_park_lazy_stw_wake_for_test(true);
    let registry = Arc::new(registry);
    let stop = Arc::new(AtomicBool::new(false));
    let not_interrupted = AtomicBool::new(false);
    let long = std::time::Duration::from_secs(30);
    let (r2, stop2) = (registry.clone(), stop.clone());
    let initiator = std::thread::spawn(move || {
        // Wait until the parker is listed, then raise the "pause" and wake.
        while r2.lazy_park_state_count() == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        stop2.store(true, Ordering::SeqCst);
        r2.wake_lazy_park_phases();
    });
    let t0 = std::time::Instant::now();
    let over = registry.lazily_parked(&ps, || {
        ps.park_lazily_sliced(long, long, &not_interrupted, &|| {
            !stop.load(Ordering::SeqCst)
        })
    });
    assert!(!over, "a pause is not the end of the park");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(10),
        "the wake ended the phase"
    );
    assert_eq!(
        registry.lazy_park_state_count(),
        0,
        "unlisted on the way out"
    );
    assert_eq!(registry.lazy_park_phases.load(Ordering::SeqCst), 0);
    initiator.join().unwrap();
}

/// Interpreter round i3 wave 72, lane L6 (`CRATONVM_UNPARK_ONE_LOOKUP`):
/// the one-call lookup answers what the two separate lookups answer, and
/// nothing for an unknown or zero `tid`.
#[test]
fn the_one_call_unpark_target_matches_the_separate_lookups() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(9172);
    registry.register(tid, "w72-unpark-target", None);
    let mut backing = [0u64; 2];
    // SAFETY: the registry only stores, hashes and compares this address.
    let mirror = unsafe { ObjectRef::from_raw(backing.as_mut_ptr() as *mut u8) };
    registry.set_java_thread_obj_with_tid(tid, mirror, 77_172);
    let (id, state) = registry
        .unpark_target_by_java_tid(77_172)
        .expect("registered");
    assert_eq!(id, tid);
    assert_eq!(registry.find_thread_id_by_java_tid(77_172), Some(tid));
    let separate = registry
        .find_park_state_by_java_tid(77_172)
        .expect("registered");
    assert!(Arc::ptr_eq(&state, &separate));
    assert!(registry.unpark_target_by_java_tid(77_173).is_none());
    assert!(registry.unpark_target_by_java_tid(0).is_none());
}

/// Interpreter round i2 wave 65 (lane L1, `CRATONVM_SERVED_WAIT_KINDS`):
/// a served stand-in's monitor wait for JDK code that parks reads
/// `PARKED` through both kind readers -- the one JVMTI asks without
/// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` and the one it asks with it -- and
/// spans the lazy phase as an `Object.wait` does.
#[test]
fn a_served_monitor_park_reads_parked_with_or_without_all_wait_kinds() {
    use crate::threading::jvm_thread::{
        JAVA_WAIT_KIND_MONITOR_PARKED, JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_OBJECT_WAIT,
    };
    let mut registry = ThreadRegistry::new();
    registry.set_published_all_wait_kinds_for_test(false);
    registry.set_served_wait_kinds_for_test(true);
    let tid = ThreadId(9111);
    registry.register(tid, "w65-latch-waiter", None);
    let block = registry.gc_block_state_of(tid).expect("registered");
    block.java_state.store(3, Ordering::Release);
    block.in_blocked_region.store(true, Ordering::Release);
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_MONITOR_PARKED, Ordering::Release);
    assert!(registry.published_monitor_parked(tid), "switch on: parked");
    assert_eq!(registry.java_block_state(tid), 3, "timed: TIMED_WAITING");
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_OBJECT_WAIT, Ordering::Release);
    assert!(
        !registry.published_monitor_parked(tid),
        "a plain Object.wait"
    );
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_MONITOR_PARKED, Ordering::Release);
    registry.set_served_wait_kinds_for_test(false);
    assert!(!registry.published_monitor_parked(tid), "switch off");
    registry.set_served_wait_kinds_for_test(true);
    assert!(
        !registry.published_monitor_parked(ThreadId(9112)),
        "unknown thread"
    );

    registry.set_published_all_wait_kinds_for_test(true);
    assert_eq!(
        registry.published_wait_kind_of(tid),
        Some(PublishedWaitKind::Parked)
    );
    // The lazy phase: flag down, the kind and the state up.
    block.in_blocked_region.store(false, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 3, "lazy: TIMED_WAITING");
    block.java_state.store(1, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 1, "lazy untimed: WAITING");
    block
        .java_wait_kind
        .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
    assert_eq!(registry.java_block_state(tid), 0, "wait over: RUNNABLE");
}
