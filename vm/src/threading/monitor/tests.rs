use super::*;
use crate::classloading::ClassId;
use crate::memory::heap::Heap;

/// Helper to create a test object on the heap.
///
/// Leaks the backing `Heap`: dropping it here would free its two
/// (multi-MB) arenas out from under the returned `ObjectRef`, which
/// every caller dereferences well after this function returns. That was
/// a real, reproducible use-after-free — SIGSEGV inside
/// `MonitorTable::enter_inflated_or_contend` -> `ensure_inflated` ->
/// `header_of` reading the (freed) mark word — that crashed the test
/// binary partway through this module's suite once the freed arena
/// mapping got unmapped/reused. A tiny capacity keeps the per-call leak
/// negligible (this helper is called from ~20 tests).
fn test_object() -> ObjectRef {
    let heap: &'static Heap = Box::leak(Box::new(Heap::with_capacity(4096)));
    heap.alloc_object(ClassId::new(0), 0)
}

/// The class id [`compact_object`] allocates: one reference field.
const COMPACT_TEST_CLASS: u32 = 900_777;

/// A COMPACT instance (8-byte header): the shape whose identity hash lives
/// in its NEUTRAL mark word, so hashing it and thin-locking it compete for
/// the same word. A legacy (long-header) object keeps its hash in the aux
/// word and can be hashed and thin-locked at once.
pub(super) fn compact_object(heap: &Heap) -> ObjectRef {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        cratonvm_types::register_class_layout(
            cratonvm_types::FIRST_LAYOUT_DOMAIN,
            COMPACT_TEST_CLASS,
            Arc::new(cratonvm_types::CompactLayout {
                field_disps: vec![8],
                is_ref: vec![true],
                field_kinds: vec![cratonvm_types::FieldStorageKind::Reference],
                ref_disps: vec![8],
                total_size: 16,
            }),
        );
    });
    let obj = heap.alloc_object(ClassId::new(COMPACT_TEST_CLASS), 1);
    assert!(
        header_of(obj).is_short(),
        "precondition: the test class allocates compact"
    );
    obj
}

/// A thin lock/unlock round trip must leave the mark word's quartet
/// (`kind` / `element_type` / `gc_age` / `gc_flags`) exactly as it found
/// it.
///
/// Those bits live in the mark word since the header shrank 24 -> 16, and
/// the last release used to store the bare `MARK_NEUTRAL` constant over
/// them. `GC_FLAG_COMPACT` is one of them, so the first `synchronized`
/// block on a compact object converted it to "legacy" and every later field
/// read decoded a compact-packed body with 16-byte cells.
#[test]
fn a_thin_lock_round_trip_preserves_the_mark_word_quartet() {
    use cratonvm_types::{GC_FLAG_COMPACT, GC_FLAG_OLD_GEN};

    let obj = test_object();
    let header = header_of(obj);
    header.set_gc_flags(GC_FLAG_COMPACT | GC_FLAG_OLD_GEN);
    let before = header.mark_word.load(Ordering::Relaxed);
    assert_eq!(
        header.gc_flags(),
        GC_FLAG_COMPACT | GC_FLAG_OLD_GEN,
        "precondition: the flags are in the mark word"
    );

    try_thin_lock(header, 7).expect("an unlocked, unhashed object thin-locks");
    assert_eq!(
        ObjectHeader::quartet_of(header.mark_word.load(Ordering::Relaxed)),
        ObjectHeader::quartet_of(before),
        "locking must carry the quartet"
    );

    // Recursive acquire/release must carry it too — that arm derives from
    // `cur`, but pin it so a future rewrite cannot regress silently.
    try_thin_recursive_lock(header, 7).expect("recursive acquire");
    try_thin_unlock(header, 7).expect("recursive release");
    assert_eq!(
        ObjectHeader::quartet_of(header.mark_word.load(Ordering::Relaxed)),
        ObjectHeader::quartet_of(before),
        "recursive release must carry the quartet"
    );

    assert_eq!(
        try_thin_unlock(header, 7),
        Ok(None),
        "the last release returns the lock to NEUTRAL"
    );
    let after = header.mark_word.load(Ordering::Relaxed);
    assert_eq!(
        ObjectHeader::mark_state(after),
        types::MARK_NEUTRAL,
        "and the state really is NEUTRAL"
    );
    assert_eq!(
        header.gc_flags(),
        GC_FLAG_COMPACT | GC_FLAG_OLD_GEN,
        "the LAST release is the one that used to erase the flags"
    );
    assert_eq!(
        ObjectHeader::quartet_of(after),
        ObjectHeader::quartet_of(before),
        "kind / element_type / gc_age must survive the unlock too"
    );
}


















/// zgc10: `dead_keys_among` agrees with a naive membership test on both
/// arms, including duplicates and keys outside `dead`.
#[test]
fn dead_keys_among_matches_naive_membership() {
    let dead_sorted: Vec<usize> = (0..1000).map(|i| i * 16 + 8).collect();
    let keys = vec![8usize, 24, 7, 15_992, 16_000, 9_000, 24];
    let mut want: Vec<usize> = keys
        .iter()
        .copied()
        .filter(|k| dead_sorted.contains(k))
        .collect();
    want.sort_unstable();
    want.dedup();

    let mut got = dead_keys_among(&dead_sorted, true, keys.clone());
    got.sort_unstable();
    got.dedup();
    assert_eq!(got, want);

    let mut dead_unsorted = dead_sorted.clone();
    dead_unsorted.reverse();
    let mut got = dead_keys_among(&dead_unsorted, false, keys);
    got.sort_unstable();
    got.dedup();
    assert_eq!(got, want);

    assert!(dead_keys_among(&dead_unsorted, false, Vec::new()).is_empty());
}













// ── Additional edge case tests ────────────────────────────────────










// ── Thin-lock fast-path tests ──────────────────────────────────────────

/// Returns true if `obj` is currently THIN_LOCKED in its mark word.
fn is_thin_locked(obj: ObjectRef) -> bool {
    let mark = header_of(obj).mark_word.load(Ordering::Acquire);
    ObjectHeader::mark_state(mark) == types::MARK_THIN_LOCKED
}

/// Returns true if `obj` is currently INFLATED.
fn is_inflated(obj: ObjectRef) -> bool {
    let mark = header_of(obj).mark_word.load(Ordering::Acquire);
    ObjectHeader::mark_state(mark) == types::MARK_INFLATED
}

/// Returns the active monitor count in the table's enumeration index
/// (summed across shards).
fn monitor_registry_len(table: &MonitorTable) -> usize {
    table.indexed_monitor_count()
}






/// A displaced hash is write-once. Two racers must converge, because an
/// identity hash may never change once observed.
#[test]
fn displacing_a_hash_twice_keeps_the_first() {
    let m = Monitor::new();
    assert_eq!(m.displace_hash(111), 111);
    assert_eq!(m.displace_hash(222), 111);
    assert_eq!(m.displaced_hash(), 111);
}





// ── ARCH-2026-07-26: mark-word-reachable monitors ─────────────────────
//
// The properties these guard are the ones that turn a global-lock probe
// into a per-object pointer chase without introducing a use-after-free.

/// Leak a heap so `ObjectRef`s stay valid for the whole test (same reason
/// as `test_object`, but shared by several objects).
fn leaked_heap() -> &'static Heap {
    Box::leak(Box::new(Heap::new()))
}

/// Drive `obj` all the way to `MARK_INFLATED` and leave it unowned.
fn force_inflated(table: &MonitorTable, obj: ObjectRef, tid: ThreadId) {
    table.enter(obj, tid);
    // `wait` requires a heavyweight monitor, so it inflates. A 1ms timeout
    // keeps it fast; the monitor is re-acquired on return.
    table.wait(obj, tid, Some(1), None).unwrap();
    table.exit(obj, tid).unwrap();
    assert!(is_inflated(obj), "setup must leave the object INFLATED");
}






/// `wait`/`notify` round-trip across two threads, driven entirely through
/// the mark word (the object is already inflated before either thread
/// starts, so no inflation happens on either side).
/// A NOTIFICATION SURVIVES A CONDVAR SIGNAL THAT IS NEVER OBSERVED.
///
/// This is the invariant the netty `ParameterizedSslHandlerTest` stall cost
/// 420 s a run to find, and it is white-box on purpose: the black-box
/// version ("notify, then assert the waiter woke") passes on the BROKEN
/// code too, because there the condvar signal normally does arrive. What
/// broke was the case where it does not, and only the state can be asked
/// about that.
///
/// The measured failure was `polls=78025 signalled=0` with
/// `notifies_since_wait=1` — 78 025 five-millisecond waits, not one of them
/// a signalled return, against a `notifyAll()` the monitor really did
/// serve. The fix is that the notification is now a CREDIT in
/// `MonitorState`, taken under the same mutex a notifier must hold, so the
/// condvar signal is an optimisation rather than the mechanism.
///
/// What this test pins, with no condvar involved at all:
///
/// * `notifyAll()` leaves exactly one credit per waiter parked AT THAT
///   MOMENT — not more (a later waiter must not consume one) and not fewer;
/// * `notify()` leaves exactly one, and never stockpiles credits for
///   waiters that do not exist;
/// * a credit is CONSUMED by a decrement, which is what keeps `notify()`
///   from behaving like `notifyAll()`.
#[test]
fn a_notification_is_a_credit_in_monitor_state_not_only_a_condvar_signal() {
    let m = Monitor::new();
    let owner = ThreadId(7);

    // Pretend three threads are parked, without actually parking any:
    // three tickets in the wait set, in arrival order.
    assert!(m.try_enter(owner));
    let (t1, t2, t3) = {
        let mut s = m.state.lock();
        s.parked_waiters = 3;
        (
            s.enroll_wait_ticket(),
            s.enroll_wait_ticket(),
            s.enroll_wait_ticket(),
        )
    };

    // `notify()` leaves ONE credit, for the longest waiter, and never more
    // than there are waiters.
    m.notify(owner).unwrap();
    assert_eq!(m.state.lock().pending_notifies, 1);
    {
        let mut s = m.state.lock();
        assert!(!s.take_notification(t2), "not the second waiter's");
        assert!(
            s.take_notification(t1),
            "the longest waiter's (wave 29, FIFO)"
        );
    }
    m.notify(owner).unwrap();
    m.notify(owner).unwrap();
    assert_eq!(m.state.lock().pending_notifies, 2);
    m.notify(owner).unwrap();
    assert_eq!(
        m.state.lock().pending_notifies,
        2,
        "a notify with no waiter left to take it must not stockpile a credit \
         that a FUTURE wait would consume as a spurious wakeup"
    );
    {
        let mut s = m.state.lock();
        assert!(s.take_notification(t3) && s.take_notification(t2));
        assert_eq!(s.pending_notifies, 0);
    }

    // `notifyAll()` is exactly the waiters in the wait set now.
    let (a, b) = {
        let mut s = m.state.lock();
        (s.enroll_wait_ticket(), s.enroll_wait_ticket())
    };
    m.notify_all(owner).unwrap();
    assert_eq!(m.state.lock().pending_notifies, 2);

    // A waiter that arrives AFTER the notifyAll cannot take one of those
    // credits and count itself notified -- nor, since wave 29, a credit
    // a `notify` left for an earlier waiter (the lost wakeup: the earlier
    // waiter would have stayed parked).
    let late = m.state.lock().enroll_wait_ticket();
    assert!(!m.state.lock().take_notification(late));
    assert_eq!(
        m.state.lock().pending_notifies,
        2,
        "notifyAll() promises the set parked when it ran, not a standing offer"
    );

    // Consuming is per waiter: the two marked waiters take theirs, and a
    // waiter leaving unmarked is out of the wait set.
    {
        let mut s = m.state.lock();
        assert!(s.take_notification(a) && s.take_notification(b));
        assert_eq!(s.pending_notifies, 0);
        s.leave_wait_set(late);
        assert!(s.wait_queue.is_empty());
    }
}

/// `notify` / `notifyAll` on a monitor this thread does not own must still
/// be an `IllegalMonitorStateException` and must leave NO credit behind —
/// the credit is only reachable past the ownership check, and a refused
/// notification that still armed a waiter would be a spurious wakeup with
/// no notifier.
#[test]
fn a_refused_notify_leaves_no_credit() {
    let m = Monitor::new();
    assert!(m.try_enter(ThreadId(1)));
    {
        let mut s = m.state.lock();
        s.parked_waiters = 2;
    }
    assert!(m.notify(ThreadId(2)).is_err());
    assert!(m.notify_all(ThreadId(2)).is_err());
    assert_eq!(m.state.lock().pending_notifies, 0);
}





/// `prune_stale_after_gc` over every shard at once.
fn prune_all(table: &MonitorTable, survived: &dyn Fn(usize) -> bool) -> usize {
    table.prune_stale_in_shards(0, MONITOR_SHARDS, survived)
}




/// Monitors for different objects land in different shards often enough
/// that the index is not a single serialization point, and `shard_of` stays
/// inside bounds for every input it can be handed.
#[test]
fn shard_selection_is_in_bounds_and_spreads_consecutive_addresses() {
    for raw in [0usize, 8, 16, 0x1000, usize::MAX & !7] {
        assert!(shard_of(raw) < MONITOR_SHARDS);
    }
    // Consecutive 8-byte-aligned allocations must not all collide: the
    // whole point of hashing the address is that neighbouring objects get
    // independent shards.
    let base = 0x7f00_0000_0000usize;
    // addr-keyed: a test-local set of shard indices, not addresses.
    let distinct: std::collections::HashSet<usize> = (0..MONITOR_SHARDS)
        .map(|i| shard_of(base + i * 32))
        .collect();
    assert!(
        distinct.len() > MONITOR_SHARDS / 4,
        "shard hash clusters badly: {} distinct shards for {} consecutive \
         allocations",
        distinct.len(),
        MONITOR_SHARDS
    );
}

// ── Round 11 wave 4 (lane sync): contended spin, release reporting, and
// the thin-lock seed that dead-owner recovery needs. ──────────────────


