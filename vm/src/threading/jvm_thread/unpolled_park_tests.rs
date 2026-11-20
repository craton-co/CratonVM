use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const BOUND: Duration = Duration::from_secs(10);

/// Park `ps` unpolled on a new thread; the receiver gets how long it
/// waited.
fn spawn_parker(
    ps: &Arc<ParkState>,
    flag: &Arc<AtomicBool>,
    timeout: Option<Duration>,
) -> mpsc::Receiver<Duration> {
    let (tx, rx) = mpsc::channel();
    let (ps2, flag2) = (ps.clone(), flag.clone());
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        ps2.park_interruptible_unpolled(timeout, &flag2);
        let _ = tx.send(t0.elapsed());
    });
    rx
}

/// A lock-free state counts its parker as a sleeper before the permit
/// check: wait for that, then let it reach the wait itself.
fn wait_until_sleeping(ps: &ParkState) {
    while ps.sleeper_count() == 0 {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(5));
}

/// An untimed and a long timed park are each ended by an `unpark`, on a
/// plain state and on a lock-free one, and consume the permit.
#[test]
fn an_unpolled_park_is_ended_by_an_unpark() {
    for lock_free in [false, true] {
        for timed in [false, true] {
            let ps = Arc::new(ParkState::new());
            if lock_free {
                ps.enable_lock_free();
            }
            let flag = Arc::new(AtomicBool::new(false));
            let timeout = timed.then_some(Duration::from_secs(30));
            let rx = spawn_parker(&ps, &flag, timeout);
            if lock_free {
                wait_until_sleeping(&ps);
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
            ps.unpark_lock_free();
            let waited = rx.recv_timeout(BOUND).expect("the unpark ended the park");
            assert!(waited < BOUND, "lock_free={lock_free} timed={timed}");
            assert!(
                !ps.take_permit(),
                "lock_free={lock_free} timed={timed}: consumed"
            );
        }
    }
}

/// An interrupt that raises the flag and calls `wake_for_interrupt` --
/// no permit -- ends an untimed and a long timed park, and leaves no
/// permit behind.
#[test]
fn an_unpolled_park_is_ended_by_an_interrupt_wake() {
    for timed in [false, true] {
        let ps = Arc::new(ParkState::new());
        ps.enable_lock_free();
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_parker(&ps, &flag, timed.then_some(Duration::from_secs(30)));
        wait_until_sleeping(&ps);
        flag.store(true, Ordering::SeqCst);
        ps.wake_for_interrupt();
        rx.recv_timeout(BOUND)
            .expect("the interrupt wake ended the park");
        assert!(
            !ps.take_permit(),
            "timed={timed}: an interrupt gives no permit"
        );
    }
}

/// A wake that finds neither the flag nor the permit (a stale
/// `wake_for_interrupt`) does not end the park; the next unpark does.
#[test]
fn a_stale_interrupt_wake_does_not_end_an_unpolled_park() {
    let ps = Arc::new(ParkState::new());
    ps.enable_lock_free();
    let flag = Arc::new(AtomicBool::new(false));
    let rx = spawn_parker(&ps, &flag, None);
    wait_until_sleeping(&ps);
    ps.wake_for_interrupt();
    assert!(
        rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "nothing to wake for: still parked"
    );
    ps.unpark();
    rx.recv_timeout(BOUND).expect("the unpark ended the park");
}

/// A timed park ends on its deadline; a zero timeout, a permit already
/// set or a flag already raised return at once; a timeout past
/// `Instant`'s range is an untimed park.
#[test]
fn an_unpolled_timed_park_keeps_its_deadline_and_its_fast_paths() {
    let flag = Arc::new(AtomicBool::new(false));
    let ps = Arc::new(ParkState::new());
    let rx = spawn_parker(&ps, &flag, Some(Duration::from_millis(30)));
    let waited = rx.recv_timeout(BOUND).expect("the deadline ended the park");
    assert!(
        waited >= Duration::from_millis(29),
        "not early: {:?}",
        waited
    );

    ps.park_interruptible_unpolled(Some(Duration::ZERO), &flag);
    ps.unpark();
    ps.park_interruptible_unpolled(None, &flag);
    assert!(!ps.take_permit(), "the permit fast path consumed it");
    let raised = AtomicBool::new(true);
    ps.park_interruptible_unpolled(None, &raised);
    assert!(
        raised.load(Ordering::SeqCst),
        "a park never clears the flag"
    );

    let rx = spawn_parker(&ps, &flag, Some(Duration::MAX));
    std::thread::sleep(Duration::from_millis(20));
    ps.unpark();
    rx.recv_timeout(BOUND)
        .expect("an unbounded timeout is still woken");
}
