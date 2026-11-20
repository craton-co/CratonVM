use super::*;
use std::sync::atomic::Ordering;

#[test]
fn a_band_must_be_aligned_non_empty_and_bounded() {
    assert!(lazy_native_band_ok(0x1000, 0x2000));
    assert!(!lazy_native_band_ok(0, 0x2000), "no band");
    assert!(!lazy_native_band_ok(0x1004, 0x2000), "misaligned low end");
    assert!(!lazy_native_band_ok(0x2000, 0x2000), "empty");
    assert!(!lazy_native_band_ok(0x3000, 0x2000), "inverted");
    assert!(!lazy_native_band_ok(
        0x1000,
        0x1000 + LAZY_NATIVE_MAX_BAND + 8
    ));
}

#[test]
fn enter_publishes_the_band_and_leave_clears_the_state() {
    let b = GcBlockState::new();
    assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
    b.enter_lazy_native(0x1000, 0x9000);
    assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
    assert!(!b.leave_lazy_native(), "an unclaimed leave never waits");
    assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
    // A leave without an entry is a no-op.
    assert!(!b.leave_lazy_native());
    assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
}

#[test]
fn only_a_thread_in_native_is_claimed_and_only_once() {
    let b = GcBlockState::new();
    assert_eq!(b.try_claim_lazy_native(), None, "a running thread");
    b.enter_lazy_native(0x1000, 0x9000);
    assert_eq!(b.try_claim_lazy_native(), Some((0x1000, 0x9000)));
    assert_eq!(b.try_claim_lazy_native(), None, "already claimed");
    assert!(b.release_lazy_native_claim());
    assert!(!b.release_lazy_native_claim(), "released once");
    assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
    assert!(!b.leave_lazy_native());
}

#[test]
fn a_claim_with_an_unusable_band_is_given_back() {
    let b = GcBlockState::new();
    b.enter_lazy_native(0x1004, 0x9000);
    assert_eq!(b.try_claim_lazy_native(), None);
    assert_eq!(
        b.lazy_native.load(Ordering::Acquire),
        LAZY_NATIVE_IN,
        "the thread stays in native, unclaimed: the pause waits for it"
    );
}