/// The owner word follows every production ownership transition: acquire,
/// re-entry, release, `wait`'s release-and-reacquire (with its depth
/// restored) and the dead-thread force release. Since round 11 wave 16 it
/// is THE owner, not an advisory copy, so the entry count is pinned too.
#[test]
fn the_owner_word_tracks_every_ownership_transition() {
    let m = Monitor::new();
    let a = ThreadId(3);
    let word = |m: &Monitor| m.owner.load(Ordering::Relaxed);
    let count = |m: &Monitor| m.entry_count.load(Ordering::Relaxed);
    assert_eq!(word(&m), 0);
    assert!(m.try_enter(a));
    assert_eq!(word(&m), owner_word(Some(a)));
    assert!(m.try_enter(a));
    assert_eq!(count(&m), 2);
    assert!(!m.exit_reporting_release(a).unwrap());
    assert_eq!(
        word(&m),
        owner_word(Some(a)),
        "re-entrant release keeps the owner"
    );
    assert!(m.exit_reporting_release(a).unwrap());
    assert_eq!(word(&m), 0);
    assert_eq!(count(&m), 0);

    m.block_enter(a);
    m.block_enter(a);
    assert_eq!(word(&m), owner_word(Some(a)));
    // A 1 ms timed wait releases and re-acquires at the same depth.
    assert_eq!(
        m.wait(a, Some(1), None, None, None).unwrap(),
        WaitOutcome::TimedOutOrSpurious
    );
    assert_eq!(word(&m), owner_word(Some(a)));
    assert_eq!(count(&m), 2, "wait() restores the re-entry depth");
    assert!(m.force_release_if_owned_by(a));
    assert_eq!(word(&m), 0);
    assert_eq!(count(&m), 0);
    assert!(
        !m.force_release_if_owned_by(a),
        "a second forced release finds nothing to release"
    );

    let fresh = Monitor::new();
    fresh.enter_with_recursion(a, 2);
    assert_eq!(word(&fresh), owner_word(Some(a)));
    assert_eq!(fresh.thin_seed_owner(), Some(a));
    assert!(!fresh.exit_reporting_release(a).unwrap());
    assert_eq!(
        fresh.thin_seed_owner(),
        Some(a),
        "still held without a break"
    );
    assert!(fresh.exit_reporting_release(a).unwrap());
    assert_eq!(fresh.thin_seed_owner(), None);
    assert_eq!(word(&fresh), 0);
    // `ThreadId(0)` (the JNI placeholder owner) must not encode as "free",
    // and no real thread encodes as the forced-release sentinel.
    assert_ne!(owner_word(Some(ThreadId(0))), 0);
    assert_ne!(owner_word(Some(ThreadId(0))), OWNER_RELEASING);
}

/// Round 11 wave 16 (lock): the parking handshake. Many threads fighting
/// over one inflated monitor, each parking for real (`block_enter`), must
/// all get through with the protected counter exact -- no lost wake-up
/// (a hang) and no double ownership (a lost update). The counter is a
/// plain read-modify-write done only under the monitor.
#[test]
fn a_contended_monitor_loses_no_wakeup_and_no_update() {
    const THREADS: u64 = 8;
    const ROUNDS: u64 = 2_000;
    let m = Arc::new(Monitor::new());
    let counter = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let m = Arc::clone(&m);
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                let me = ThreadId(100 + t);
                for i in 0..ROUNDS {
                    // Alternate the two contended entries: straight to the
                    // park, and spin-then-park, as the VM's callers do.
                    if i % 2 == 0 || !m.spin_try_enter_adaptive(me) {
                        m.block_enter(me);
                    }
                    // Re-entry inside the critical section.
                    assert!(m.try_enter(me));
                    let v = counter.load(Ordering::Relaxed);
                    std::hint::spin_loop();
                    counter.store(v + 1, Ordering::Relaxed);
                    assert!(!m.exit_reporting_release(me).unwrap());
                    assert!(m.exit_reporting_release(me).unwrap());
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(counter.load(Ordering::Relaxed), THREADS * ROUNDS);
    assert_eq!(m.current_owner(), None);
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
    assert_eq!(m.spinners.load(Ordering::SeqCst), 0);
}

/// Round 11 wave 16 (lock): a release with a woken successor already on
/// its way wakes nobody else, and a woken entrant clears the flag when it
/// re-tries -- whether it wins or goes back to sleep.
#[test]
fn a_pending_successor_suppresses_further_wakeups_until_it_retries() {
    let m = Arc::new(Monitor::new());
    let (owner, parker) = (ThreadId(201), ThreadId(202));
    assert!(m.try_enter(owner));
    let m2 = Arc::clone(&m);
    let h = std::thread::spawn(move || {
        m2.block_enter(parker);
        assert!(m2.exit_reporting_release(parker).unwrap());
    });
    // Wait until the parker is registered.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while m.entry_waiters.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "parker never parked");
        std::thread::yield_now();
    }
    // Pretend a successor is already on its way: a release then owes no
    // wake-up. (Cleared again before the real release: no such successor
    // exists to clear it.)
    m.succ_pending.store(true, Ordering::SeqCst);
    assert!(!m.successor_wake_needed());
    m.succ_pending.store(false, Ordering::SeqCst);
    assert!(m.successor_wake_needed());
    assert!(m.exit_reporting_release(owner).unwrap());
    h.join().unwrap();
    assert!(
        !m.succ_pending.load(Ordering::SeqCst),
        "the woken entrant cleared it"
    );
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
    assert_eq!(m.current_owner(), None);
}

/// Round 11 wave 19 (lock, proposal W17-1): the bounded running park
/// gives up on a monitor held for its whole budget having taken nothing
/// and left no registration behind, and is woken -- as a registered
/// entrant, by the release's own successor wake-up -- when the owner
/// releases inside it.
#[test]
fn park_enter_for_is_bounded_and_woken_by_a_release() {
    let m = Arc::new(Monitor::new());
    let (owner, parker) = (ThreadId(211), ThreadId(212));
    assert!(m.park_enter_while(parker, std::time::Duration::from_millis(1), || true));
    assert_eq!(
        m.current_owner(),
        Some(parker),
        "a free monitor is taken at once"
    );
    assert!(m.exit_reporting_release(parker).unwrap());

    assert!(m.try_enter(owner));
    assert!(!m.park_enter_while(parker, std::time::Duration::from_millis(2), || true));
    assert_eq!(
        m.current_owner(),
        Some(owner),
        "a timed-out park takes nothing"
    );
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0, "and deregisters");
    assert!(!m.succ_pending.load(Ordering::SeqCst));

    let m2 = Arc::clone(&m);
    let h = std::thread::spawn(move || {
        let got = m2.park_enter_while(parker, std::time::Duration::from_secs(30), || true);
        if got {
            assert!(m2.exit_reporting_release(parker).unwrap());
        }
        got
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while m.entry_waiters.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "parker never parked");
        std::thread::yield_now();
    }
    assert!(
        m.successor_wake_needed(),
        "a running parker is owed a wake-up"
    );
    assert!(m.exit_reporting_release(owner).unwrap());
    assert!(
        h.join().unwrap(),
        "the release woke the parker, which took the monitor"
    );
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
    assert!(
        !m.succ_pending.load(Ordering::SeqCst),
        "the woken entrant cleared it"
    );
    assert_eq!(m.current_owner(), None);

    // Round 12 wave 1: a running park asked to stop (a pause requested)
    // gives up before it waits, taking nothing and leaving no
    // registration.
    assert!(m.try_enter(owner));
    let started = std::time::Instant::now();
    assert!(!m.park_enter_while(parker, std::time::Duration::from_secs(30), || false));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "it did not wait"
    );
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
    assert_eq!(m.current_owner(), Some(owner));
    assert!(m.exit_reporting_release(owner).unwrap());
}



/// Round 12 wave 1 (lane lock, W16-3 / W19-3): the spin a woken entrant
/// takes before re-parking wins a free monitor at once, as a plain
/// acquisition (entry count 1), and gives up on a held one within its
/// budget having taken nothing.
#[test]
fn spin_after_wake_takes_a_free_monitor_and_gives_up_on_a_held_one() {
    let m = Monitor::new();
    let (owner, woken) = (ThreadId(221), ThreadId(222));
    let me = owner_word(Some(woken));
    assert!(m.spin_after_wake(me));
    assert_eq!(m.current_owner(), Some(woken));
    assert_eq!(m.entry_count.load(Ordering::Relaxed), 1);
    assert!(m.exit_reporting_release(woken).unwrap());
    assert!(m.try_enter(owner));
    assert!(!m.spin_after_wake(me));
    assert_eq!(
        m.current_owner(),
        Some(owner),
        "a held monitor is never taken"
    );
    assert_eq!(
        m.spinners.load(Ordering::SeqCst),
        0,
        "not counted as a spinner"
    );
    assert!(m.exit_reporting_release(owner).unwrap());
}


/// Round 11 wave 19 (lock): the monitor's three cache lines -- nothing a
/// contender writes shares the owner word's line, and the state mutex
/// shares neither -- and the index epoch alone on its line.
/// `r12w8-monitor-wait-leaves-the-jfr-enter-flag-to-the-next-owner`: the
/// owner that takes the monitor while a JFR-recorded owner waits does not
/// inherit its enter flag, and the waiter gets it back.
#[test]
fn wait_hands_the_next_owner_no_jfr_enter_flag_and_restores_it() {
    let m = Arc::new(Monitor::new());
    let a = ThreadId(411);
    let b = ThreadId(412);
    m.block_enter(a);
    m.set_jfr_enter_recorded(a);
    assert!(m.jfr_enter_recorded());
    let other = Arc::clone(&m);
    let seen = std::thread::spawn(move || {
        other.block_enter(b);
        let inherited = other.jfr_enter_recorded();
        other.notify(b).unwrap();
        assert!(other.exit_reporting_release(b).unwrap());
        inherited
    });
    let _ = m.wait(a, Some(5_000), None, None, None).unwrap();
    assert!(
        !seen.join().unwrap(),
        "the next owner inherited the waiter's JFR flag"
    );
    assert_eq!(m.current_owner(), Some(a));
    assert!(
        m.jfr_enter_recorded(),
        "wait() restores the owner's JFR flag"
    );
    assert!(m.exit_reporting_release(a).unwrap());
}


/// The spin wins a monitor that is free, grows its budget when it does,
/// gives up on a monitor held for the whole budget, and halves the budget
/// when it does — never leaving `SPIN_MIN..=SPIN_MAX`.
#[test]
fn spin_try_enter_is_bounded_and_adapts() {
    let m = Monitor::new();
    let owner = ThreadId(1);
    let spinner = ThreadId(2);
    let budget = |m: &Monitor| m.spin_limit.load(Ordering::Relaxed);
    assert_eq!(budget(&m), SPIN_INITIAL);

    assert!(
        m.spin_try_enter_adaptive(spinner),
        "a free monitor is won on the first probe"
    );
    assert_eq!(budget(&m), SPIN_INITIAL + SPIN_BONUS);
    assert!(m.exit(spinner).is_ok());

    assert!(m.try_enter(owner));
    for _ in 0..32 {
        assert!(
            !m.spin_try_enter_adaptive(spinner),
            "an owned monitor is never won by spinning"
        );
        assert!((SPIN_MIN..=SPIN_MAX).contains(&budget(&m)));
    }
    assert_eq!(budget(&m), SPIN_MIN, "repeated failures decay to the floor");
    assert_eq!(
        m.current_owner(),
        Some(owner),
        "a failed spin takes nothing"
    );

    assert!(m.exit(owner).is_ok());
    for _ in 0..64 {
        assert!(m.spin_try_enter_adaptive(spinner));
        assert!(m.exit(spinner).is_ok());
    }
    assert_eq!(
        budget(&m),
        SPIN_MAX,
        "repeated successes grow to the ceiling"
    );
}




/// gen r4w5/thrash5: the contended-enter spin acquires a free monitor at
/// once, re-enters one it owns, and never takes one another thread owns.
#[test]
fn spin_try_enter_takes_free_and_owned_but_not_foreign_monitors() {
    let m = Monitor::new();
    let (me, other) = (ThreadId(11), ThreadId(12));
    assert!(m.spin_try_enter(me, || true), "a free monitor is acquired");
    assert_eq!(m.current_owner(), Some(me));
    assert!(
        m.spin_try_enter(me, || true),
        "an owned monitor is re-entered"
    );
    let mut polls = 0u32;
    assert!(
        !m.spin_try_enter(other, || {
            polls += 1;
            true
        }),
        "a monitor another thread holds is never acquired by the spin"
    );
    assert_eq!(polls, ENTER_SPIN_ROUNDS, "the spin is bounded");
    assert_eq!(m.current_owner(), Some(me));
    assert!(m.exit(me).is_ok());
    assert!(m.exit(me).is_ok());
    assert_eq!(m.current_owner(), None);
}

/// gen r4w5/thrash5: `keep_spinning` is polled before every attempt, so a
/// pending stop-the-world request ends the spin before the next try.
#[test]
fn spin_try_enter_stops_when_told_to() {
    let m = Monitor::new();
    assert!(!m.spin_try_enter(ThreadId(21), || false));
    assert_eq!(m.current_owner(), None, "no attempt after a stop");
}

/// gen r4w5/thrash5: the owner releasing during the spin is what the
/// spin is for; the spinner then owns the monitor without parking.
#[test]
fn spin_try_enter_acquires_once_the_owner_releases() {
    let m = Monitor::new();
    let (owner, spinner) = (ThreadId(31), ThreadId(32));
    assert!(m.spin_try_enter(owner, || true));
    let mut polls = 0u32;
    let got = m.spin_try_enter(spinner, || {
        polls += 1;
        if polls == 3 {
            // The owner leaves its critical section mid-spin.
            assert!(m.exit(owner).is_ok());
        }
        true
    });
    assert!(got, "the release is observed within the bound");
    assert_eq!(polls, 3);
    assert_eq!(m.current_owner(), Some(spinner));
}

/// Round 12 wave 2 (lane lock2, `CRATONVM_MONITOR_LAZY_SPINNERS`): a
/// spinner counts itself in `spinners` only while an entrant is parked
/// (the only case in which the count spares anyone a wake-up), and never
/// leaves the count behind; the parked entrant is still woken by the
/// owner's release.
#[test]
fn a_spinner_counts_itself_only_while_an_entrant_is_parked() {
    let m = Arc::new(Monitor::new());
    let (owner, spinner, parker) = (ThreadId(251), ThreadId(252), ThreadId(253));
    assert!(m.try_enter(owner));
    let mut seen: Vec<u32> = Vec::new();
    let got = m.spin_try_enter(spinner, || {
        seen.push(m.spinners.load(Ordering::SeqCst));
        seen.len() < 4
    });
    assert!(!got, "an owned monitor is never won");
    let expected = u32::from(!lazy_spinner_registration());
    assert!(
        seen.iter().all(|&n| n == expected),
        "nobody parked: {seen:?}"
    );
    assert_eq!(m.spinners.load(Ordering::SeqCst), 0);

    let m2 = Arc::clone(&m);
    let h = std::thread::spawn(move || {
        m2.block_enter(parker);
        assert!(m2.exit_reporting_release(parker).unwrap());
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while m.entry_waiters.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "parker never parked");
        std::thread::yield_now();
    }
    seen.clear();
    let got = m.spin_try_enter(spinner, || {
        seen.push(m.spinners.load(Ordering::SeqCst));
        seen.len() < 4
    });
    assert!(!got);
    assert!(
        seen.iter().all(|&n| n == 1),
        "an entrant parked: counted ({seen:?})"
    );
    assert_eq!(
        m.spinners.load(Ordering::SeqCst),
        0,
        "and uncounted on the way out"
    );
    assert!(m.exit_reporting_release(owner).unwrap());
    h.join().unwrap();
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
    assert_eq!(m.current_owner(), None);
}

