use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

const BOUND: Duration = Duration::from_secs(10);

fn spawn_event_parker(ps: &Arc<ParkState>, flag: &Arc<AtomicBool>) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    let (ps2, flag2) = (ps.clone(), flag.clone());
    std::thread::spawn(move || {
        ps2.park_wait_with(None, &flag2, false, true);
        let _ = tx.send(());
    });
    rx
}

/// Let the parker reach its wait (the event waiter flag goes up under
/// the mutex, right before the wait).
fn wait_until_on_event(ps: &ParkState) {
    let t0 = std::time::Instant::now();
    while !ps.event_waiter.load(Ordering::Relaxed) && t0.elapsed() < BOUND {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(5));
}

#[test]
fn every_unpark_shape_ends_an_untimed_event_wait() {
    for lock_free in [false, true] {
        for wake_all in [false, true] {
            for after_unlock in [false, true] {
                let shape = UnparkShape {
                    lock_free,
                    wake_all,
                    after_unlock,
                };
                let ps = Arc::new(ParkState::new());
                if lock_free {
                    ps.enable_lock_free();
                }
                let flag = Arc::new(AtomicBool::new(false));
                let rx = spawn_event_parker(&ps, &flag);
                wait_until_on_event(&ps);
                ps.unpark_with(shape);
                rx.recv_timeout(BOUND)
                    .unwrap_or_else(|_| panic!("{shape:?}: the unpark did not end the wait"));
                assert!(!ps.take_permit(), "{shape:?}: the wait consumed the permit");
                assert!(
                    !ps.wake_event_left_signalled(),
                    "{shape:?}: no signal left on the event"
                );
            }
        }
    }
}

#[test]
fn interrupt_wakes_end_an_untimed_event_wait_and_a_stale_one_does_not() {
    for after_unlock in [false, true] {
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_event_parker(&ps, &flag);
        wait_until_on_event(&ps);
        ps.wake_for_interrupt_with(after_unlock);
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "after_unlock={after_unlock}: a stale wake does not end the wait"
        );
        wait_until_on_event(&ps);
        flag.store(true, Ordering::SeqCst);
        ps.wake_for_interrupt_with(after_unlock);
        rx.recv_timeout(BOUND).expect("woken by the flag");
        assert!(!ps.take_permit(), "after_unlock={after_unlock}: no permit given");
        assert!(!ps.wake_event_left_signalled());
    }
    let ps = Arc::new(ParkState::new());
    let flag = Arc::new(AtomicBool::new(false));
    let rx = spawn_event_parker(&ps, &flag);
    wait_until_on_event(&ps);
    flag.store(true, Ordering::SeqCst);
    ps.interrupt_wake_locked();
    rx.recv_timeout(BOUND).expect("woken by the one-trip interrupt wake");
}

/// A permit already set returns at once, without touching the event.
#[test]
fn a_set_permit_skips_the_event_wait() {
    let ps = ParkState::new();
    let flag = AtomicBool::new(false);
    ps.unpark();
    ps.park_wait_with(None, &flag, false, true);
    assert!(!ps.take_permit());
    assert!(!ps.wake_event_left_signalled());
}

/// Interpreter round i5 wave 108, lane L6
/// (`CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK`): a timed coarse wait whose
/// waker raises the event after its unlock is ended by every unpark shape
/// and by the one-trip interrupt wake, consumes the permit, and leaves no
/// signal behind; a wait that times out leaves none either.
#[test]
fn a_deferred_signal_ends_a_timed_wait_and_leaves_nothing() {
    for after_unlock in [false, true] {
        let shape = UnparkShape {
            after_unlock,
            ..UnparkShape::LOCKED
        };
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            ps2.park_wait_once_with(Duration::from_secs(30), &flag2, true);
            let _ = tx.send(());
        });
        wait_until_on_event(&ps);
        ps.unpark_with(shape);
        rx.recv_timeout(BOUND)
            .unwrap_or_else(|_| panic!("{shape:?}: the unpark did not end the wait"));
        assert!(!ps.take_permit(), "{shape:?}: the wait consumed the permit");
        assert!(!ps.wake_event_left_signalled(), "{shape:?}: no signal left");
    }
    let ps = Arc::new(ParkState::new());
    let flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let (ps2, flag2) = (ps.clone(), flag.clone());
    std::thread::spawn(move || {
        ps2.park_wait_once_with(Duration::from_secs(30), &flag2, true);
        let _ = tx.send(());
    });
    wait_until_on_event(&ps);
    flag.store(true, Ordering::SeqCst);
    ps.interrupt_wake_locked();
    rx.recv_timeout(BOUND).expect("woken by the one-trip interrupt wake");
    assert!(!ps.wake_event_left_signalled());
    let ps = ParkState::new();
    let flag = AtomicBool::new(false);
    ps.park_wait_once_with(Duration::from_millis(2), &flag, true);
    assert!(!ps.wake_event_left_signalled());
}
