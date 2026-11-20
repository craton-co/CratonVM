use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const BOUND: Duration = Duration::from_secs(10);

fn shapes() -> Vec<UnparkShape> {
    let mut all = Vec::new();
    for lock_free in [false, true] {
        for wake_all in [false, true] {
            for after_unlock in [false, true] {
                all.push(UnparkShape {
                    lock_free,
                    wake_all,
                    after_unlock,
                });
            }
        }
    }
    all
}

fn spawn_waiter(
    ps: &Arc<ParkState>,
    flag: &Arc<AtomicBool>,
    polled: bool,
    timeout: Option<Duration>,
) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    let (ps2, flag2) = (ps.clone(), flag.clone());
    std::thread::spawn(move || {
        ps2.park_wait(timeout, &flag2, polled);
        let _ = tx.send(());
    });
    rx
}

fn settle(ps: &ParkState, lock_free_state: bool) {
    if lock_free_state {
        while ps.sleeper_count() == 0 {
            std::thread::yield_now();
        }
    }
    std::thread::sleep(Duration::from_millis(20));
}

/// Every unpark shape ends every wait shape -- polled and unpolled,
/// untimed and long-timed, on a plain and a lock-free state -- and the
/// wait consumes the permit.
#[test]
fn every_unpark_shape_ends_every_wait_shape() {
    for shape in shapes() {
        for lock_free_state in [false, true] {
            for polled in [false, true] {
                for timed in [false, true] {
                    let ps = Arc::new(ParkState::new());
                    if lock_free_state {
                        ps.enable_lock_free();
                    }
                    let flag = Arc::new(AtomicBool::new(false));
                    let timeout = timed.then_some(Duration::from_secs(30));
                    let rx = spawn_waiter(&ps, &flag, polled, timeout);
                    settle(&ps, lock_free_state);
                    ps.unpark_with(shape);
                    rx.recv_timeout(BOUND).unwrap_or_else(|_| {
                        panic!(
                            "{shape:?} lock_free_state={lock_free_state} polled={polled} \
                             timed={timed}: not woken"
                        )
                    });
                    assert!(
                        !ps.take_permit(),
                        "{shape:?} lock_free_state={lock_free_state} polled={polled}: consumed"
                    );
                }
            }
        }
    }
}

/// An unpark before the wait, in every shape, leaves the permit the
/// wait takes at once; a second one leaves no second permit.
#[test]
fn every_unpark_shape_leaves_one_permit_for_a_later_wait() {
    for shape in shapes() {
        for lock_free_state in [false, true] {
            let ps = Arc::new(ParkState::new());
            if lock_free_state {
                ps.enable_lock_free();
            }
            let flag = Arc::new(AtomicBool::new(false));
            ps.unpark_with(shape);
            ps.unpark_with(shape);
            ps.park_wait(None, &flag, false);
            assert!(
                !ps.take_permit(),
                "{shape:?} lock_free_state={lock_free_state}: a permit is one bit"
            );
        }
    }
}

/// `interrupt_unpark` after the flag is raised ends a wait in every
/// shape and leaves the permit set for a park that has not begun yet.
#[test]
fn every_interrupt_unpark_shape_ends_a_wait_and_leaves_the_permit() {
    for shape in shapes() {
        for polled in [false, true] {
            let ps = Arc::new(ParkState::new());
            let flag = Arc::new(AtomicBool::new(false));
            let rx = spawn_waiter(&ps, &flag, polled, None);
            settle(&ps, false);
            flag.store(true, Ordering::SeqCst);
            ps.interrupt_unpark(shape);
            rx.recv_timeout(BOUND).unwrap_or_else(|_| {
                panic!("{shape:?} polled={polled}: the interrupt did not end the wait")
            });

            let fresh = ParkState::new();
            fresh.interrupt_unpark(shape);
            assert!(
                fresh.take_permit(),
                "{shape:?}: the interrupt's unpark sets the permit"
            );
        }
    }
}

/// The flag-only wake, both bodies: no permit; a raised flag ends the
/// wait, a stale wake does not.
#[test]
fn the_flag_wake_gives_no_permit_either_way() {
    for after_unlock in [false, true] {
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_waiter(&ps, &flag, false, None);
        settle(&ps, false);
        ps.wake_for_interrupt_with(after_unlock);
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "after_unlock={after_unlock}: a stale wake does not end the wait"
        );
        flag.store(true, Ordering::SeqCst);
        ps.wake_for_interrupt_with(after_unlock);
        rx.recv_timeout(BOUND).expect("woken by the flag");
        assert!(
            !ps.take_permit(),
            "after_unlock={after_unlock}: no permit given"
        );
    }
}

/// The wait's own exits, polled and not: a zero timeout and a raised flag
/// return at once without consuming a permit that is not there; a short
/// timed wait returns after (about) its deadline.
#[test]
fn park_wait_returns_on_zero_timeout_flag_and_deadline() {
    for polled in [false, true] {
        let ps = ParkState::new();
        let down = AtomicBool::new(false);
        let up = AtomicBool::new(true);
        let t0 = std::time::Instant::now();
        ps.park_wait(Some(Duration::ZERO), &down, polled);
        ps.park_wait(None, &up, polled);
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "polled={polled}: returned at once"
        );
        let t1 = std::time::Instant::now();
        ps.park_wait(Some(Duration::from_millis(12)), &down, polled);
        assert!(
            t1.elapsed() >= Duration::from_millis(10),
            "polled={polled}: waited out the deadline"
        );
        assert!(!ps.take_permit(), "polled={polled}: no permit appeared");
    }
}