/// gen r4w5/thrash5: the busy phase backs off exponentially, capped.
#[test]
fn enter_spin_hints_back_off_and_cap() {
    assert_eq!(enter_spin_hints(0), 1);
    assert_eq!(enter_spin_hints(3), 8);
    assert_eq!(enter_spin_hints(8), 256);
    assert_eq!(enter_spin_hints(15), 256);
    assert!(ENTER_SPIN_BUSY_ROUNDS < ENTER_SPIN_ROUNDS);
}





/// Round 11 wave 16 (lock): a carrier alternating between a handful of
/// virtual threads (consecutive `ThreadId`s) keeps every one's lease in
/// its thread-local cache instead of evicting on each mount switch.
#[test]
fn the_lease_cache_keeps_one_entry_per_alternating_thread() {
    let slots = LockSlots::new();
    let tids: Vec<ThreadId> = (0..LOCK_SLOT_CACHE_WAYS as u64)
        .map(|i| ThreadId(7_000 + i))
        .collect();
    let leased: Vec<u32> = tids
        .iter()
        .map(|&t| slots.slot_for(t).expect("free slots"))
        .collect();
    for _ in 0..3 {
        for (t, slot) in tids.iter().zip(&leased) {
            assert_eq!(slots.cached_slot(*t), Some(Some(*slot)));
        }
    }
}


/// Round 12 wave 8 (lane monitor): the lost-race back-off grows from its
/// floor, stops at its ceiling, never spends more than the room the
/// spin's budget has left, and does nothing (keeping the old budget
/// adaptation) when switched off.
#[test]
fn lost_cas_backoff_windows_grow_cap_and_fit_the_room() {
    let mut b = LostCasBackoff::new(true);
    assert!(!b.saw_a_handover());
    assert_eq!(b.after_loss(10_000), LOST_CAS_BACKOFF_MIN);
    assert_eq!(b.after_loss(10_000), LOST_CAS_BACKOFF_MIN * 2);
    for _ in 0..16 {
        assert!(b.after_loss(10_000) <= LOST_CAS_BACKOFF_MAX);
    }
    assert_eq!(b.after_loss(10_000), LOST_CAS_BACKOFF_MAX);
    assert_eq!(b.after_loss(3), 3, "never past the budget's room");
    assert_eq!(b.after_loss(0), 0);
    assert!(b.saw_a_handover());

    let mut off = LostCasBackoff::new(false);
    assert_eq!(off.after_loss(10_000), 0);
    assert!(
        !off.saw_a_handover(),
        "switched off, a failed spin halves the budget as before"
    );
}

/// Round 12 wave 8 (lane monitor, `CRATONVM_MONITOR_QUIET_RELEASE`): a
/// release that skips re-writing an already-clear `thin_seed` /
/// `jfr_enter_recorded` ends in exactly the state the unconditional
/// stores leave -- through a seeded inflation, re-entry, a JFR-recorded
/// outermost enter, `wait`'s release and a dead owner's forced release.
#[test]
fn a_quiet_release_ends_in_the_state_a_plain_one_does() {
    for quiet in [true, false] {
        let tuning = MonitorTuning {
            quiet_release: quiet,
            ..MonitorTuning::DEFAULT
        };
        let a = ThreadId(301);
        let m = Monitor::with_tuning(tuning);
        m.enter_with_recursion(a, 2);
        assert!(!m.exit_reporting_release(a).unwrap());
        assert_eq!(m.thin_seed_owner(), Some(a), "quiet={quiet}");
        assert!(m.exit_reporting_release(a).unwrap());
        assert_eq!(m.thin_seed_owner(), None, "quiet={quiet}");
        assert!(m.try_enter(a));
        assert!(m.exit_reporting_release(a).unwrap());
        assert_eq!(m.thin_seed_owner(), None, "quiet={quiet}");

        assert!(m.try_enter(a));
        m.set_jfr_enter_recorded(a);
        assert!(m.try_enter(a));
        assert!(!m.exit_reporting_release(a).unwrap());
        assert!(
            m.jfr_enter_recorded(),
            "an inner release keeps it, quiet={quiet}"
        );
        assert!(m.exit_reporting_release(a).unwrap());
        assert!(!m.jfr_enter_recorded(), "quiet={quiet}");
        assert!(m.try_enter(a));
        assert!(m.exit_reporting_release(a).unwrap());
        assert!(!m.jfr_enter_recorded(), "quiet={quiet}");
        assert_eq!(m.current_owner(), None);
        assert_eq!(m.entry_count.load(Ordering::Relaxed), 0);

        // `wait` releases through the same word and restores the depth.
        assert!(m.try_enter(a));
        assert!(m.try_enter(a));
        assert_eq!(
            m.wait(a, Some(1), None, None, None).unwrap(),
            WaitOutcome::TimedOutOrSpurious
        );
        assert_eq!(m.current_owner(), Some(a));
        assert_eq!(m.entry_count.load(Ordering::Relaxed), 2);
        assert!(!m.exit_reporting_release(a).unwrap());
        assert!(m.exit_reporting_release(a).unwrap());

        let f = Monitor::with_tuning(tuning);
        f.enter_with_recursion(a, 1);
        assert!(f.force_release_if_owned_by(a));
        assert_eq!(f.thin_seed_owner(), None, "quiet={quiet}");
        assert_eq!(f.current_owner(), None);
        assert!(f.try_enter(ThreadId(302)), "free for the next owner");
    }
}

/// Round 12 wave 8 (lane monitor): a crowd of Rust-side spinners that
/// back off after every lost race -- falling back to the real park when a
/// spin runs out -- still gets every thread through with the protected
/// counter exact (no double ownership, no lost wake-up), in both arms of
/// `CRATONVM_MONITOR_SPIN_BACKOFF`, and leaves the budget in range.
#[test]
fn backing_off_spinners_lose_no_update_and_no_wakeup() {
    const THREADS: u64 = 8;
    const ROUNDS: u64 = 3_000;
    for backoff in [true, false] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            lost_cas_backoff: backoff,
            ..MonitorTuning::DEFAULT
        }));
        let counter = Arc::new(AtomicU64::new(0));
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let m = Arc::clone(&m);
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    let me = ThreadId(400 + t);
                    for _ in 0..ROUNDS {
                        if !m.spin_try_enter_adaptive(me) {
                            m.block_enter(me);
                        }
                        // A torn read-modify-write would lose an update.
                        let v = counter.load(Ordering::Relaxed);
                        std::hint::spin_loop();
                        counter.store(v + 1, Ordering::Relaxed);
                        assert!(m.try_enter(me), "re-entry inside the section");
                        assert!(!m.exit_reporting_release(me).unwrap());
                        assert!(m.exit_reporting_release(me).unwrap());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            THREADS * ROUNDS,
            "backoff={backoff}"
        );
        assert_eq!(m.current_owner(), None);
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(m.spinners.load(Ordering::SeqCst), 0);
        assert!((SPIN_MIN..=SPIN_MAX).contains(&m.spin_limit.load(Ordering::Relaxed)));
    }
}



/// Round 13 wave 10 (lane monitor2, `CRATONVM_MONITOR_WAIT_REACQUIRE_SPIN`):
/// a wait/notify ping-pong -- every hand-over is a notified waiter
/// re-acquiring a monitor its notifier still holds -- alternates exactly,
/// returns from every `wait` owning the monitor at its depth, and leaves
/// no registered entrant behind, with the re-acquisition spin on and off.
#[test]
fn a_wait_notify_ping_pong_hands_over_exactly_in_both_reacquire_arms() {
    const ROUNDS: u64 = 2_000;
    for spin in [true, false] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            wait_reacquire_spin: spin,
            ..MonitorTuning::DEFAULT
        }));
        // Whose turn it is (0 or 1); read and written only while owning `m`.
        let turn = Arc::new(AtomicU64::new(0));
        let handoffs = Arc::new(AtomicU64::new(0));
        let players: Vec<_> = (0..2u64)
            .map(|side| {
                let m = Arc::clone(&m);
                let turn = Arc::clone(&turn);
                let handoffs = Arc::clone(&handoffs);
                std::thread::spawn(move || {
                    let me = ThreadId(500 + side);
                    let never = std::sync::atomic::AtomicBool::new(false);
                    for _ in 0..ROUNDS {
                        m.enter(me);
                        assert!(m.try_enter(me), "re-entry: `wait` must restore depth 2");
                        while turn.load(Ordering::Relaxed) != side {
                            let outcome = m.wait(me, None, Some(&never), None, None).unwrap();
                            assert_ne!(outcome, WaitOutcome::Interrupted);
                            assert_eq!(m.current_owner(), Some(me), "spin={spin}");
                            assert_eq!(m.entry_count.load(Ordering::Relaxed), 2);
                        }
                        handoffs.fetch_add(1, Ordering::Relaxed);
                        turn.store(1 - side, Ordering::Relaxed);
                        m.notify(me).unwrap();
                        assert!(!m.exit_reporting_release(me).unwrap());
                        assert!(m.exit_reporting_release(me).unwrap());
                    }
                })
            })
            .collect();
        for p in players {
            p.join().unwrap();
        }
        assert_eq!(handoffs.load(Ordering::Relaxed), 2 * ROUNDS, "spin={spin}");
        assert_eq!(m.current_owner(), None);
        assert_eq!(m.entry_count.load(Ordering::Relaxed), 0);
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0, "spin={spin}");
    }
}

/// JIT round 16 (lane monitor, MON14-5, `CRATONVM_MONITOR_NOTIFY_DEFER_WAKE`):
/// with the wake deferred to the release, a two-way ping-pong (`notify`)
/// and a four-way ring (`notifyAll`, three waiters re-checking per turn)
/// hand over exactly, every `wait` returns owning the monitor at its
/// depth, and no registration or queued wake is left behind.
#[test]
fn mon14_5_deferred_notify_ping_pong_and_ring_hand_over_exactly() {
    const ROUNDS: u64 = 1_000;
    for parties in [2u64, 4] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            notify_defers_wake: true,
            ..MonitorTuning::DEFAULT
        }));
        let turn = Arc::new(AtomicU64::new(0));
        let handoffs = Arc::new(AtomicU64::new(0));
        let players: Vec<_> = (0..parties)
            .map(|side| {
                let m = Arc::clone(&m);
                let turn = Arc::clone(&turn);
                let handoffs = Arc::clone(&handoffs);
                std::thread::spawn(move || {
                    let me = ThreadId(1600 + side);
                    let never = std::sync::atomic::AtomicBool::new(false);
                    for _ in 0..ROUNDS {
                        m.enter(me);
                        assert!(m.try_enter(me), "re-entry: `wait` must restore depth 2");
                        while turn.load(Ordering::Relaxed) != side {
                            let outcome = m.wait(me, None, Some(&never), None, None).unwrap();
                            assert_eq!(outcome, WaitOutcome::Notified, "parties={parties}");
                            assert_eq!(m.current_owner(), Some(me));
                            assert_eq!(m.entry_count.load(Ordering::Relaxed), 2);
                        }
                        handoffs.fetch_add(1, Ordering::Relaxed);
                        turn.store((side + 1) % parties, Ordering::Relaxed);
                        if parties == 2 {
                            m.notify(me).unwrap();
                        } else {
                            m.notify_all(me).unwrap();
                        }
                        assert!(!m.exit_reporting_release(me).unwrap());
                        assert!(m.exit_reporting_release(me).unwrap());
                    }
                })
            })
            .collect();
        for p in players {
            p.join().unwrap();
        }
        assert_eq!(handoffs.load(Ordering::Relaxed), parties * ROUNDS);
        assert_eq!(m.current_owner(), None);
        assert_eq!(
            m.entry_waiters.load(Ordering::SeqCst),
            0,
            "parties={parties}"
        );
        let s = m.state.lock();
        assert!(s.deferred_wakes.is_empty() && s.wait_queue.is_empty());
        assert_eq!((s.pending_notifies, s.parked_waiters), (0, 0));
    }
}

/// MON14-5: a deferred notify registers the waiter as an entrant and
/// wakes nobody while the notifier holds the monitor; the notifier's
/// release wakes it, and it returns notified, owning the monitor. A
/// timed waiter whose deadline passes while its wake is still queued
/// finds its notification, waits for the release as a registered entrant,
/// and also returns notified.
#[test]
fn mon14_5_a_deferred_notify_wakes_the_waiter_only_at_the_release() {
    for timeout_ms in [None, Some(20u64)] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            notify_defers_wake: true,
            ..MonitorTuning::DEFAULT
        }));
        let (w, n) = (ThreadId(1611), ThreadId(1612));
        let returned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let m = Arc::clone(&m);
            let returned = Arc::clone(&returned);
            std::thread::spawn(move || {
                let never = std::sync::atomic::AtomicBool::new(false);
                m.enter(w);
                let outcome = m.wait(w, timeout_ms, Some(&never), None, None).unwrap();
                returned.store(true, Ordering::SeqCst);
                assert_eq!(outcome, WaitOutcome::Notified, "timeout={timeout_ms:?}");
                assert_eq!(m.current_owner(), Some(w));
                assert!(m.exit_reporting_release(w).unwrap());
            })
        };
        // The waiter is in the wait set.
        while m.state.lock().wait_queue.is_empty() {
            std::thread::yield_now();
        }
        m.enter(n);
        m.notify(n).unwrap();
        {
            let s = m.state.lock();
            assert_eq!(s.deferred_wakes.len(), 1, "queued, not signalled");
            assert!(s.wait_queue.is_empty());
        }
        assert_eq!(
            m.entry_waiters.load(Ordering::SeqCst),
            1,
            "registered by the notify"
        );
        // Held well past the timed waiter's deadline and any spin.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !returned.load(Ordering::SeqCst),
            "no return while the notifier holds it"
        );
        assert!(m.exit_reporting_release(n).unwrap());
        waiter.join().unwrap();
        assert!(returned.load(Ordering::SeqCst));
        assert_eq!(m.current_owner(), None);
        assert_eq!(
            m.entry_waiters.load(Ordering::SeqCst),
            0,
            "timeout={timeout_ms:?}"
        );
        assert!(
            !m.succ_pending.load(Ordering::SeqCst),
            "the successor cleared its flag"
        );
        assert!(m.state.lock().deferred_wakes.is_empty());
    }
}

