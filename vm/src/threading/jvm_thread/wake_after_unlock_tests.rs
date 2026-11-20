use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const BOUND: Duration = Duration::from_secs(10);

fn spawn_parker(
    ps: &Arc<ParkState>,
    flag: &Arc<AtomicBool>,
    polled: bool,
    timeout: Option<Duration>,
) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    let (ps2, flag2) = (ps.clone(), flag.clone());
    std::thread::spawn(move || {
        if polled {
            ps2.park_interruptible(timeout, &flag2);
        } else {
            ps2.park_interruptible_unpolled(timeout, &flag2);
        }
        let _ = tx.send(());
    });
    rx
}

fn spawn_sleeper(
    ps: &Arc<ParkState>,
    flag: &Arc<AtomicBool>,
    dur: Duration,
) -> mpsc::Receiver<bool> {
    let (tx, rx) = mpsc::channel();
    let (ps2, flag2) = (ps.clone(), flag.clone());
    std::thread::spawn(move || {
        let _ = tx.send(ps2.sleep_unpolled(dur, &flag2));
    });
    rx
}

/// Let a spawned parker reach its wait (a lock-free state counts it a
/// sleeper first).
fn settle(ps: &ParkState, lock_free: bool) {
    if lock_free {
        while ps.sleeper_count() == 0 {
            std::thread::yield_now();
        }
    }
    std::thread::sleep(Duration::from_millis(20));
}

/// The after-unlock unpark ends an untimed and a long timed park, polled
/// and unpolled, on a plain and a lock-free state, and the park consumes
/// the permit.
#[test]
fn an_after_unlock_unpark_ends_every_park_shape() {
    for lock_free in [false, true] {
        for polled in [false, true] {
            for timed in [false, true] {
                let ps = Arc::new(ParkState::new());
                if lock_free {
                    ps.enable_lock_free();
                }
                let flag = Arc::new(AtomicBool::new(false));
                let timeout = timed.then_some(Duration::from_secs(30));
                let rx = spawn_parker(&ps, &flag, polled, timeout);
                settle(&ps, lock_free);
                if lock_free {
                    ps.unpark_lock_free_after_unlock();
                } else {
                    ps.unpark_after_unlock();
                }
                rx.recv_timeout(BOUND).unwrap_or_else(|_| {
                    panic!("lock_free={lock_free} polled={polled} timed={timed}: not woken")
                });
                assert!(
                    !ps.take_permit(),
                    "lock_free={lock_free} polled={polled} timed={timed}: consumed"
                );
            }
        }
    }
}

/// Two unparks before the park leave ONE permit (the second finds it set
/// and wakes nobody): the park takes it at once, the next park waits.
#[test]
fn an_after_unlock_unpark_of_a_set_permit_is_one_permit() {
    for lock_free in [false, true] {
        let ps = Arc::new(ParkState::new());
        if lock_free {
            ps.enable_lock_free();
        }
        let flag = Arc::new(AtomicBool::new(false));
        if lock_free {
            ps.unpark_lock_free_after_unlock();
            ps.unpark_lock_free_after_unlock();
        } else {
            ps.unpark_after_unlock();
            ps.unpark_after_unlock();
        }
        ps.park_interruptible_unpolled(None, &flag);
        let rx = spawn_parker(&ps, &flag, false, None);
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "lock_free={lock_free}: one permit only"
        );
        ps.unpark_after_unlock();
        rx.recv_timeout(BOUND).expect("woken");
    }
}

/// Back-to-back unparks of a parked thread end the park; at most one
/// permit is left once the parker has run.
#[test]
fn two_after_unlock_unparks_end_the_park_once() {
    let ps = Arc::new(ParkState::new());
    let flag = Arc::new(AtomicBool::new(false));
    let rx = spawn_parker(&ps, &flag, false, None);
    settle(&ps, false);
    ps.unpark_after_unlock();
    ps.unpark_after_unlock();
    rx.recv_timeout(BOUND).expect("woken");
    let _ = ps.take_permit();
    assert!(!ps.take_permit(), "a permit is one bit");
}

/// `interrupt_wake_after_unlock` (the flag raised first, as
/// `Thread.interrupt` does) ends an unpolled park and a sleep -- even a
/// sleep with a leftover permit, which the unpark-shaped wake would skip
/// -- and leaves the permit set for the sleeper's next park.
#[test]
fn an_interrupt_wake_ends_a_park_and_a_sleep_with_a_leftover_permit() {
    for lock_free in [false, true] {
        let ps = Arc::new(ParkState::new());
        if lock_free {
            ps.enable_lock_free();
        }
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_parker(&ps, &flag, false, None);
        settle(&ps, lock_free);
        flag.store(true, Ordering::SeqCst);
        ps.interrupt_wake_after_unlock();
        rx.recv_timeout(BOUND)
            .expect("the interrupt wake ended the park");
        let _ = ps.take_permit();

        flag.store(false, Ordering::SeqCst);
        ps.unpark_after_unlock();
        let sleeper = spawn_sleeper(&ps, &flag, Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(20));
        flag.store(true, Ordering::SeqCst);
        ps.interrupt_wake_after_unlock();
        assert!(
            sleeper
                .recv_timeout(BOUND)
                .expect("the interrupt wake ended the sleep"),
            "lock_free={lock_free}: the sleep saw the flag"
        );
        assert!(
            ps.take_permit(),
            "lock_free={lock_free}: the permit is left"
        );
    }
}

/// `interrupt_wake_locked` (interpreter round i4 wave 91, lane L6,
/// `CRATONVM_INTERRUPT_ONE_WAKE`), the flag raised first: ends an unpolled
/// park and a sleep with a leftover permit, and leaves the permit set, as
/// `interrupt_wake_after_unlock` does.
#[test]
fn i91_l6_a_locked_interrupt_wake_ends_a_park_and_a_sleep() {
    for lock_free in [false, true] {
        let ps = Arc::new(ParkState::new());
        if lock_free {
            ps.enable_lock_free();
        }
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_parker(&ps, &flag, false, None);
        settle(&ps, lock_free);
        flag.store(true, Ordering::SeqCst);
        ps.interrupt_wake_locked();
        rx.recv_timeout(BOUND)
            .expect("the locked interrupt wake ended the park");
        let _ = ps.take_permit();

        flag.store(false, Ordering::SeqCst);
        ps.unpark_after_unlock();
        let sleeper = spawn_sleeper(&ps, &flag, Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(20));
        flag.store(true, Ordering::SeqCst);
        ps.interrupt_wake_locked();
        assert!(
            sleeper
                .recv_timeout(BOUND)
                .expect("the locked interrupt wake ended the sleep"),
            "lock_free={lock_free}: the sleep saw the flag"
        );
        assert!(
            ps.take_permit(),
            "lock_free={lock_free}: the permit is left"
        );
    }
}

/// `wake_for_interrupt_after_unlock` gives no permit: with no flag raised
/// the park waits on; with the flag it ends.
#[test]
fn a_flag_wake_after_unlock_gives_no_permit() {
    let ps = Arc::new(ParkState::new());
    ps.enable_lock_free();
    let flag = Arc::new(AtomicBool::new(false));
    let rx = spawn_parker(&ps, &flag, false, None);
    settle(&ps, true);
    ps.wake_for_interrupt_after_unlock();
    assert!(
        rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "a stale wake does not end the park"
    );
    flag.store(true, Ordering::SeqCst);
    ps.wake_for_interrupt_after_unlock();
    rx.recv_timeout(BOUND).expect("woken by the flag");
    assert!(!ps.take_permit(), "no permit given");
}