/// The leave of a claimed thread waits until the pause releases it, and
/// only then returns -- the property that keeps its frames frozen while
/// the pause reads them.
#[test]
fn a_claimed_thread_leaves_only_after_the_release() {
    let b = Arc::new(GcBlockState::new());
    b.enter_lazy_native(0x1000, 0x9000);
    assert!(b.try_claim_lazy_native().is_some());
    let left = Arc::new(AtomicBool::new(false));
    let t = {
        let b = Arc::clone(&b);
        let left = Arc::clone(&left);
        std::thread::spawn(move || {
            let waited = b.leave_lazy_native();
            left.store(true, Ordering::Release);
            waited
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        !left.load(Ordering::Acquire),
        "the leave must not pass a claim"
    );
    assert!(b.release_lazy_native_claim());
    let waited = t.join().unwrap_or(false);
    assert!(waited, "the leave reports that it waited");
    assert!(left.load(Ordering::Acquire));
    assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
}

/// JIT round 18 wave 10 (lane ffmeintr, `CRATONVM_FFM_LAZY_LEAVE_HANDOFF`,
/// default ON): a leave that found its thread claimed marks it
/// `CLAIMED_LEAVING`; no later claim can take it, and the release
/// completes the leave (`-> OFF`) instead of putting the thread back in C,
/// where the next pause could claim it again before it ran.
#[test]
fn a_waiting_leave_is_handed_over_by_the_release() {
    if !lazy_leave_handoff_enabled() {
        return; // `=0` in the environment: the round 16 leave, tested above.
    }
    let b = Arc::new(GcBlockState::new());
    b.enter_lazy_native(0x1000, 0x9000);
    assert!(b.try_claim_lazy_native().is_some());
    let t = {
        let b = Arc::clone(&b);
        std::thread::spawn(move || b.leave_lazy_native())
    };
    let t0 = std::time::Instant::now();
    while b.lazy_native.load(Ordering::Acquire) != LAZY_NATIVE_CLAIMED_LEAVING {
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(20),
            "the waiting leave never marked itself"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        b.try_claim_lazy_native(),
        None,
        "a leaving thread is not in C"
    );
    assert!(
        b.release_lazy_native_claim(),
        "the release takes the leaving state"
    );
    assert_eq!(
        b.lazy_native.load(Ordering::Acquire),
        LAZY_NATIVE_OFF,
        "handed the leave: the next pause has nothing to claim"
    );
    assert_eq!(b.try_claim_lazy_native(), None);
    assert!(
        t.join().unwrap_or(false),
        "the leave reports that it waited"
    );
    assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
    assert!(!b.release_lazy_native_claim(), "released once");
}

/// JIT round 18 wave 10b (lane ffmeintr, `CRATONVM_FFM_LAZY_SIGNAL_GATE`):
/// a gated thread cannot go into C lazily until the pass ungates it; a
/// thread already in C is reported, not gated; a claim never takes a
/// gated thread.
#[test]
fn a_gated_thread_waits_to_go_into_c_until_the_signal_is_answered() {
    let b = Arc::new(GcBlockState::new());
    assert_eq!(b.try_gate_takeover_signal(), TakeoverSignalGate::Gated);
    assert_eq!(
        b.try_gate_takeover_signal(),
        TakeoverSignalGate::Other,
        "gated once"
    );
    assert_eq!(
        b.try_claim_lazy_native(),
        None,
        "a gated thread is not in C"
    );
    let entered = Arc::new(AtomicBool::new(false));
    let t = {
        let b = Arc::clone(&b);
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            b.enter_lazy_native(0x1000, 0x9000);
            entered.store(true, Ordering::Release);
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        !entered.load(Ordering::Acquire),
        "the entry must not pass the gate"
    );
    b.ungate_takeover_signal();
    let _ = t.join();
    assert!(entered.load(Ordering::Acquire));
    assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
    assert_eq!(
        b.try_gate_takeover_signal(),
        TakeoverSignalGate::InNative,
        "a thread in C is not signalled"
    );
    assert_eq!(
        b.lazy_native.load(Ordering::Acquire),
        LAZY_NATIVE_IN,
        "untouched"
    );
    assert!(!b.leave_lazy_native());
    assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
}

/// JIT round 18 wave 10 (lane ffmeintr, EINTR2-1): a collector's hold on
/// a blocked-wait band keeps the owner from leaving its trampoline until
/// the release, one holder at a time; no band, no hold.
#[test]
fn a_held_blocked_wait_band_keeps_its_thread_inside() {
    let b = Arc::new(GcBlockState::new());
    assert_eq!(b.try_hold_blocked_wait_band(), None, "not in a trampoline");
    b.enter_blocked_wait_band(0x7000);
    assert_eq!(b.try_hold_blocked_wait_band(), Some(0x7000));
    assert_eq!(b.try_hold_blocked_wait_band(), None, "one holder");
    let left = Arc::new(AtomicBool::new(false));
    let t = {
        let b = Arc::clone(&b);
        let left = Arc::clone(&left);
        std::thread::spawn(move || {
            b.leave_blocked_wait_band(0x7000);
            left.store(true, Ordering::Release);
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        !left.load(Ordering::Acquire),
        "the leave must not pass a hold"
    );
    b.release_blocked_wait_band(0x7000);
    let _ = t.join();
    assert!(left.load(Ordering::Acquire));
    assert_eq!(b.blocked_wait_lo.load(Ordering::Acquire), 0);
    assert_eq!(b.try_hold_blocked_wait_band(), None);
}

/// The trampoline runs its wait and returns its value; with the switch
/// off (the default) it publishes nothing.
#[test]
fn the_blocked_wait_trampoline_returns_the_wait_and_withdraws_its_band() {
    let b = GcBlockState::new();
    let seen = run_blocked_wait(&b, || b.blocked_wait_lo.load(Ordering::Acquire));
    if helper_unparked_read_enabled() {
        assert_ne!(seen, 0, "published while waiting");
        assert_eq!(seen & 7, 0, "8-aligned");
    } else {
        assert_eq!(seen, 0, "the switch off publishes nothing");
    }
    assert_eq!(b.blocked_wait_lo.load(Ordering::Acquire), 0, "withdrawn");
    assert_eq!(run_blocked_wait(&b, || 41 + 1), 42);
}