/// MON14-5 at the state level: the deferring notify marks exactly what
/// the plain one marks, queues (and registers) only waiters with a
/// condvar of their own, and signals none of them; a release pops in
/// notify order.
#[test]
fn mon14_5_the_deferring_notify_queues_only_own_condvar_waiters() {
    let m = Monitor::with_tuning(MonitorTuning {
        notify_defers_wake: true,
        ..MonitorTuning::DEFAULT
    });
    let mut s = m.state.lock();
    let t0 = s.enroll_waiter(Some(Arc::new(Condvar::new())), ThreadId(1620));
    let t1 = s.enroll_wait_ticket();
    let t2 = s.enroll_waiter(Some(Arc::new(Condvar::new())), ThreadId(1622));
    let t3 = s.enroll_waiter(Some(Arc::new(Condvar::new())), ThreadId(1623));
    assert_eq!(
        s.notify_longest_waiter_deferring(&m.entry_waiters),
        Some(false)
    );
    assert_eq!(
        s.notify_longest_waiter_deferring(&m.entry_waiters),
        Some(true)
    );
    assert_eq!(
        m.entry_waiters.load(Ordering::SeqCst),
        1,
        "only the own-condvar waiter"
    );
    assert_eq!(s.notify_every_waiter_deferring(&m.entry_waiters), 2);
    assert_eq!(s.notify_longest_waiter_deferring(&m.entry_waiters), None);
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 3);
    assert_eq!(s.pending_notifies, 4);
    let queued: Vec<u64> = s.deferred_wakes.iter().map(|d| d.ticket).collect();
    assert_eq!(queued, vec![t0, t2, t3], "notify order");
    // A waiter that woke on its own takes its queued wake out.
    s.drop_deferred_wake(t2);
    let queued: Vec<u64> = s.deferred_wakes.iter().map(|d| d.ticket).collect();
    assert_eq!(queued, vec![t0, t3]);
    // A release that owes a wake (nobody parked on `entry_condvar`) pops
    // the oldest and marks a successor on its way.
    m.wake_successor_locked(&mut s);
    assert!(m.succ_pending.load(Ordering::SeqCst));
    let queued: Vec<u64> = s.deferred_wakes.iter().map(|d| d.ticket).collect();
    assert_eq!(queued, vec![t3]);
    // ... and the next release, with that successor still on its way, none.
    m.wake_successor_locked(&mut s);
    assert_eq!(s.deferred_wakes.len(), 1);
    for t in [t0, t1, t2, t3] {
        assert!(s.take_notification(t));
    }
}

/// JIT round 16 (lane monitor, `CRATONVM_MONITOR_ENTER_SPIN_YIELDS`): a
/// `spin_try_enter` that never gets the monitor runs every round with the
/// yield rounds on and only the busy rounds with them off (it asks
/// `keep_spinning` once per round), and fails either way without taking
/// the monitor.
#[test]
fn r16_enter_spin_yield_rounds_follow_their_switch() {
    for (yields, rounds) in [(true, ENTER_SPIN_ROUNDS), (false, ENTER_SPIN_BUSY_ROUNDS)] {
        let m = Monitor::with_tuning(MonitorTuning {
            enter_spin_yields: yields,
            ..MonitorTuning::DEFAULT
        });
        let (owner, contender) = (ThreadId(1631), ThreadId(1632));
        m.enter(owner);
        let mut asked = 0u32;
        assert!(!m.spin_try_enter(contender, || {
            asked += 1;
            true
        }));
        assert_eq!(asked, rounds, "yields={yields}");
        assert_eq!(m.current_owner(), Some(owner));
        assert!(m.exit_reporting_release(owner).unwrap());
    }
}

/// JIT round 16 (lane monitor, MON16-1): the wait prologue runs once, with
/// the monitor already released and the waiter already in the wait set;
/// a notify that another thread completes DURING the prologue (enter,
/// notify, exit) is not lost -- the wait ends notified without parking
/// (its first park would be the 250 ms safety slice) and re-owns the
/// monitor -- with the notify's wake deferred (MON14-5) or not.
#[test]
fn mon16_1_a_notify_during_the_prologue_ends_the_wait_without_a_park() {
    for defer in [false, true] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            notify_defers_wake: defer,
            ..MonitorTuning::DEFAULT
        }));
        let (w, n) = (ThreadId(1651), ThreadId(1652));
        let never = std::sync::atomic::AtomicBool::new(false);
        m.enter(w);
        let mut ran = 0u32;
        let started = std::time::Instant::now();
        let mut prologue = || {
            ran += 1;
            assert_eq!(m.current_owner(), None, "released before the prologue");
            assert_eq!(m.state.lock().wait_queue.len(), 1, "enrolled before it");
            let notifier = Arc::clone(&m);
            std::thread::spawn(move || {
                notifier.enter(n);
                notifier.notify(n).unwrap();
                assert!(notifier.exit_reporting_release(n).unwrap());
            })
            .join()
            .unwrap();
        };
        let outcome = m
            .wait_with_prologue(
                w,
                None,
                Some(&never),
                None,
                None,
                Some(&mut prologue as &mut dyn FnMut()),
            )
            .unwrap();
        assert_eq!(outcome, WaitOutcome::Notified, "defer={defer}");
        assert_eq!(ran, 1);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "no park: defer={defer}"
        );
        assert_eq!(m.current_owner(), Some(w));
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0, "defer={defer}");
        assert!(m.exit_reporting_release(w).unwrap());
        let s = m.state.lock();
        assert!(s.deferred_wakes.is_empty() && s.wait_queue.is_empty());
        assert_eq!((s.pending_notifies, s.parked_waiters), (0, 0));
    }
}

/// MON16-1: an interrupt set while the prologue runs ends the wait
/// `Interrupted` without a park, whatever the enrolment re-check switch
/// says; a wait whose caller does not own the monitor fails before the
/// prologue would run.
#[test]
fn mon16_1_an_interrupt_during_the_prologue_is_seen_and_a_non_owner_never_runs_it() {
    for enrol_check in [true, false] {
        let m = Monitor::with_tuning(MonitorTuning {
            wait_enrol_interrupt_check: enrol_check,
            ..MonitorTuning::DEFAULT
        });
        let w = ThreadId(1653);
        let flag = std::sync::atomic::AtomicBool::new(false);
        m.enter(w);
        let started = std::time::Instant::now();
        let mut prologue = || flag.store(true, Ordering::SeqCst);
        let outcome = m
            .wait_with_prologue(
                w,
                None,
                Some(&flag),
                None,
                None,
                Some(&mut prologue as &mut dyn FnMut()),
            )
            .unwrap();
        assert_eq!(
            outcome,
            WaitOutcome::Interrupted,
            "enrol_check={enrol_check}"
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert_eq!(m.current_owner(), Some(w));
        assert!(m.exit_reporting_release(w).unwrap());
        assert!(m.state.lock().wait_queue.is_empty());
    }
    let m = Monitor::new();
    let mut ran = false;
    let mut prologue = || ran = true;
    let result = m.wait_with_prologue(
        ThreadId(1654),
        None,
        None,
        None,
        None,
        Some(&mut prologue as &mut dyn FnMut()),
    );
    assert!(matches!(result, Err(MonitorError::NotOwner)));
    assert!(!ran, "the prologue must not run for a non-owner");
}

/// Runs `Monitor::wait_lazily` for `w` (which must own `m`) with a counting
/// prologue; `(outcome, prologue runs, elapsed)`.
fn mon16_6_lazy_wait(
    m: &Monitor,
    w: ThreadId,
    timeout_ms: Option<u64>,
    budget_ms: u64,
    keep_running: &dyn Fn() -> bool,
    on_prologue: &dyn Fn(),
) -> (WaitOutcome, u32, std::time::Duration) {
    let never = std::sync::atomic::AtomicBool::new(false);
    let mut runs = 0u32;
    let mut prologue = || {
        runs += 1;
        on_prologue();
    };
    let started = std::time::Instant::now();
    let outcome = m
        .wait_lazily(
            w,
            timeout_ms,
            Some(&never),
            None,
            None,
            Some(&mut prologue as &mut dyn FnMut()),
            Some(LazyWait {
                budget: std::time::Duration::from_millis(budget_ms),
                keep_running,
            }),
        )
        .unwrap();
    (outcome, runs, started.elapsed())
}

/// Enter, notify and exit `m` as `n` once `m`'s wait set is non-empty
/// and `ready()` says so.
fn mon16_6_notifier(
    m: &Arc<Monitor>,
    n: ThreadId,
    delay_ms: u64,
    ready: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let m = Arc::clone(m);
    std::thread::spawn(move || {
        while m.state.lock().wait_queue.is_empty() || !ready.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        m.enter(n);
        m.notify(n).unwrap();
        assert!(m.exit_reporting_release(n).unwrap());
    })
}

/// JIT round 16 wave 5 (lane monitor, MON16-6): a waiter notified in its
/// running phase takes the monitor and returns without ever running the
/// prologue (it never blocked) -- at least once in a few tries: it must
/// also win the monitor from its notifier by CAS or a bounded spin, which
/// a descheduled notifier can deny it, and then it takes the full path,
/// still notified; one whose running phase runs out first runs the
/// prologue exactly once and is still notified.
///
/// Interpreter round i2 wave 54 (lane L7): "a few tries" was five, and
/// the Linux host's parallel `cargo test` lost all five (a notifier
/// descheduled between its `notify` and its exit outlasts the bounded
/// spin each time), so the test was red on `dev` with nothing wrong in
/// the monitor. It now tries until one never blocks, up to
/// `MON16_6_TRIES`, and the ran-out case runs after it either way.
#[test]
fn mon16_6_a_waiter_notified_while_running_never_blocks() {
    const MON16_6_TRIES: usize = 200;
    let run_case = |budget_ms: u64, delay_ms: u64| -> u32 {
        let m = Arc::new(Monitor::new());
        let (w, n) = (ThreadId(1661), ThreadId(1662));
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(true));
        m.enter(w);
        let notifier = mon16_6_notifier(&m, n, delay_ms, ready);
        let (outcome, runs, _) = mon16_6_lazy_wait(&m, w, None, budget_ms, &|| true, &|| {});
        notifier.join().unwrap();
        assert_eq!(outcome, WaitOutcome::Notified, "budget={budget_ms}");
        assert!(runs <= 1);
        assert_eq!(m.current_owner(), Some(w));
        assert_eq!(m.lazy_waiters.load(Ordering::SeqCst), 0);
        assert!(m.exit_reporting_release(w).unwrap());
        let s = m.state.lock();
        assert!(s.wait_queue.is_empty() && s.notified_tickets.is_empty());
        assert_eq!((s.pending_notifies, s.parked_waiters), (0, 0));
        runs
    };
    let never_blocked = (0..MON16_6_TRIES).any(|_| run_case(10_000, 0) == 0);
    assert_eq!(
        run_case(20, 150),
        1,
        "a running phase that ran out blocks once"
    );
    assert!(
        never_blocked,
        "no try in {MON16_6_TRIES} returned without blocking"
    );
}

/// Counts [`WaitReacquireReport::blocked_on_reenter`] calls.
struct CountingReenter(std::sync::atomic::AtomicU32);

