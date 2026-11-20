use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const BOUND: Duration = Duration::from_secs(10);

/// Sleep on `ps` for `dur` on a new thread; the receiver gets whether
/// the sleep saw the flag and how long it waited.
fn spawn_sleeper(
    ps: &Arc<ParkState>,
    flag: &Arc<AtomicBool>,
    dur: Duration,
) -> mpsc::Receiver<(bool, Duration)> {
    let (tx, rx) = mpsc::channel();
    let (ps2, flag2) = (ps.clone(), flag.clone());
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let interrupted = ps2.sleep_unpolled(dur, &flag2);
        let _ = tx.send((interrupted, t0.elapsed()));
    });
    rx
}

/// A sleep with nothing to wake it lasts its whole duration (not early)
/// and reports no interrupt; a flag already raised returns at once.
#[test]
fn an_unpolled_sleep_keeps_its_duration_and_its_flag_fast_path() {
    let ps = Arc::new(ParkState::new());
    let flag = Arc::new(AtomicBool::new(false));
    let rx = spawn_sleeper(&ps, &flag, Duration::from_millis(30));
    let (interrupted, waited) = rx.recv_timeout(BOUND).expect("the sleep ended");
    assert!(!interrupted);
    assert!(waited >= Duration::from_millis(29), "not early: {waited:?}");

    let raised = AtomicBool::new(true);
    assert!(ps.sleep_unpolled(Duration::from_secs(30), &raised));
    assert!(
        raised.load(Ordering::SeqCst),
        "a sleep's wait never clears the flag"
    );
}

/// An interrupt (the flag, then `wake_for_interrupt`, as
/// `ThreadRegistry::set_interrupted` does) ends a long sleep at once, on
/// a plain state and on a lock-free one, and leaves no permit.
#[test]
fn an_unpolled_sleep_is_ended_by_an_interrupt_wake() {
    for lock_free in [false, true] {
        let ps = Arc::new(ParkState::new());
        if lock_free {
            ps.enable_lock_free();
        }
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_sleeper(&ps, &flag, Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(20));
        flag.store(true, Ordering::SeqCst);
        ps.wake_for_interrupt();
        let (interrupted, waited) = rx
            .recv_timeout(BOUND)
            .expect("the interrupt wake ended the sleep");
        assert!(interrupted, "lock_free={lock_free}");
        assert!(waited < BOUND, "lock_free={lock_free}");
        assert!(
            !ps.take_permit(),
            "lock_free={lock_free}: an interrupt gives no permit"
        );
    }
}

/// An `unpark` does not end a sleep, and its permit is left for the
/// thread's next park (HotSpot's separate `_SleepEvent`).
#[test]
fn an_unpark_neither_ends_an_unpolled_sleep_nor_is_consumed_by_it() {
    let ps = Arc::new(ParkState::new());
    let flag = Arc::new(AtomicBool::new(false));
    let rx = spawn_sleeper(&ps, &flag, Duration::from_millis(300));
    std::thread::sleep(Duration::from_millis(20));
    ps.unpark();
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "an unpark does not end a sleep"
    );
    let (interrupted, waited) = rx.recv_timeout(BOUND).expect("the sleep ended");
    assert!(!interrupted);
    assert!(
        waited >= Duration::from_millis(299),
        "slept out: {waited:?}"
    );
    assert!(
        ps.take_permit(),
        "the permit is still there for the next park"
    );
}

/// A duration past `Instant`'s range sleeps until interrupted.
#[test]
fn an_unbounded_unpolled_sleep_is_still_interruptible() {
    let ps = Arc::new(ParkState::new());
    let flag = Arc::new(AtomicBool::new(false));
    let rx = spawn_sleeper(&ps, &flag, Duration::MAX);
    std::thread::sleep(Duration::from_millis(20));
    flag.store(true, Ordering::SeqCst);
    ps.wake_for_interrupt();
    let (interrupted, _) = rx.recv_timeout(BOUND).expect("woken");
    assert!(interrupted);
}

/// `unpark_waking_all` (and its lock-free twin) ends a park even when
/// another thread waits on the same condvar (here a sleep, which ignores
/// the permit): every waiter is woken, the parker takes the permit, the
/// sleeper waits on.
#[test]
fn unpark_waking_all_reaches_the_parker_beside_another_waiter() {
    for lock_free in [false, true] {
        let ps = Arc::new(ParkState::new());
        if lock_free {
            ps.enable_lock_free();
        }
        let flag = Arc::new(AtomicBool::new(false));
        let sleeper = spawn_sleeper(&ps, &flag, Duration::from_secs(60));
        let (tx, parked) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            ps2.park_interruptible_unpolled(None, &flag2);
            let _ = tx.send(());
        });
        if lock_free {
            while ps.sleeper_count() == 0 {
                std::thread::yield_now();
            }
        }
        std::thread::sleep(Duration::from_millis(20));
        if lock_free {
            ps.unpark_lock_free_waking_all();
        } else {
            ps.unpark_waking_all();
        }
        parked.recv_timeout(BOUND).expect("the park ended");
        assert!(
            !ps.take_permit(),
            "lock_free={lock_free}: the park consumed the permit"
        );
        assert!(
            sleeper.recv_timeout(Duration::from_millis(50)).is_err(),
            "lock_free={lock_free}: the sleep waits on"
        );
        flag.store(true, Ordering::SeqCst);
        ps.wake_for_interrupt();
        let (interrupted, _) = sleeper.recv_timeout(BOUND).expect("woken");
        assert!(interrupted);
    }
}
