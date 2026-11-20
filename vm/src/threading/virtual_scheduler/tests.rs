use super::*;

#[test]
fn scheduler_creation_default() {
    let sched = VirtualThreadScheduler::new_default();
    assert!(sched.carrier_count() > 0);
}

#[test]
fn scheduler_acquire_release() {
    let sched = VirtualThreadScheduler::new(2);
    // Acquire two permits
    sched.acquire();
    sched.acquire();
    // Release one
    sched.release();
    // Acquire again should succeed
    sched.acquire();
    sched.release();
    sched.release();
}

#[test]
fn release_cannot_exceed_carrier_count() {
    // Bug B4: an over-release (release without a paired acquire) must not
    // raise `available` past `carrier_count`.
    let sched = VirtualThreadScheduler::new(2);
    assert_eq!(sched.available(), 2);

    // Stray releases while already full are no-ops.
    sched.release();
    sched.release();
    sched.release();
    assert_eq!(
        sched.available(),
        2,
        "available must clamp to carrier_count"
    );

    // The bound is still enforced: exactly two acquires drain the pool.
    sched.acquire();
    sched.acquire();
    assert_eq!(sched.available(), 0);

    // Two releases matching the two acquires above are both legitimate
    // and fully restore the pool to capacity. (An earlier version of this
    // test asserted `1` here, expecting the second release to be treated
    // as a "double release" — but with 2 real acquires outstanding, both
    // releases genuinely pair with one, so both must count.
    //
    // The original rationale continued "…real callers pair
    // acquire/release per blocking event (see `vt_acquire_carrier` /
    // `vt_release_carrier` in `vm_exec.rs`)". Those callers are gone as of
    // 2026-07-26 and there are no real callers at all now; what is left
    // below is a semantics test of the counter itself.)
    sched.release();
    sched.release();
    assert_eq!(
        sched.available(),
        2,
        "two releases matching two prior acquires must fully restore the pool"
    );

    // A further release with no corresponding acquire is a genuine
    // over-release and is still clamped rather than accumulated.
    sched.release();
    assert_eq!(
        sched.available(),
        2,
        "an over-release beyond carrier_count is clamped, not accumulated"
    );
}

#[test]
fn scheduler_blocks_when_exhausted() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    let sched = VirtualThreadScheduler::new(1);
    let sched2 = sched.clone();

    // Acquire the only permit
    sched.acquire();

    let acquired = Arc::new(AtomicBool::new(false));
    let acquired2 = acquired.clone();

    // Spawn a thread that tries to acquire — should block
    let handle = thread::spawn(move || {
        sched2.acquire();
        acquired2.store(true, Ordering::SeqCst);
        sched2.release();
    });

    // Give the other thread time to start blocking
    thread::sleep(Duration::from_millis(50));
    assert!(!acquired.load(Ordering::SeqCst), "should still be blocked");

    // Release our permit — the other thread should wake up
    sched.release();
    handle.join().unwrap();
    assert!(acquired.load(Ordering::SeqCst), "should have acquired");
}