impl WaitReacquireReport for CountingReenter {
    fn blocked_on_reenter(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Interpreter round i2 wave 54 (lane L7,
/// `L7W29TWaitNotify` rows 3-4): a notified waiter whose notifier keeps
/// the monitor reports BLOCKED exactly once for its re-acquisition
/// (`WaitReacquireReport`, which the VM uses to write `threadStatus`
/// too); a timed-out waiter that finds the monitor free reports nothing
/// (the uncontended re-acquire does no extra work).
#[test]
fn i54_l7_a_contended_reacquire_reports_blocked_once_and_a_free_one_never() {
    let m = Arc::new(Monitor::new());
    let (w, n) = (ThreadId(5471), ThreadId(5472));
    let report = CountingReenter(std::sync::atomic::AtomicU32::new(0));
    m.enter(w);
    let notifier = {
        let m = Arc::clone(&m);
        std::thread::spawn(move || {
            while m.state.lock().wait_queue.is_empty() {
                std::thread::yield_now();
            }
            m.enter(n);
            m.notify(n).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(m.exit_reporting_release(n).unwrap());
        })
    };
    let outcome = m.wait(w, None, None, None, Some(&report)).unwrap();
    notifier.join().unwrap();
    assert_eq!(outcome, WaitOutcome::Notified);
    assert_eq!(
        report.0.load(Ordering::SeqCst),
        1,
        "a held monitor: BLOCKED once"
    );
    assert_eq!(m.current_owner(), Some(w));
    assert!(m.exit_reporting_release(w).unwrap());

    let free = CountingReenter(std::sync::atomic::AtomicU32::new(0));
    m.enter(w);
    let outcome = m.wait(w, Some(5), None, None, Some(&free)).unwrap();
    assert_eq!(outcome, WaitOutcome::TimedOutOrSpurious);
    assert_eq!(
        free.0.load(Ordering::SeqCst),
        0,
        "a free monitor: no report"
    );
    assert!(m.exit_reporting_release(w).unwrap());
}

/// A report that holds the waiter off once: its give-back sees the
/// FRESH hold (count 1), releases it, lets `other` take and leave the
/// monitor meanwhile, and enters again.
struct HoldOffOnce {
    held: std::sync::atomic::AtomicBool,
    other: ThreadId,
    other_got_it: std::sync::atomic::AtomicBool,
}

impl WaitReacquireReport for HoldOffOnce {
    fn blocked_on_reenter(&self) {}

    fn reentry_held_off(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    fn give_back_while_held_off(&self, monitor: &Monitor, thread_id: ThreadId) {
        assert_eq!(
            monitor.entry_count.load(Ordering::Relaxed),
            1,
            "a fresh hold"
        );
        self.held.store(false, Ordering::SeqCst);
        assert!(monitor.exit_reporting_release(thread_id).unwrap());
        // Another thread gets the lock while this one is held off.
        let other = self.other;
        std::thread::scope(|s| {
            s.spawn(|| {
                monitor.enter(other);
                self.other_got_it.store(true, Ordering::SeqCst);
                assert!(monitor.exit_reporting_release(other).unwrap());
            });
        });
        monitor.block_enter(thread_id);
    }
}

/// Interpreter round i2 wave 55 (lane L6,
/// `L6W54JdiSuspendedWaiterReentry`): a notified waiter the debugger
/// holds suspended gives the monitor it has just re-acquired back, and
/// gets its saved recursion back after it enters again.
#[test]
fn i55_l6_a_held_off_waiter_gives_its_reacquired_monitor_back() {
    let m = Arc::new(Monitor::new());
    let (w, n) = (ThreadId(5571), ThreadId(5572));
    let report = HoldOffOnce {
        held: std::sync::atomic::AtomicBool::new(true),
        other: ThreadId(5573),
        other_got_it: std::sync::atomic::AtomicBool::new(false),
    };
    m.enter(w);
    m.enter(w);
    let notifier = {
        let m = Arc::clone(&m);
        std::thread::spawn(move || {
            while m.state.lock().wait_queue.is_empty() {
                std::thread::yield_now();
            }
            m.enter(n);
            m.notify(n).unwrap();
            assert!(m.exit_reporting_release(n).unwrap());
        })
    };
    let outcome = m.wait(w, None, None, None, Some(&report)).unwrap();
    notifier.join().unwrap();
    assert_eq!(outcome, WaitOutcome::Notified);
    assert!(
        report.other_got_it.load(Ordering::SeqCst),
        "the lock was given back"
    );
    assert_eq!(m.current_owner(), Some(w));
    assert!(
        !m.exit_reporting_release(w).unwrap(),
        "the saved recursion is back"
    );
    assert!(m.exit_reporting_release(w).unwrap());
}

/// An [`EntryHold`] that holds its thread off once; while held off,
/// `other` takes and leaves the monitor (it could not if the hold had
/// been kept). Counts the waits.
struct EntryHoldOnce {
    held: std::sync::atomic::AtomicBool,
    other: ThreadId,
    monitor: Arc<Monitor>,
    other_got_it: std::sync::atomic::AtomicBool,
    waits: std::sync::atomic::AtomicU32,
}

impl EntryHold for EntryHoldOnce {
    fn held_off(&self, _thread_id: ThreadId) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    fn wait_until_released(&self, _thread_id: ThreadId) {
        self.waits.fetch_add(1, Ordering::SeqCst);
        let (monitor, other) = (&self.monitor, self.other);
        std::thread::scope(|s| {
            s.spawn(|| {
                monitor.enter(other);
                self.other_got_it.store(true, Ordering::SeqCst);
                assert!(monitor.exit_reporting_release(other).unwrap());
            });
        });
        self.held.store(false, Ordering::SeqCst);
    }
}

impl WaitReacquireReport for EntryHoldOnce {
    fn blocked_on_reenter(&self) {}

    // The wave-55 pair is not asked when `entry_hold` answers: holding
    // off here would leave `waits` at 0.
    fn reentry_held_off(&self) -> bool {
        false
    }

    fn entry_hold(&self) -> Option<&dyn EntryHold> {
        Some(self)
    }
}

fn entry_hold_once(monitor: &Arc<Monitor>, other: ThreadId) -> EntryHoldOnce {
    EntryHoldOnce {
        held: std::sync::atomic::AtomicBool::new(true),
        other,
        monitor: Arc::clone(monitor),
        other_got_it: std::sync::atomic::AtomicBool::new(false),
        waits: std::sync::atomic::AtomicU32::new(0),
    }
}

/// Interpreter round i2 wave 56 (lane L6,
/// `CRATONVM_MONITOR_SUSPENSION_AWARE_ENTRY`): a contended entry whose
/// thread is held off when it wins gives the fresh hold back, lets
/// another thread through, and owns the monitor once released.
#[test]
fn i56_l6_block_enter_with_gives_a_held_off_win_back() {
    let m = Arc::new(Monitor::new());
    let (e, h) = (ThreadId(5671), ThreadId(5672));
    let hold = entry_hold_once(&m, ThreadId(5673));
    m.enter(h);
    let releaser = {
        let m = Arc::clone(&m);
        std::thread::spawn(move || {
            while m.entry_waiters.load(Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            assert!(m.exit_reporting_release(h).unwrap());
        })
    };
    let given_back = m.block_enter_with(e, None, &hold);
    releaser.join().unwrap();
    assert_eq!(given_back, 1);
    assert_eq!(hold.waits.load(Ordering::SeqCst), 1);
    assert!(
        hold.other_got_it.load(Ordering::SeqCst),
        "the lock was given back"
    );
    assert_eq!(m.current_owner(), Some(e));
    assert!(
        m.exit_reporting_release(e).unwrap(),
        "one level, the fresh one"
    );
}

/// Wave 56 (lane L6): a hold that is not fresh -- a re-entrant level the
/// thread took itself -- is never given back.
#[test]
fn i56_l6_a_hold_that_is_not_fresh_is_kept() {
    let m = Arc::new(Monitor::new());
    let e = ThreadId(5674);
    let hold = entry_hold_once(&m, ThreadId(5675));
    m.enter(e);
    m.enter(e);
    assert_eq!(m.give_back_fresh_hold_while_held_off(e, &hold), 0);
    assert_eq!(hold.waits.load(Ordering::SeqCst), 0);
    assert!(!m.exit_reporting_release(e).unwrap());
    assert!(m.exit_reporting_release(e).unwrap());
}

/// Wave 56 (lane L6): `Object.wait`'s re-entry runs the same loop when
/// its report answers an [`EntryHold`], and gets its saved recursion
/// back after it enters again.
#[test]
fn i56_l6_a_held_off_waiter_gives_back_through_the_entry_loop() {
    let m = Arc::new(Monitor::new());
    let (w, n) = (ThreadId(5676), ThreadId(5677));
    let report = entry_hold_once(&m, ThreadId(5678));
    m.enter(w);
    m.enter(w);
    let notifier = {
        let m = Arc::clone(&m);
        std::thread::spawn(move || {
            while m.state.lock().wait_queue.is_empty() {
                std::thread::yield_now();
            }
            m.enter(n);
            m.notify(n).unwrap();
            assert!(m.exit_reporting_release(n).unwrap());
        })
    };
    let outcome = m.wait(w, None, None, None, Some(&report)).unwrap();
    notifier.join().unwrap();
    assert_eq!(outcome, WaitOutcome::Notified);
    assert_eq!(report.waits.load(Ordering::SeqCst), 1);
    assert!(
        report.other_got_it.load(Ordering::SeqCst),
        "the lock was given back"
    );
    assert_eq!(m.current_owner(), Some(w));
    assert!(
        !m.exit_reporting_release(w).unwrap(),
        "the saved recursion is back"
    );
    assert!(m.exit_reporting_release(w).unwrap());
}

/// [`EntryHoldOnce`] that counts its BLOCKED reports.
struct CountingEntryHold {
    hold: EntryHoldOnce,
    blocked: std::sync::atomic::AtomicU32,
}

impl WaitReacquireReport for CountingEntryHold {
    fn blocked_on_reenter(&self) {
        self.blocked.fetch_add(1, Ordering::SeqCst);
    }

    fn entry_hold(&self) -> Option<&dyn EntryHold> {
        Some(&self.hold)
    }
}

/// Interpreter round i2 wave 57, lane L6 (`L1W45JdiSuspendedInNative`):
/// a held-off waiter whose timed wait EXPIRED on a free monitor gives the
/// fresh hold back without reporting BLOCKED -- HotSpot's suspended
/// waiter keeps its wait state until it is resumed -- while a NOTIFIED
/// one reports BLOCKED, as HotSpot's notify begins its re-entry.
#[test]
fn i57_l6_a_held_off_timed_out_waiter_keeps_its_wait_state() {
    let m = Arc::new(Monitor::new());
    let w = ThreadId(5771);
    let timed = CountingEntryHold {
        hold: entry_hold_once(&m, ThreadId(5772)),
        blocked: std::sync::atomic::AtomicU32::new(0),
    };
    m.enter(w);
    let outcome = m.wait(w, Some(5), None, None, Some(&timed)).unwrap();
    assert_eq!(outcome, WaitOutcome::TimedOutOrSpurious);
    assert_eq!(
        timed.hold.waits.load(Ordering::SeqCst),
        1,
        "given back once"
    );
    assert_eq!(timed.blocked.load(Ordering::SeqCst), 0, "still WAITING");
    assert!(m.exit_reporting_release(w).unwrap());

    let n = ThreadId(5773);
    let notified = CountingEntryHold {
        hold: entry_hold_once(&m, ThreadId(5774)),
        blocked: std::sync::atomic::AtomicU32::new(0),
    };
    m.enter(w);
    let notifier = {
        let m = Arc::clone(&m);
        std::thread::spawn(move || {
            while m.state.lock().wait_queue.is_empty() {
                std::thread::yield_now();
            }
            m.enter(n);
            m.notify(n).unwrap();
            assert!(m.exit_reporting_release(n).unwrap());
        })
    };
    let outcome = m.wait(w, None, None, None, Some(&notified)).unwrap();
    notifier.join().unwrap();
    assert_eq!(outcome, WaitOutcome::Notified);
    assert_eq!(notified.hold.waits.load(Ordering::SeqCst), 1);
    assert!(
        notified.blocked.load(Ordering::SeqCst) >= 1,
        "BLOCKED once notified"
    );
    assert!(m.exit_reporting_release(w).unwrap());
}

/// Enter and notify `m` as `n` once `m`'s wait set is non-empty, keep the
/// monitor `hold_ms` -- far past any re-acquisition spin -- then exit.
fn lazy_reacquire_notifier(
    m: &Arc<Monitor>,
    n: ThreadId,
    hold_ms: u64,
) -> std::thread::JoinHandle<()> {
    let m = Arc::clone(m);
    std::thread::spawn(move || {
        while m.state.lock().wait_queue.is_empty() {
            std::thread::yield_now();
        }
        m.enter(n);
        m.notify(n).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        assert!(m.exit_reporting_release(n).unwrap());
    })
}

/// `MonitorTuning::wait_lazy_reacquire` (page
/// `r16-monitor-wait-notify-handovers-are-10-to-150x-hotspot-FIXED-20261006`): a lazy
/// waiter notified while its notifier keeps the monitor queues as a
/// running entrant and takes the monitor at the release without ever
/// running the prologue; with the switch off it blocks once, as before.
/// Either way it ends notified, owning the monitor, with no registration
/// left behind.
#[test]
fn a_lazy_waiter_notified_into_a_held_monitor_takes_it_without_blocking() {
    for reacquire in [true, false] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            wait_lazy_reacquire: reacquire,
            ..MonitorTuning::DEFAULT
        }));
        let (w, n) = (ThreadId(1671), ThreadId(1672));
        m.enter(w);
        let notifier = lazy_reacquire_notifier(&m, n, 40);
        let (outcome, runs, _) = mon16_6_lazy_wait(&m, w, None, 10_000, &|| true, &|| {});
        notifier.join().unwrap();
        assert_eq!(outcome, WaitOutcome::Notified, "reacquire={reacquire}");
        assert_eq!(runs, u32::from(!reacquire), "reacquire={reacquire}");
        assert_eq!(m.current_owner(), Some(w));
        assert_eq!(m.lazy_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert!(m.exit_reporting_release(w).unwrap());
        let s = m.state.lock();
        assert!(s.wait_queue.is_empty() && s.notified_tickets.is_empty());
        assert_eq!((s.pending_notifies, s.parked_waiters), (0, 0));
    }
}

/// ... and a deferring notify's waiter (MON14-5, already a registered
/// entrant) takes the monitor at its notifier's release still running:
/// woken once, by that release, it never runs the prologue, and leaves
/// neither its registration nor its queued wake behind.
#[test]
fn a_deferred_lazy_waiter_takes_the_monitor_at_the_release() {
    let m = Arc::new(Monitor::with_tuning(MonitorTuning {
        notify_defers_wake: true,
        ..MonitorTuning::DEFAULT
    }));
    let (w, n) = (ThreadId(1675), ThreadId(1676));
    m.enter(w);
    let notifier = lazy_reacquire_notifier(&m, n, 40);
    let (outcome, runs, _) = mon16_6_lazy_wait(&m, w, None, 10_000, &|| true, &|| {});
    notifier.join().unwrap();
    assert_eq!((outcome, runs), (WaitOutcome::Notified, 0));
    assert_eq!(m.current_owner(), Some(w));
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
    assert_eq!(m.lazy_waiters.load(Ordering::SeqCst), 0);
    assert!(m.exit_reporting_release(w).unwrap());
    let s = m.state.lock();
    assert!(s.deferred_wakes.is_empty() && s.wait_queue.is_empty());
    assert_eq!((s.pending_notifies, s.parked_waiters), (0, 0));
}

/// ... and a pause requested while it queues ends the running
/// re-acquisition at the initiator's `entry_condvar` wake
/// (`MonitorTable::wake_lazy_parkers` notifies every monitor with a lazy
/// waiter): it deregisters, runs the prologue once and re-acquires on the
/// blocking path, still notified.
#[test]
fn a_requested_pause_ends_the_running_reacquisition() {
    let m = Arc::new(Monitor::new());
    let (w, n) = (ThreadId(1673), ThreadId(1674));
    let pause = Arc::new(std::sync::atomic::AtomicBool::new(false));
    m.enter(w);
    let notifier = lazy_reacquire_notifier(&m, n, 300);
    let initiator = {
        let (m, pause) = (Arc::clone(&m), Arc::clone(&pause));
        std::thread::spawn(move || {
            // Queued as a running entrant: notified, counted lazy.
            while m.entry_waiters.load(Ordering::SeqCst) == 0
                || m.lazy_waiters.load(Ordering::SeqCst) == 0
            {
                std::thread::yield_now();
            }
            pause.store(true, Ordering::SeqCst);
            std::sync::atomic::fence(Ordering::SeqCst);
            let _state = m.state.lock();
            m.entry_condvar.notify_all();
        })
    };
    let keep_running = || !pause.load(Ordering::SeqCst);
    let (outcome, runs, _) = mon16_6_lazy_wait(&m, w, None, 60_000, &keep_running, &|| {});
    initiator.join().unwrap();
    notifier.join().unwrap();
    assert_eq!((outcome, runs), (WaitOutcome::Notified, 1));
    assert_eq!(m.current_owner(), Some(w));
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
    assert!(m.exit_reporting_release(w).unwrap());
}

/// MON16-6: a pause requested during the running phase ends it at the
/// initiator's wake (`wake_lazy_waiters`), long before its budget: the
/// prologue runs and the wait goes on to its notification.
#[test]
fn mon16_6_a_requested_pause_ends_the_running_phase() {
    let m = Arc::new(Monitor::new());
    let (w, n) = (ThreadId(1663), ThreadId(1664));
    let pause = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    m.enter(w);
    let initiator = {
        let (m, pause) = (Arc::clone(&m), Arc::clone(&pause));
        std::thread::spawn(move || {
            while m.lazy_waiters.load(Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            pause.store(true, Ordering::SeqCst);
            std::sync::atomic::fence(Ordering::SeqCst);
            m.wake_lazy_waiters();
        })
    };
    let notifier = mon16_6_notifier(&m, n, 0, Arc::clone(&ran));
    let keep_running = || !pause.load(Ordering::SeqCst);
    let on_prologue = || ran.store(true, Ordering::SeqCst);
    let (outcome, runs, elapsed) =
        mon16_6_lazy_wait(&m, w, None, 60_000, &keep_running, &on_prologue);
    initiator.join().unwrap();
    notifier.join().unwrap();
    assert_eq!((outcome, runs), (WaitOutcome::Notified, 1));
    assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
    assert_eq!(m.current_owner(), Some(w));
    assert!(m.exit_reporting_release(w).unwrap());
}

/// MON16-6: a timed wait's timeout counts from the call: a running phase
/// longer than the timeout ends at the timeout, the prologue runs once,
/// and the wait times out without waiting its timeout a second time. A
/// deferring notify's registered waiter takes the full path even when
/// notified while running, and leaves no registration behind.
#[test]
fn mon16_6_the_timeout_counts_from_the_call_and_a_registered_waiter_blocks() {
    let m = Monitor::new();
    let w = ThreadId(1665);
    m.enter(w);
    let (outcome, runs, elapsed) = mon16_6_lazy_wait(&m, w, Some(60), 60_000, &|| true, &|| {});
    assert_eq!((outcome, runs), (WaitOutcome::TimedOutOrSpurious, 1));
    assert!(
        elapsed < std::time::Duration::from_millis(2_000),
        "{elapsed:?}"
    );
    assert!(m.state.lock().wait_queue.is_empty());
    assert!(m.exit_reporting_release(w).unwrap());

    // Without the running re-acquisition (`wait_lazy_reacquire`); with
    // it, see `a_deferred_lazy_waiter_takes_the_monitor_at_the_release`.
    let m = Arc::new(Monitor::with_tuning(MonitorTuning {
        notify_defers_wake: true,
        wait_lazy_reacquire: false,
        ..MonitorTuning::DEFAULT
    }));
    let (w, n) = (ThreadId(1666), ThreadId(1667));
    m.enter(w);
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let notifier = mon16_6_notifier(&m, n, 0, ready);
    let (outcome, runs, _) = mon16_6_lazy_wait(&m, w, None, 10_000, &|| true, &|| {});
    notifier.join().unwrap();
    assert_eq!((outcome, runs), (WaitOutcome::Notified, 1));
    assert_eq!(m.current_owner(), Some(w));
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
    assert!(m.exit_reporting_release(w).unwrap());
    assert!(m.state.lock().deferred_wakes.is_empty());
}

/// JIT round 16 (lane monitor, MB2): with the pre-check on, the recorder
/// mutex is wanted exactly when JFR is enabled; with it off, always, and
/// the JFR state is then not even asked.
#[test]
fn mb2_the_wait_jfr_event_is_wanted_only_while_jfr_is_on() {
    assert!(!wait_jfr_event_wanted(true, || false));
    assert!(wait_jfr_event_wanted(true, || true));
    assert!(wait_jfr_event_wanted(false, || panic!(
        "not asked with the pre-check off"
    )));
}

/// Round 13 wave 10 (lane monitor2): a notified waiter whose notifier
/// keeps the monitor far longer than any spin still re-acquires it (the
/// spin gives up and the waiter parks as before) and is woken by the
/// notifier's release.
#[test]
fn a_reacquire_spin_that_runs_out_still_parks_and_is_woken() {
    let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
    let (w, n) = (ThreadId(511), ThreadId(512));
    let waiting = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waiter = {
        let m = Arc::clone(&m);
        let waiting = Arc::clone(&waiting);
        std::thread::spawn(move || {
            let never = std::sync::atomic::AtomicBool::new(false);
            m.enter(w);
            waiting.store(true, Ordering::Release);
            let outcome = m.wait(w, None, Some(&never), None, None).unwrap();
            assert_eq!(outcome, WaitOutcome::Notified);
            assert_eq!(m.current_owner(), Some(w));
            assert!(m.exit_reporting_release(w).unwrap());
        })
    };
    // The waiter releases the monitor inside `wait`; take it and notify.
    while !waiting.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    m.enter(n);
    m.notify(n).unwrap();
    // Hold it well past any spin budget: the waiter must park, not spin.
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(m.exit_reporting_release(n).unwrap());
    waiter.join().unwrap();
    assert_eq!(m.current_owner(), None);
    assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
}



/// Round 14 wave 1 (lane sync, M2-1): the inline spin's census words are
/// drained into the process census only under
/// `CRATONVM_DBG_MONITOR_CONTENTION`, and a drain touches nothing else of
/// the monitor. (The words' offsets are pinned against the JIT's
/// `INFLATED_MONITOR_CENSUS_SPIN_*_OFFSET` at compile time, above.)
#[test]
fn the_inline_spin_census_words_drain_only_under_the_census() {
    let m = Monitor::new();
    m.census_inline_spin_wins.store(3, Ordering::Relaxed);
    m.census_inline_spin_budget_outs.store(2, Ordering::Relaxed);
    m.census_inline_spin_waiter_exits
        .store(1, Ordering::Relaxed);
    let before =
        CONTENTION_CENSUS[ContentionEvent::InlineSpinWin as usize].load(Ordering::Relaxed);
    m.drain_inline_spin_census();
    let on = contention_census_on();
    let left = |w: &AtomicU64| w.load(Ordering::Relaxed);
    assert_eq!(left(&m.census_inline_spin_wins), if on { 0 } else { 3 });
    assert_eq!(
        left(&m.census_inline_spin_budget_outs),
        if on { 0 } else { 2 }
    );
    assert_eq!(
        left(&m.census_inline_spin_waiter_exits),
        if on { 0 } else { 1 }
    );
    if on {
        // Other tests may count concurrently: at least ours arrived.
        assert!(
            CONTENTION_CENSUS[ContentionEvent::InlineSpinWin as usize].load(Ordering::Relaxed)
                >= before + 3
        );
    }
    assert!(m.is_idle());
    assert_eq!(m.entry_count.load(Ordering::Relaxed), 0);
    assert_eq!(
        CONTENTION_EVENT_NAMES[ContentionEvent::InlineSpinWaiterExit as usize],
        "inline_spin_waiter_exits"
    );
}

/// Spin (yielding) until `done` holds, failing after 10 s.
fn await_condition(what: &str, done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::yield_now();
    }
}

/// Round 14 wave 3 (lane monitor, M2-3, `CRATONVM_MONITOR_NOTIFY_ONE_WAITER`):
/// each `notify()` returns exactly one waiter -- the longest -- and leaves
/// every other one in the wait set, in both arms; with the switch on, no
/// waiter parks on the shared condvar (so no `notify` can wake the others)
/// and `notify` signals only the entry it pops.
#[test]
fn m2_3_notify_returns_exactly_the_marked_waiter_in_both_arms() {
    const N: u64 = 16;
    for one_waiter in [true, false] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            notify_one_waiter: one_waiter,
            ..MonitorTuning::DEFAULT
        }));
        let returned = Arc::new(AtomicU64::new(0));
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiters = Vec::new();
        for i in 0..N {
            let (wm, wreturned, worder) =
                (Arc::clone(&m), Arc::clone(&returned), Arc::clone(&order));
            let tid = ThreadId(600 + i);
            // Enrol one at a time, so arrival order is the ticket order.
            let before = m.state.lock().wait_queue.len();
            waiters.push(std::thread::spawn(move || {
                let never = std::sync::atomic::AtomicBool::new(false);
                wm.enter(tid);
                let outcome = wm.wait(tid, None, Some(&never), None, None).unwrap();
                assert_eq!(outcome, WaitOutcome::Notified);
                assert_eq!(wm.current_owner(), Some(tid));
                worder.lock().push(i);
                wreturned.fetch_add(1, Ordering::SeqCst);
                wm.exit(tid).unwrap();
            }));
            await_condition("a waiter to enrol", || {
                m.state.lock().wait_queue.len() == before + 1
            });
        }
        let notifier = ThreadId(699);
        for k in 1..=N {
            if one_waiter && monitor_pending_notify() {
                // Measured on THIS monitor only (the process-wide
                // `CONDVAR_SIGNALLED_RETURNS` also counts every other
                // test's waits): every waiter carries a condvar of its
                // own, and nobody parks on the shared one -- a
                // `notify_all` there finds no thread to wake. (With the
                // switch off the waiters park there, so this probe would
                // be a spurious wake-up; it runs in the ON arm only.)
                assert!(m.state.lock().wait_queue.iter().all(|e| e.signal.is_some()));
                assert_eq!(
                    m.wait_condvar.notify_all(),
                    0,
                    "a waiter parked on the shared condvar"
                );
            }
            m.enter(notifier);
            m.notify(notifier).unwrap();
            m.exit(notifier).unwrap();
            await_condition("the notified waiter to return", || {
                returned.load(Ordering::SeqCst) == k
            });
            // Several poll slices: nobody else may leave.
            std::thread::sleep(std::time::Duration::from_millis(15));
            assert_eq!(
                returned.load(Ordering::SeqCst),
                k,
                "one_waiter={one_waiter}"
            );
            assert_eq!(
                m.state.lock().wait_queue.len() as u64,
                N - k,
                "one_waiter={one_waiter}"
            );
        }
        for w in waiters {
            w.join().unwrap();
        }
        assert_eq!(
            *order.lock(),
            (0..N).collect::<Vec<_>>(),
            "FIFO, one_waiter={one_waiter}"
        );
        let s = m.state.lock();
        assert_eq!(s.pending_notifies, 0);
        assert!(s.notified_tickets.is_empty() && s.wait_queue.is_empty());
        assert_eq!(s.parked_waiters, 0);
        drop(s);
    }
    // `notify` signals only the entry it pops: an entry with a condvar of
    // its own asks for no shared signal, one without does.
    let mut s = MonitorState {
        parked_waiters: 0,
        pending_notifies: 0,
        wait_queue: std::collections::VecDeque::new(),
        notified_tickets: Vec::new(),
        next_wait_ticket: 0,
        deferred_wakes: std::collections::VecDeque::new(),
        heir_line: std::collections::VecDeque::new(),
    };
    let own = Arc::new(Condvar::new());
    let t0 = s.enroll_waiter(Some(Arc::clone(&own)), ThreadId(0));
    let t1 = s.enroll_wait_ticket();
    assert_eq!(s.notify_longest_waiter(), Some(false));
    assert_eq!(
        Arc::strong_count(&own),
        1,
        "the popped entry's clone is gone"
    );
    assert_eq!(s.notify_longest_waiter(), Some(true));
    assert_eq!(s.notify_longest_waiter(), None);
    assert!(s.take_notification(t0) && s.take_notification(t1));
}

/// Round 14 wave 3 (lane monitor, M2-3): an interrupt wake reaches a
/// waiter parked on its own condvar, and `notifyAll` returns every
/// waiter parked on one.
#[test]
fn m2_3_interrupt_wake_and_notify_all_reach_waiters_on_their_own_condvars() {
    let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
    let flags: Vec<_> = (0..3)
        .map(|_| Arc::new(std::sync::atomic::AtomicBool::new(false)))
        .collect();
    let waiters: Vec<_> = (0..3u64)
        .map(|i| {
            let (m, flag) = (Arc::clone(&m), Arc::clone(&flags[i as usize]));
            std::thread::spawn(move || {
                let tid = ThreadId(700 + i);
                m.enter(tid);
                let outcome = m.wait(tid, None, Some(&flag), None, None).unwrap();
                m.exit(tid).unwrap();
                outcome
            })
        })
        .collect();
    await_condition("three waiters to enrol", || {
        m.state.lock().wait_queue.len() == 3
    });
    let mut waiters = waiters.into_iter();
    let first = waiters.next().unwrap();
    // Let the slices back off, then interrupt the first-spawned waiter,
    // whatever its place in the wait set.
    std::thread::sleep(std::time::Duration::from_millis(300));
    flags[0].store(true, Ordering::Release);
    m.wake_all_for_interrupt();
    assert_eq!(first.join().unwrap(), WaitOutcome::Interrupted);
    assert_eq!(m.state.lock().wait_queue.len(), 2);
    let notifier = ThreadId(799);
    m.enter(notifier);
    m.notify_all(notifier).unwrap();
    m.exit(notifier).unwrap();
    for w in waiters {
        assert_eq!(w.join().unwrap(), WaitOutcome::Notified);
    }
    assert!(m.state.lock().wait_queue.is_empty());
    assert_eq!(m.current_owner(), None);
}


/// Round 14 wave 3 (lane monitor, M2-4, `CRATONVM_MONITOR_WAIT_POLL_BACKOFF`):
/// the slice doubles from 5 ms to the 100 ms cap with the switch, and
/// stays at 5 ms without it; an interrupt flag that no wake announces is
/// still noticed within a capped slice by a thread that has waited long
/// enough to back off.
#[test]
fn m2_4_the_wait_poll_slice_backs_off_to_its_cap_and_still_sees_a_silent_interrupt() {
    let ms = std::time::Duration::from_millis;
    let mut slice = WAIT_POLL_SLICE;
    let mut seen = Vec::new();
    for _ in 0..7 {
        seen.push(slice);
        slice = next_wait_poll_slice(slice, true);
    }
    assert_eq!(
        seen,
        [ms(5), ms(10), ms(20), ms(40), ms(80), ms(100), ms(100)]
    );
    assert_eq!(
        next_wait_poll_slice(WAIT_POLL_SLICE, false),
        WAIT_POLL_SLICE
    );

    let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waiter = {
        let (m, flag) = (Arc::clone(&m), Arc::clone(&flag));
        std::thread::spawn(move || {
            let tid = ThreadId(711);
            m.enter(tid);
            let outcome = m.wait(tid, None, Some(&flag), None, None).unwrap();
            m.exit(tid).unwrap();
            outcome
        })
    };
    await_condition("the waiter to enrol", || {
        m.state.lock().wait_queue.len() == 1
    });
    std::thread::sleep(ms(400));
    // No `wake_all_for_interrupt`: only the poll can see this.
    let set_at = std::time::Instant::now();
    flag.store(true, Ordering::Release);
    assert_eq!(waiter.join().unwrap(), WaitOutcome::Interrupted);
    assert!(set_at.elapsed() < std::time::Duration::from_secs(5));
    assert!(m.state.lock().wait_queue.is_empty());
}

/// Round 14 wave 4 (lane monitor2, MON14-1): the interrupt wake aimed at
/// one thread signals that thread's wait-set entry and no other; a target
/// on the shared condvar asks the caller for the shared signal; a thread
/// not in the wait set is signalled nowhere.
#[test]
fn mon14_1_the_interrupt_wake_signals_only_its_target_entry() {
    let mut s = MonitorState {
        parked_waiters: 0,
        pending_notifies: 0,
        wait_queue: std::collections::VecDeque::new(),
        notified_tickets: Vec::new(),
        next_wait_ticket: 0,
        deferred_wakes: std::collections::VecDeque::new(),
        heir_line: std::collections::VecDeque::new(),
    };
    for i in 0..8u64 {
        s.enroll_waiter(Some(Arc::new(Condvar::new())), ThreadId(810 + i));
    }
    let shared_ticket = s.enroll_waiter(None, ThreadId(830));
    assert_eq!(s.signal_waiter_for_interrupt(ThreadId(813)), (1, false));
    assert_eq!(s.signal_waiter_for_interrupt(ThreadId(830)), (1, true));
    assert_eq!(s.signal_waiter_for_interrupt(ThreadId(899)), (0, false));
    assert_eq!(s.wait_queue.len(), 9, "an interrupt wake marks nothing");
    assert!(s.notified_tickets.is_empty() && s.pending_notifies == 0);
    s.leave_wait_set(shared_ticket);
    assert_eq!(s.signal_waiter_for_interrupt(ThreadId(830)), (0, false));
}

/// Round 14 wave 4 (lane monitor2, MON14-1): through real waiters -- the
/// targeted wake returns the interrupted waiter `Interrupted` and leaves
/// the three others in the wait set until a `notifyAll`.
#[test]
fn mon14_1_a_targeted_interrupt_wake_returns_only_the_target() {
    let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
    let flags: Vec<_> = (0..4)
        .map(|_| Arc::new(std::sync::atomic::AtomicBool::new(false)))
        .collect();
    let waiters: Vec<_> = (0..4u64)
        .map(|i| {
            let (m, flag) = (Arc::clone(&m), Arc::clone(&flags[i as usize]));
            std::thread::spawn(move || {
                let tid = ThreadId(840 + i);
                m.enter(tid);
                let outcome = m.wait(tid, None, Some(&flag), None, None).unwrap();
                m.exit(tid).unwrap();
                outcome
            })
        })
        .collect();
    await_condition("four waiters to enrol", || {
        m.state.lock().wait_queue.len() == 4
    });
    flags[2].store(true, Ordering::Release);
    // 1, or 0 when the target's own safety slice ran out between the
    // flag and the wake and it left the wait set by itself.
    assert!(m.wake_for_interrupt_of(ThreadId(842)) <= 1);
    let mut waiters: Vec<_> = waiters.into_iter().map(Some).collect();
    let target = waiters[2].take().unwrap();
    assert_eq!(target.join().unwrap(), WaitOutcome::Interrupted);
    {
        let s = m.state.lock();
        assert_eq!(s.wait_queue.len(), 3);
        assert!(s.wait_queue.iter().all(|e| e.waiter != ThreadId(842)));
    }
    let notifier = ThreadId(849);
    m.enter(notifier);
    m.notify_all(notifier).unwrap();
    m.exit(notifier).unwrap();
    for w in waiters.into_iter().flatten() {
        assert_eq!(w.join().unwrap(), WaitOutcome::Notified);
    }
    assert!(m.state.lock().wait_queue.is_empty());
    assert_eq!(m.current_owner(), None);
}



/// Round 14 wave 4 (lane monitor2, MON14-2): a flag already set when the
/// waiter enrols ends the wait under the state lock it enrolled under --
/// the waiter never parks, so no observer can see it in the wait set --
/// and the monitor is re-acquired at the saved depth, timed or not.
#[test]
fn mon14_2_an_interrupt_before_enrolment_ends_the_wait_without_a_park() {
    for timeout in [None, Some(60_000u64)] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
        let seen_enrolled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observer = {
            let (m, seen) = (Arc::clone(&m), Arc::clone(&seen_enrolled));
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    if !m.state.lock().wait_queue.is_empty() {
                        seen.store(true, Ordering::Release);
                    }
                    std::thread::yield_now();
                }
            })
        };
        let tid = ThreadId(860);
        let flag = std::sync::atomic::AtomicBool::new(true);
        m.enter(tid);
        m.enter(tid);
        let outcome = m.wait(tid, timeout, Some(&flag), None, None).unwrap();
        done.store(true, Ordering::Release);
        observer.join().unwrap();
        assert_eq!(outcome, WaitOutcome::Interrupted, "timeout={timeout:?}");
        let parked = seen_enrolled.load(Ordering::Acquire);
        assert!(!parked, "the waiter parked, timeout={timeout:?}");
        assert!(
            flag.load(Ordering::Acquire),
            "wait reads the flag, never clears it"
        );
        assert_eq!(m.current_owner(), Some(tid));
        assert_eq!(m.entry_count.load(Ordering::Relaxed), 2);
        {
            let s = m.state.lock();
            assert!(s.wait_queue.is_empty() && s.notified_tickets.is_empty());
            assert_eq!(s.parked_waiters, 0);
        }
        m.exit(tid).unwrap();
        m.exit(tid).unwrap();
        assert_eq!(m.current_owner(), None);
    }
}

/// Round 14 wave 4 (lane monitor2, MON14-2 / MON14-4): the slice plan.
/// A waiter parks once per safety slice only with the credit, the
/// enrolment re-check and the back-off all on; the back-off's kill
/// switch still restores the fixed 5 ms cadence; a requested stack dump
/// starts at 5 ms; the re-check alone starts the back-off at 20 ms.
#[test]
fn mon14_4_a_waiter_parks_once_only_with_every_wake_in_place() {
    let ms = std::time::Duration::from_millis;
    let on = MonitorTuning::DEFAULT;
    assert!(on.wait_parks_once(true));
    assert!(
        !on.wait_parks_once(false),
        "no credit: a bare signal is the notification"
    );
    assert_eq!(on.first_wait_slice(true, false), WAIT_SAFETY_SLICE);
    assert_eq!(on.first_wait_slice(true, true), WAIT_POLL_SLICE);
    assert_eq!(
        next_wait_slice(WAIT_POLL_SLICE, true, true),
        WAIT_SAFETY_SLICE
    );
    assert_eq!(
        next_wait_slice(WAIT_SAFETY_SLICE, true, true),
        WAIT_SAFETY_SLICE
    );
    assert_eq!(next_wait_slice(ms(20), true, false), ms(40));

    let no_backoff = MonitorTuning {
        wait_poll_backoff: false,
        ..MonitorTuning::DEFAULT
    };
    assert!(!no_backoff.wait_parks_once(true));
    assert_eq!(no_backoff.first_wait_slice(true, false), WAIT_POLL_SLICE);
    assert_eq!(
        next_wait_slice(WAIT_POLL_SLICE, false, false),
        WAIT_POLL_SLICE
    );

    let no_recheck = MonitorTuning {
        wait_enrol_interrupt_check: false,
        ..MonitorTuning::DEFAULT
    };
    assert!(!no_recheck.wait_parks_once(true));
    assert_eq!(no_recheck.first_wait_slice(true, false), WAIT_POLL_SLICE);

    let polling = MonitorTuning {
        wait_single_park: false,
        ..MonitorTuning::DEFAULT
    };
    assert!(!polling.wait_parks_once(true));
    assert_eq!(
        polling.first_wait_slice(true, false),
        WAIT_POLL_SLICE_RECHECKED
    );
}

/// Round 14 wave 4 (lane monitor2, MON14-4): a timed wait that parks
/// once still ends at its deadline, not a safety slice later, and a
/// notification still ends an untimed one at once.
#[test]
fn mon14_4_a_single_park_wait_keeps_its_deadline_and_its_notification() {
    let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
    let tid = ThreadId(870);
    let never = std::sync::atomic::AtomicBool::new(false);
    m.enter(tid);
    let started = std::time::Instant::now();
    let outcome = m.wait(tid, Some(150), Some(&never), None, None).unwrap();
    let waited = started.elapsed();
    assert_eq!(outcome, WaitOutcome::TimedOutOrSpurious);
    assert!(
        waited >= std::time::Duration::from_millis(150),
        "{waited:?}"
    );
    assert!(waited < std::time::Duration::from_secs(10), "{waited:?}");
    assert_eq!(m.current_owner(), Some(tid));
    m.exit(tid).unwrap();

    let waiter = {
        let m = Arc::clone(&m);
        std::thread::spawn(move || {
            let tid = ThreadId(871);
            let never = std::sync::atomic::AtomicBool::new(false);
            m.enter(tid);
            let outcome = m.wait(tid, None, Some(&never), None, None).unwrap();
            m.exit(tid).unwrap();
            outcome
        })
    };
    await_condition("the waiter to enrol", || {
        m.state.lock().wait_queue.len() == 1
    });
    let notifier = ThreadId(872);
    m.enter(notifier);
    m.notify(notifier).unwrap();
    m.exit(notifier).unwrap();
    assert_eq!(waiter.join().unwrap(), WaitOutcome::Notified);
}

/// Round 14 wave 6 (lane monitor3, M2-2): the hand-over watch reports a
/// held owner word naming another thread, and nothing else -- not a free
/// read, not a forced release's marker, not the same owner re-taking it.
#[test]
fn monitor3_the_handover_watch_sees_only_an_owner_changing_threads() {
    let a = owner_word(Some(ThreadId(1)));
    let b = owner_word(Some(ThreadId(2)));
    let mut w = HandoverWatch::new();
    assert!(
        !w.owner_polled(a),
        "the first held poll has nothing to compare with"
    );
    assert!(!w.owner_polled(a));
    assert!(!w.owner_polled(0));
    assert!(!w.owner_polled(OWNER_RELEASING));
    assert!(
        !w.owner_polled(a),
        "released and re-taken by the same owner"
    );
    assert!(!w.seen);
    assert!(w.owner_polled(b));
    assert!(w.seen);
    assert!(!w.owner_polled(b));
    assert!(w.owner_polled(a));
    let mut lost = HandoverWatch::new();
    assert!(!lost.seen);
    lost.lost_race();
    assert!(lost.seen);
}

/// Round 14 wave 6 (lane monitor3, M2-2): a spin watching the owner word
/// flip between two OTHER threads (never free, so no spin can win) --
/// with the abort off runs its budget out and halves it, exactly the
/// round-12 rule (it never saw the monitor free); with the abort on leaves
/// at a flip and keeps its budget. The abort arm is sampled many times
/// because a spin the flipper's thread happens not to overlap runs out
/// like the control; every sample must fail and take nothing.
#[test]
fn monitor3_an_aborting_spin_leaves_at_an_owner_change_and_keeps_its_budget() {
    use std::sync::atomic::AtomicBool;
    let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
    let a = owner_word(Some(ThreadId(880)));
    let b = owner_word(Some(ThreadId(881)));
    let spinner = ThreadId(882);
    m.owner.store(a, Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    let flips = Arc::new(AtomicU64::new(0));
    let flipper = {
        let (m, stop, flips) = (Arc::clone(&m), Arc::clone(&stop), Arc::clone(&flips));
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                m.owner
                    .store(if n % 2 == 0 { b } else { a }, Ordering::SeqCst);
                n += 1;
                flips.store(n, Ordering::Relaxed);
                for _ in 0..4 {
                    std::hint::spin_loop();
                }
            }
        })
    };
    await_condition("the flipper to run", || {
        flips.load(Ordering::Relaxed) > 1000
    });

    m.spin_limit.store(SPIN_INITIAL, Ordering::Relaxed);
    assert!(!m.adaptive_spin(spinner, false));
    assert_eq!(
        m.spin_limit.load(Ordering::Relaxed),
        SPIN_INITIAL / 2,
        "abort off: owner changes alone never keep the budget"
    );

    // Interpreter round i1 wave 52 (lane L7): a spin only keeps its
    // budget if the flipper's thread runs DURING it, and on a loaded host
    // (the Linux host's GC batteries) 200 back-to-back samples once all
    // missed it. Sample until one keeps it, for at least 200 samples and
    // at most 60 s, yielding between samples so a flipper sharing the
    // core gets scheduled; every sample still must fail and take nothing,
    // and the control above still must halve.
    let mut kept = 0u32;
    let mut samples = 0u32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while samples < 200 || (kept == 0 && std::time::Instant::now() < deadline) {
        m.spin_limit.store(SPIN_INITIAL, Ordering::Relaxed);
        assert!(
            !m.adaptive_spin(spinner, true),
            "a never-free monitor is not won"
        );
        let left = m.spin_limit.load(Ordering::Relaxed);
        assert!(
            left == SPIN_INITIAL || left == SPIN_INITIAL / 2,
            "an aborting spin either keeps its budget or halves it, read {left}"
        );
        if left == SPIN_INITIAL {
            kept += 1;
        }
        samples += 1;
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    // gcg g3/k: whether a spin a few microseconds long overlaps a flip
    // is the scheduler's choice (`kept > 0` failed once under a loaded
    // unit run even before i1 w52's longer sampling), so how many spins
    // kept their budget is printed, not asserted; the abort RULE is
    // pinned deterministically below and in
    // `gcg_k3_a_scripted_owner_change_aborts_the_spin_and_keeps_its_budget`.
    eprintln!(
        "[monitor3-abort] threaded spins that kept their budget: {kept}/{samples} ({} flips)",
        flips.load(Ordering::Relaxed)
    );
    assert_ne!(m.current_owner(), Some(spinner));

    // The same rule with the owner change scripted into the polls: every
    // spin sees `a` then `b`, so every one leaves at the change and keeps.
    m.owner.store(a, Ordering::SeqCst);
    for _ in 0..200 {
        m.spin_limit.store(SPIN_INITIAL, Ordering::Relaxed);
        let mut n = 0u64;
        let won = m.adaptive_spin_polling(spinner, true, || {
            n += 1;
            if n % 2 == 1 {
                a
            } else {
                b
            }
        });
        assert!(!won, "a never-free monitor is not won");
        assert_eq!(n, 2, "the spin leaves at the first owner change");
        assert_eq!(
            m.spin_limit.load(Ordering::Relaxed),
            SPIN_INITIAL,
            "no aborting spin ever left at an owner change"
        );
    }
    assert_ne!(m.current_owner(), Some(spinner));
    m.owner.store(0, Ordering::SeqCst);
}

/// gcg g3/k: the deterministic form of the abort rule, with its negative
/// control. A spin whose polls never change owner runs its budget out and
/// halves it (abort on or off); one whose polls change owner keeps it only
/// with the abort on; with the abort off it runs the whole budget out
/// (every poll made) and halves, as round 12's rule says.
#[test]
fn gcg_k3_a_scripted_owner_change_aborts_the_spin_and_keeps_its_budget() {
    let m = Monitor::with_tuning(MonitorTuning::DEFAULT);
    let a = owner_word(Some(ThreadId(890)));
    let b = owner_word(Some(ThreadId(891)));
    let spinner = ThreadId(892);
    // The real word stays held by `a`, so `try_enter` can never succeed.
    m.owner.store(a, Ordering::SeqCst);
    for (abort, flip, want_kept) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, true),
    ] {
        m.spin_limit.store(SPIN_INITIAL, Ordering::Relaxed);
        let mut polls = 0u64;
        let won = m.adaptive_spin_polling(spinner, abort, || {
            polls += 1;
            if flip && polls % 2 == 0 {
                b
            } else {
                a
            }
        });
        assert!(!won, "abort={abort} flip={flip}: never free, never won");
        let kept = m.spin_limit.load(Ordering::Relaxed) == SPIN_INITIAL;
        assert_eq!(kept, want_kept, "abort={abort} flip={flip}: polls={polls}");
        if want_kept {
            assert_eq!(polls, 2, "the abort leaves at the first change");
        } else {
            assert!(
                polls > 2,
                "abort={abort} flip={flip}: ran its budget, polls={polls}"
            );
        }
    }
    assert_ne!(m.current_owner(), Some(spinner));
    m.owner.store(0, Ordering::SeqCst);
}

/// JIT round 16 wave 8 (lane monitor, `MonitorTuning::heir`): a
/// reservation refuses every acquisition of the free monitor but the
/// heir's, the heir's acquisition ends it, and while it stands a release
/// owes the wake even with a successor on its way and a spinner
/// registered. With the switch off the word is ignored entirely.
#[test]
fn r16w8_a_reservation_refuses_everyone_but_the_heir() {
    let (heir, other) = (ThreadId(1810), ThreadId(1811));
    let m = Monitor::with_tuning(MonitorTuning {
        heir: true,
        ..MonitorTuning::DEFAULT
    });
    m.heir.store(owner_word(Some(heir)), Ordering::SeqCst);
    assert!(!m.try_enter(other), "reserved for the heir");
    assert_eq!(m.current_owner(), None);
    // The wake decision: a parked entrant, a successor on its way and a
    // registered spinner, which would excuse the wake without a heir.
    m.entry_waiters.store(1, Ordering::SeqCst);
    m.succ_pending.store(true, Ordering::SeqCst);
    m.spinners.store(1, Ordering::SeqCst);
    assert!(m.successor_wake_needed(), "the heir is owed the wake");
    assert!(m.try_enter(heir));
    assert_eq!(m.heir.load(Ordering::SeqCst), 0, "taken: reservation over");
    assert!(!m.successor_wake_needed());
    assert!(m.exit_reporting_release(heir).unwrap());
    assert!(m.try_enter(other), "no reservation: anyone");
    assert!(m.exit_reporting_release(other).unwrap());
    m.entry_waiters.store(0, Ordering::SeqCst);
    m.succ_pending.store(false, Ordering::SeqCst);
    m.spinners.store(0, Ordering::SeqCst);

    let off = Monitor::with_tuning(MonitorTuning::DEFAULT);
    off.heir.store(owner_word(Some(heir)), Ordering::SeqCst);
    assert!(off.try_enter(other), "switch off: the word means nothing");
    off.entry_waiters.store(1, Ordering::SeqCst);
    off.spinners.store(1, Ordering::SeqCst);
    assert!(!off.successor_wake_needed());
    off.entry_waiters.store(0, Ordering::SeqCst);
    off.spinners.store(0, Ordering::SeqCst);
    assert!(off.exit_reporting_release(other).unwrap());
}

/// The starvation `MonitorTuning::heir` is for, made deterministic: an
/// entrant parks while a barging owner holds the monitor, a registered
/// spinner (pinned in `spinners`) excuses every release from waking it,
/// and the owner re-takes the monitor right after each release. Without
/// the guard nothing ever wakes the entrant, so it waits for the whole
/// barging run (positive control: at least `BARGE`); with it, the entrant
/// reserves after `HEIR_AFTER` and gets the monitor while the owner is
/// still barging, and nothing is left registered or reserved.
#[test]
fn r16w8_a_starved_entrant_reserves_and_is_served_while_the_owner_barges() {
    const BARGE: std::time::Duration = std::time::Duration::from_secs(1);
    for heir_on in [false, true] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            heir: heir_on,
            ..MonitorTuning::DEFAULT
        }));
        let (owner, parker) = (ThreadId(1820), ThreadId(1821));
        assert!(m.try_enter(owner));
        let served = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p = {
            let (m, served) = (Arc::clone(&m), Arc::clone(&served));
            std::thread::spawn(move || {
                let t0 = std::time::Instant::now();
                m.block_enter(parker);
                let waited = t0.elapsed();
                served.store(true, Ordering::SeqCst);
                assert!(m.exit_reporting_release(parker).unwrap());
                waited
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while m.entry_waiters.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "parker never parked");
            std::thread::yield_now();
        }
        // The parker holds the state lock from its registration until it
        // is inside its wait: once we get it, it is parked.
        drop(m.state.lock());
        // A spinner that never comes: every release reads it and wakes nobody.
        m.spinners.store(1, Ordering::SeqCst);
        let barge_until = std::time::Instant::now() + BARGE;
        let mut held = true;
        while !served.load(Ordering::SeqCst) {
            let now = std::time::Instant::now();
            assert!(now < deadline, "the parker was never served");
            if held {
                for _ in 0..64 {
                    std::hint::spin_loop();
                }
                assert!(m.exit_reporting_release(owner).unwrap());
                held = false;
                if now >= barge_until {
                    // The run is over: drop the pinned spinner and wake
                    // the parker the way a leaving spinner would.
                    m.spinners.store(0, Ordering::SeqCst);
                    m.wake_successor();
                    break;
                }
            }
            held = m.try_enter(owner);
        }
        let waited = p.join().unwrap();
        if held {
            assert!(m.exit_reporting_release(owner).unwrap());
        }
        m.spinners.store(0, Ordering::SeqCst);
        if heir_on {
            assert!(waited < BARGE, "served while the owner barged ({waited:?})");
        } else {
            assert!(waited >= BARGE, "positive control: starved ({waited:?})");
        }
        assert_eq!(m.heir.load(Ordering::SeqCst), 0);
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(m.current_owner(), None);
    }
}

/// Round 14 wave 6 (lane monitor3): the entrant-wait census buckets a
/// wait by the bounds its names state, and the census names line up with
/// the events (the exit line prints them zipped).
#[test]
fn monitor3_entrant_waits_fall_in_their_named_buckets() {
    use super::ContentionEvent as E;
    for (us, want) in [
        (0u64, E::EntrantWaitUnder100us),
        (99, E::EntrantWaitUnder100us),
        (100, E::EntrantWaitUnder1ms),
        (999, E::EntrantWaitUnder1ms),
        (1_000, E::EntrantWaitUnder16ms),
        (15_999, E::EntrantWaitUnder16ms),
        (16_000, E::EntrantWaitLonger),
        (u64::MAX, E::EntrantWaitLonger),
    ] {
        assert_eq!(entrant_wait_bucket(us), want, "{us} us");
    }
    for (event, name) in [
        (E::SpinWinAfterHandover, "spin_wins_after_handover"),
        (E::SpinFailAfterHandover, "spin_fails_after_handover"),
        (E::SpinHandoverAbort, "spin_handover_aborts"),
        (E::EntrantWaitMicros, "entrant_wait_us"),
        (E::EntrantWaitUnder100us, "entrant_waits_under_100us"),
        (E::EntrantWaitUnder1ms, "entrant_waits_under_1ms"),
        (E::EntrantWaitUnder16ms, "entrant_waits_under_16ms"),
        (E::EntrantWaitLonger, "entrant_waits_longer"),
        (E::WaitReacquirePark, "wait_reacquire_parks"),
        (E::NotifyDeferred, "notify_deferred"),
        (E::DeferredNotifyWake, "deferred_notify_wakes"),
        (E::EnterSpinYieldWin, "enter_spin_yield_wins"),
        (E::WaitLazyWin, "wait_lazy_wins"),
        (E::WaitLazyFallback, "wait_lazy_fallbacks"),
        (E::HeirReserved, "heir_reservations"),
        (E::HeirWin, "heir_wins"),
        (E::HeirRefusal, "heir_refusals"),
        (E::EntrantWaitMaxMicros, "entrant_wait_max_us"),
        (E::HeirReservedMicros, "heir_reserved_us"),
        (E::HeirReservedMaxMicros, "heir_reserved_max_us"),
        (E::HeirSignalWake, "heir_signal_wakes"),
        (E::LazyWakeWalk, "lazy_wake_walks"),
        (E::LazyWakeList, "lazy_wake_lists"),
        (E::WaitLazyReacquireWin, "wait_lazy_reacquire_wins"),
        (E::WaitLazyReacquireGiveUp, "wait_lazy_reacquire_give_ups"),
    ] {
        assert_eq!(CONTENTION_EVENT_NAMES[event as usize], name);
    }
}


/// gcg g3/k: with the switch off `lazily_parked_on` is `lazily_parked`
/// (counted, never listed); with it on, two parks of one monitor list it
/// twice and each removes one entry.
#[test]
fn gcg_k3_lazily_parked_on_lists_only_with_the_switch() {
    let m = Arc::new(Monitor::new());
    let off = MonitorTable::new().with_lazy_wake_list(false);
    let seen = off.lazily_parked_on(&m, || {
        (
            off.lazy_parked_monitor_count(),
            off.lazy_parkers.load(Ordering::SeqCst),
        )
    });
    assert_eq!(seen, (0, 1));
    let on = MonitorTable::new().with_lazy_wake_list(true);
    let (outer, inner) = on.lazily_parked_on(&m, || {
        let outer = on.lazy_parked_monitor_count();
        let inner = on.lazily_parked_on(&m, || on.lazy_parked_monitor_count());
        (outer, inner)
    });
    assert_eq!((outer, inner), (1, 2));
    assert_eq!(on.lazy_parked_monitor_count(), 0);
    assert_eq!(on.lazy_parkers.load(Ordering::SeqCst), 0);
    // Nothing listed and nobody parked: the wake is the one-load fast path.
    on.wake_lazy_parkers();
    // The census names of the two wake shapes (the probe rows read them).
    assert_eq!(
        CONTENTION_EVENT_NAMES[ContentionEvent::LazyWakeWalk as usize],
        "lazy_wake_walks"
    );
    assert_eq!(
        CONTENTION_EVENT_NAMES[ContentionEvent::LazyWakeList as usize],
        "lazy_wake_lists"
    );
}

/// Park `parker` on `m` (held by someone else) on a thread of its own,
/// and return once it is parked: registered, and past the state lock it
/// holds from its registration until it waits. The handle yields how
/// long it waited; it releases the monitor once it has it.
fn r16w10_park(
    m: &Arc<Monitor>,
    parker: ThreadId,
) -> std::thread::JoinHandle<std::time::Duration> {
    let before = m.entry_waiters.load(Ordering::SeqCst);
    let h = {
        let m = Arc::clone(m);
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            m.block_enter(parker);
            let waited = t0.elapsed();
            assert!(m.exit_reporting_release(parker).unwrap());
            waited
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while m.entry_waiters.load(Ordering::SeqCst) == before {
        assert!(std::time::Instant::now() < deadline, "never parked");
        std::thread::yield_now();
    }
    drop(m.state.lock());
    h
}

/// JIT round 16 wave 10 (lane monitor2, `MonitorTuning::heir_ordered`):
/// an entrant that has waited past `HEIR_AFTER` may not reserve while an
/// older one is ahead of it in the line, and reserves once the line has
/// it in front. Positive control: without the switch the same entrant
/// reserves with the older word still in the (unkept) line. The older
/// entrant is a word put in the line by hand -- the line only gates the
/// reservation, so nothing else reads it. The entrant is then served and
/// leaves the line empty.
#[test]
fn r16w10_ordered_an_entrant_behind_an_older_one_does_not_reserve() {
    let (owner, parker, older) = (ThreadId(1830), ThreadId(1831), ThreadId(1832));
    for ordered in [false, true] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            heir: true,
            heir_ordered: ordered,
            ..MonitorTuning::DEFAULT
        }));
        assert!(m.try_enter(owner));
        m.state.lock().heir_line.push_back(owner_word(Some(older)));
        let p = r16w10_park(&m, parker);
        if ordered {
            let line: Vec<u64> = m.state.lock().heir_line.iter().copied().collect();
            assert_eq!(
                line,
                vec![owner_word(Some(older)), owner_word(Some(parker))]
            );
        }
        // Several heir slices past `HEIR_AFTER` (a Windows timed wait
        // rounds up to the 15.6 ms clock tick).
        std::thread::sleep(std::time::Duration::from_millis(120));
        let reserved = m.heir.load(Ordering::SeqCst);
        if ordered {
            assert_eq!(reserved, 0, "behind an older entrant: no reservation");
            m.state
                .lock()
                .heir_line
                .retain(|&w| w != owner_word(Some(older)));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while m.heir.load(Ordering::SeqCst) == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "front, never reserved"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        } else {
            assert_eq!(reserved, owner_word(Some(parker)), "positive control");
            m.state.lock().heir_line.clear();
        }
        assert_eq!(m.heir.load(Ordering::SeqCst), owner_word(Some(parker)));
        assert!(m.exit_reporting_release(owner).unwrap());
        p.join().unwrap();
        assert_eq!(m.heir.load(Ordering::SeqCst), 0);
        assert!(m.state.lock().heir_line.is_empty());
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(m.current_owner(), None);
    }
}

/// JIT round 16 wave 10 (lane monitor2, `MonitorTuning::heir_signal`):
/// once it has reserved the monitor, the heir parks on `heir_condvar`
/// (a signal there finds it), and a release owed to it reaches it there
/// even with a spinner pinned in `spinners` and a second entrant parked
/// on `entry_condvar`; the second entrant is served after it. Positive
/// control: without the switch nobody ever parks on `heir_condvar`.
#[test]
fn r16w10_signal_the_heir_parks_on_its_own_condvar_and_is_served() {
    let (owner, heir, later) = (ThreadId(1840), ThreadId(1841), ThreadId(1842));
    for signal in [false, true] {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning {
            heir: true,
            heir_signal: signal,
            ..MonitorTuning::DEFAULT
        }));
        assert!(m.try_enter(owner));
        let h = r16w10_park(&m, heir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while m.heir.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "never reserved");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(m.heir.load(Ordering::SeqCst), owner_word(Some(heir)));
        let l = r16w10_park(&m, later);
        // Releases now wake nobody but an heir.
        m.spinners.store(1, Ordering::SeqCst);
        // A signal on `heir_condvar` finds the heir there (it re-polls,
        // finds the monitor held and parks there again) -- only with the
        // switch. Bounded: it is spinning or re-trying between slices.
        let mut found = false;
        for _ in 0..100 {
            let state = m.state.lock();
            found = m.heir_condvar.notify_one();
            drop(state);
            if found {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(found, signal, "the heir's condvar");
        assert!(m.exit_reporting_release(owner).unwrap());
        let waited = h.join().unwrap();
        assert!(waited < std::time::Duration::from_secs(10));
        // The later entrant: no heir any more, the pinned spinner gone.
        m.spinners.store(0, Ordering::SeqCst);
        m.wake_successor();
        l.join().unwrap();
        assert_eq!(m.heir.load(Ordering::SeqCst), 0);
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(m.current_owner(), None);
    }
}
